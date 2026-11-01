//! DLNA TVs found and driven by the app itself (UPnP AVTransport), for networks where the
//! streaming server's own search doesn't reach them: it asks once, at its start, and along the
//! default route, which a VPN (an exit node) takes off the LAN. Here every LAN interface asks,
//! and the TV plays the server's stream at this machine's address on that LAN.
//!
//! TVs play the video they're handed as is, but many play only some audio (LGs no DTS, few
//! TrueHD). A stream whose audio the TV can't play goes through `Remux`: Stremio's ffmpeg copies
//! the video untouched and converts only the audio, served to the TV from here.

use super::*;
use crate::ui::s_;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

const SSDP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AVT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
/// How long TVs get to answer a search.
const LISTEN: Duration = Duration::from_millis(2500);

/// This machine's LAN addresses (private IPv4, as the streaming server lists them).
fn lan_addresses() -> Vec<Ipv4Addr> {
    let list = get(&format!("{SERVER}/network-info")).ok();
    list.as_ref().and_then(|v| v["availableInterfaces"].as_array()).into_iter().flatten()
        .filter_map(|a| a.as_str()?.parse::<Ipv4Addr>().ok()).filter(|a| a.is_private()).collect()
}

/// Ask every LAN for media renderers; each that answers, as a device the TV list shows:
/// `{ id, name, type: "dlna", control, via }` (its AVTransport control URL, and the address
/// it reaches this machine at).
pub fn discover() -> Vec<Value> {
    let found: Vec<Vec<(String, Ipv4Addr)>> = std::thread::scope(|sc| {
        let asks: Vec<_> = lan_addresses().into_iter().map(|ip| sc.spawn(move || search(ip).unwrap_or_default())).collect();
        asks.into_iter().filter_map(|t| t.join().ok()).collect()
    });
    let mut devices: Vec<Value> = vec![];
    for (location, via) in found.into_iter().flatten() {
        let Some(d) = describe(&location, via) else { continue };
        if !devices.iter().any(|o| o["id"] == d["id"]) { devices.push(d) }
    }
    devices
}

/// One interface's search: the description URLs that answered.
fn search(ip: Ipv4Addr) -> std::io::Result<Vec<(String, Ipv4Addr)>> {
    let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    sock.set_multicast_if_v4(&ip)?;
    sock.set_multicast_ttl_v4(2)?;
    sock.bind(&SocketAddrV4::new(ip, 0).into())?;
    let sock: UdpSocket = sock.into();
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let ask = format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {RENDERER}\r\n\r\n");
    for _ in 0..2 { sock.send_to(ask.as_bytes(), SSDP)?; }
    let (mut out, start, mut buf) = (vec![], Instant::now(), [0u8; 4096]);
    while start.elapsed() < LISTEN {
        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
        let reply = String::from_utf8_lossy(&buf[..n]);
        let location = reply.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("location")).map(|(_, v)| v.trim().to_string()));
        if let Some(l) = location.filter(|l| !out.iter().any(|(o, _)| o == l)) { out.push((l, ip)) }
    }
    Ok(out)
}

/// A renderer from its description: name, id, and where its AVTransport is controlled.
fn describe(location: &str, via: Ipv4Addr) -> Option<Value> {
    let xml = ureq::get(location).call().ok()?.body_mut().read_to_string().ok()?;
    let service = xml.split("<service>").skip(1).find(|s| s.contains(AVT))?;
    let control = absolute(location, &tag(service, "controlURL")?);
    let udn = tag(&xml, "UDN").unwrap_or_else(|| location.to_string());
    Some(json!({ "id": format!("dlna:{}", udn.trim_start_matches("uuid:")), "name": tag(&xml, "friendlyName").unwrap_or_else(|| "TV".into()),
        "type": "dlna", "control": control, "via": via.to_string() }))
}

/// The text of the first `<name>` (any namespace prefix), entities left as they are.
fn tag(xml: &str, name: &str) -> Option<String> {
    let mut at = 0;
    while let Some(i) = xml[at..].find(&format!("{name}>")) {
        let start = at + i;
        let open = xml[..start].ends_with('<') || xml[..start].rsplit('<').next().is_some_and(|p| p.ends_with(':') && !p.starts_with('/'));
        if open {
            let body = &xml[start + name.len() + 1..];
            return Some(body[..body.find("</")?].trim().to_string());
        }
        at = start + name.len() + 1;
    }
    None
}

/// `path` against the description's address (UPnP control URLs are usually relative).
fn absolute(location: &str, path: &str) -> String {
    if path.starts_with("http") { return path.to_string() }
    let origin = location.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    format!("{origin}/{}", path.trim_start_matches('/'))
}

/// An AVTransport action; its reply body.
fn soap(control: &str, action: &str, args: &str) -> Result<String, String> {
    let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{AVT}"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>"#);
    ureq::post(control).header("Content-Type", "text/xml; charset=\"utf-8\"").header("SOAPAction", &format!("\"{AVT}#{action}\""))
        .send(body.as_bytes()).map_err(|e| format!("TV: {e}"))?.body_mut().read_to_string().map_err(|e| format!("TV: {e}"))
}

fn escape(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

/// What the TV fetches: the server's own streams at this machine's LAN address; anything else
/// (an HTTPS debrid link, which TVs' players often can't open) through the server's `/proxy`,
/// which passes ranges on, so seeking still works.
pub fn reachable(src: &str, via: &str) -> String {
    for local in ["http://127.0.0.1:", "http://localhost:"] {
        if let Some(rest) = src.strip_prefix(local) { return format!("http://{via}:{rest}") }
    }
    let Some((scheme, rest)) = src.split_once("://") else { return src.to_string() };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let origin: String = format!("{scheme}://{host}").bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    format!("http://{via}:11470/proxy/d={origin}/{path}")
}

/// `1:02:03` for UPnP's REL_TIME.
fn hms(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

/// Seconds from UPnP's `H:MM:SS(.fff)`; none for `NOT_IMPLEMENTED` and the like.
fn secs(hms: &str) -> Option<f64> {
    let mut t = 0.0;
    for p in hms.split(':') { t = t * 60.0 + p.parse::<f64>().ok()? }
    Some(t)
}

/// The read-ahead between ffmpeg and the TV, as mpv keeps one when playing here: ffmpeg pulls at
/// full speed while the TV takes it at its pace, so a dip in the download doesn't reach the TV
/// (whose player buffers little of a live stream). Playback starts once `PREBUFFER` is in.
const AHEAD: usize = 300 << 20;
const PREBUFFER: usize = 40 << 20;

/// Audio codecs TVs generally play (ffprobe's names).
const TV_AUDIO: [&str; 5] = ["aac", "ac3", "eac3", "mp3", "mp2"];
/// DLNA flags: byte ranges (seekable), and a live stream (`Remux`, seeked by restarting).
const RANGES: &str = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
const LIVE: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=1;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// One of Stremio's bundled tools.
fn tool(name: &str) -> std::path::PathBuf {
    server::stremio_dir().join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() })
}

/// A stream's audio codecs (in order) and length in seconds, read by ffprobe.
fn probe(src: &str) -> Option<(Vec<String>, f64)> {
    let out = Command::new(tool("ffprobe")).args(["-v", "error", "-show_entries", "stream=codec_type,codec_name:format=duration", "-of", "json", src])
        .stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let audio = v["streams"].as_array()?.iter().filter(|st| st["codec_type"] == "audio").map(|st| s_(&st["codec_name"]).to_string()).collect();
    Some((audio, v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0)))
}

/// The stream being converted for the TV: a listener on the LAN, each request a fresh ffmpeg
/// from `offset` (the TV's position counts from there).
struct Remux {
    url: String,
    offset: f64,
    duration: f64,
    ffmpeg: Arc<Mutex<Option<Child>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

static REMUX: Mutex<Option<Remux>> = Mutex::new(None);

impl Remux {
    fn start(src: &str, via: &str, duration: f64) -> std::io::Result<Remux> {
        let listener = TcpListener::bind((via, 0))?;
        let url = format!("http://{via}:{}/cast.mkv", listener.local_addr()?.port());
        let (ffmpeg, stop) = (Arc::new(Mutex::new(None::<Child>)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let (src_, ff, st) = (src.to_string(), ffmpeg.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if st.load(std::sync::atomic::Ordering::Relaxed) { break }
                let Ok(conn) = conn else { continue };
                let (src, ff) = (src_.clone(), ff.clone());
                std::thread::spawn(move || { let _ = serve(conn, &src, &ff); });
            }
        });
        Ok(Remux { url, offset: 0.0, duration, ffmpeg, stop })
    }

    /// The address the TV plays from `at` seconds.
    fn at(&mut self, at: f64) -> String {
        self.offset = at;
        format!("{}?t={}", self.url, at.max(0.0) as u64)
    }
}

impl Drop for Remux {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut c) = self.ffmpeg.lock().unwrap().take() { let _ = c.kill(); }
        // wake the listener so its thread sees `stop`
        if let Some(addr) = self.url.strip_prefix("http://").and_then(|r| r.split('/').next()) { let _ = TcpStream::connect(addr); }
    }
}

/// One request from the TV: headers, and for a GET the converted stream from `?t=`, replacing
/// any ffmpeg still running for an earlier request.
fn serve(mut conn: TcpStream, src: &str, ffmpeg: &Mutex<Option<Child>>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
        if conn.read(&mut byte)? == 0 { return Ok(()) }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let mut first = head.lines().next().unwrap_or("").split_whitespace();
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let at = path.split_once("t=").and_then(|(_, t)| t.split('&').next()?.parse::<u64>().ok()).unwrap_or(0);
    conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: video/x-matroska\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {LIVE}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n").as_bytes())?;
    if method != "GET" { return Ok(()) }
    let mut child = Command::new(tool("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-ss", &at.to_string(), "-i", src, "-map", "0:v:0", "-map", "0:a:0?",
            "-c:v", "copy", "-c:a", "ac3", "-b:a", "640k", "-sn", "-avoid_negative_ts", "make_zero", "-f", "matroska", "pipe:1"])
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let (mut out, pid) = (child.stdout.take().expect("piped"), child.id());
    if let Some(mut old) = ffmpeg.lock().unwrap().replace(child) { let _ = old.kill(); }
    // This request's ffmpeg only: a later request (a seek) has its own.
    let done = || {
        let mut slot = ffmpeg.lock().unwrap();
        if slot.as_ref().is_some_and(|c| c.id() == pid) && let Some(mut c) = slot.take() { let _ = c.kill(); let _ = c.wait(); }
    };
    // the TV hung up (a seek, stop) or the stream ended: this ffmpeg is done
    let r = buffered(&mut out, &mut conn, done);
    done();
    r
}

/// Copy `from` to `to` through up to `AHEAD` of read-ahead, starting once `PREBUFFER` is in.
/// `gone` ends the source when `to` fails (so a read waiting on the network returns).
fn buffered(from: &mut (impl Read + Send), to: &mut impl Write, gone: impl Fn()) -> std::io::Result<()> {
    #[derive(Default)]
    struct Ahead { chunks: std::collections::VecDeque<Vec<u8>>, bytes: usize, ended: bool, gone: bool }
    let (ahead, cv) = (&Mutex::new(Ahead::default()), &std::sync::Condvar::new());
    std::thread::scope(|sc| {
        sc.spawn(move || {
            loop {
                let mut chunk = vec![0u8; 1 << 20];
                let n = from.read(&mut chunk).unwrap_or(0);
                let mut a = ahead.lock().unwrap();
                if n == 0 || a.gone { a.ended = true; cv.notify_all(); return }
                chunk.truncate(n);
                a.bytes += n;
                a.chunks.push_back(chunk);
                cv.notify_all();
                while a.bytes >= AHEAD && !a.gone { a = cv.wait(a).unwrap() }
            }
        });
        let mut started = false;
        let r = loop {
            let chunk = {
                let mut a = ahead.lock().unwrap();
                while !a.ended && (a.chunks.is_empty() || !started && a.bytes < PREBUFFER) { a = cv.wait(a).unwrap() }
                started = true;
                let Some(c) = a.chunks.pop_front() else { break Ok(()) };
                a.bytes -= c.len();
                cv.notify_all();
                c
            };
            if let Err(e) = to.write_all(&chunk) { break Err(e) }
        };
        // the TV is gone or all is sent: stop the reader
        {
            let mut a = ahead.lock().unwrap();
            a.gone = true;
            cv.notify_all();
        }
        if r.is_err() { gone() }
        r
    })
}

/// Hand the TV `url` (`info`: its DLNA protocol info), then play.
fn load(control: &str, url: &str, info: &str, title: &str) -> Result<(), String> {
    let didl = format!(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="0" parentID="-1" restricted="1"><dc:title>{}</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="{info}">{}</res></item></DIDL-Lite>"#, escape(title), escape(url));
    let _ = soap(control, "Stop", "");
    soap(control, "SetAVTransportURI", &format!("<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>", escape(url), escape(&didl)))?;
    soap(control, "Play", "<Speed>1</Speed>").map(|_| ())
}

/// Play `src` on the TV from `from` seconds: as is when the TV plays its audio (seeking once it
/// plays), else converted (`Remux`) from there.
pub fn play(device: &Value, src: &str, title: &str, from: f64) -> Result<(), String> {
    let (control, via) = (s_(&device["control"]), s_(&device["via"]));
    *REMUX.lock().unwrap() = None;
    let (audio, duration) = probe(src).unwrap_or_default();
    if audio.first().is_some_and(|a| !TV_AUDIO.contains(&a.as_str())) {
        let mut r = Remux::start(src, via, duration).map_err(|e| format!("Casting: {e}"))?;
        let url = r.at(from);
        *REMUX.lock().unwrap() = Some(r);
        return load(control, &url, &format!("http-get:*:video/x-matroska:{LIVE}"), title);
    }
    let url = reachable(src, via);
    let mime = ureq::head(&url).call().ok().and_then(|r| r.headers().get("content-type")?.to_str().ok().map(String::from))
        .filter(|m| m.starts_with("video/")).unwrap_or_else(|| "video/x-matroska".into());
    load(control, &url, &format!("http-get:*:{mime}:{RANGES}"), title)?;
    if from > 5.0 {
        // TVs take a seek only once playing
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(500));
            let state = soap(control, "GetTransportInfo", "").ok().and_then(|x| tag(&x, "CurrentTransportState")).unwrap_or_default();
            if state == "PLAYING" { return seek(device, from) }
        }
    }
    Ok(())
}

pub fn pause(device: &Value, paused: bool) -> Result<(), String> {
    let control = s_(&device["control"]);
    if paused { soap(control, "Pause", "") } else { soap(control, "Play", "<Speed>1</Speed>") }.map(|_| ())
}

/// Seek the TV; a converted stream starts over from there.
pub fn seek(device: &Value, secs: f64) -> Result<(), String> {
    let control = s_(&device["control"]);
    let restart = REMUX.lock().unwrap().as_mut().map(|r| r.at(secs));
    match restart {
        Some(url) => load(control, &url, &format!("http-get:*:video/x-matroska:{LIVE}"), ""),
        None => soap(control, "Seek", &format!("<Unit>REL_TIME</Unit><Target>{}</Target>", hms(secs))).map(|_| ()),
    }
}

pub fn stop(device: &Value) {
    *REMUX.lock().unwrap() = None;
    let _ = soap(s_(&device["control"]), "Stop", "");
}

/// The TV's position as the server's casting status reads (`time`/`length` in ms, `paused`),
/// so `cast::landed` takes either; a converted stream's counts from where it started, and its
/// length is the source's.
pub fn status(device: &Value) -> Result<Value, String> {
    let control = s_(&device["control"]);
    let pos = soap(control, "GetPositionInfo", "")?;
    let state = soap(control, "GetTransportInfo", "").ok().and_then(|x| tag(&x, "CurrentTransportState")).unwrap_or_default();
    let ms = |k: &str| tag(&pos, k).and_then(|v| secs(&v)).map(|s| (s * 1000.0) as u64);
    let (time, length) = match REMUX.lock().unwrap().as_ref() {
        Some(r) => (ms("RelTime").map(|t| t + (r.offset * 1000.0) as u64), Some((r.duration * 1000.0) as u64).filter(|&d| d > 0)),
        None => (ms("RelTime"), ms("TrackDuration")),
    };
    Ok(json!({ "time": time, "length": length, "paused": state == "PAUSED_PLAYBACK" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "asks the LAN for TVs; needs the streaming server for the LAN addresses"]
    fn finds_tvs_on_the_lan() {
        for ip in lan_addresses() {
            let r = search(ip);
            println!("{ip}: {r:?}");
            for (l, via) in r.unwrap_or_default() { println!("  describe {l}: {:?}", describe(&l, via)) }
        }
        let found = discover();
        println!("{}", serde_json::to_string_pretty(&found).unwrap());
        assert!(!found.is_empty(), "no TV answered on {:?}", lan_addresses());
        println!("status: {:?}", status(&found[0]));
    }

    #[test]
    #[ignore = "plays a sample video on the first TV found"]
    fn plays_a_sample_on_the_tv() {
        let tv = discover().into_iter().next().expect("a TV");
        let src = std::env::var("TV_TEST_SRC").unwrap_or("https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/720/Big_Buck_Bunny_720_10s_5MB.mp4".into());
        let r = play(&tv, &src, "Big Buck Bunny", 0.0);
        println!("play: {r:?}");
        let secs: u64 = std::env::var("TV_TEST_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
        for _ in 0..secs.div_ceil(5) {
            std::thread::sleep(Duration::from_secs(5));
            println!("status: {:?}", status(&tv));
        }
    }

    #[test]
    fn read_ahead_passes_everything_on_in_order() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let (mut out, mut src) = (Vec::new(), std::io::Cursor::new(data.clone()));
        buffered(&mut src, &mut out, || {}).unwrap();
        assert_eq!(out, data, "short of PREBUFFER: all of it once the source ends");
    }

    #[test]
    fn upnp_bits() {
        assert_eq!(hms(3723.9), "1:02:03");
        assert_eq!(secs("1:02:03.500"), Some(3723.5));
        assert_eq!(secs("NOT_IMPLEMENTED"), None);
        assert_eq!(reachable("http://127.0.0.1:11470/abc/0", "192.168.1.192"), "http://192.168.1.192:11470/abc/0");
        assert_eq!(reachable("https://cdn.example/d/AB12/v.mkv?t=1", "192.168.1.192"), "http://192.168.1.192:11470/proxy/d=https%3A%2F%2Fcdn.example/d/AB12/v.mkv?t=1");
        assert_eq!(absolute("http://192.168.1.102:1223/desc.xml", "/AVTransport/control"), "http://192.168.1.102:1223/AVTransport/control");
        assert_eq!(absolute("http://192.168.1.102:1223/", "upnp/control/AVTransport1"), "http://192.168.1.102:1223/upnp/control/AVTransport1");
        let xml = "<s:Body><u:GetPositionInfoResponse><Track>1</Track><TrackDuration>1:30:00</TrackDuration><RelTime>0:20:34</RelTime></u:GetPositionInfoResponse>";
        assert_eq!(tag(xml, "RelTime").as_deref(), Some("0:20:34"));
        assert_eq!(tag("<root><device><friendlyName>[LG] webOS TV</friendlyName></device></root>", "friendlyName").as_deref(), Some("[LG] webOS TV"));
        assert_eq!(tag("<a:UDN>uuid:1</a:UDN>", "UDN").as_deref(), Some("uuid:1"));
    }
}
