//! Casting to a TV the way the Stremio app does it, through its streaming server: the server
//! finds Chromecasts and DLNA renderers on the network (`/casting/`) and plays to one through
//! `/casting/<device>/player`, fetching and transcoding the stream itself, so the TV never needs
//! to reach this machine's `127.0.0.1`. The local player pauses while the TV plays, and picks
//! up where the TV was when casting stops. DLNA TVs the server's search misses (`dlna`) are
//! found and driven by the app itself.

use super::*;
use crate::ui::s_;

/// How often the TV's position is asked for while casting.
const POLL: Duration = Duration::from_secs(1);

/// The TV being played to and what it last said.
pub struct Session {
    pub id: String,
    pub name: String,
    /// The device as listed (`dlna`'s carry their own control URL).
    device: Value,
    /// Position and length in seconds, as the TV last reported.
    pub pos: f64,
    pub dur: f64,
    pub paused: bool,
    /// Where to seek once the TV has the video (the local position when casting began).
    seek: Option<f64>,
    asked: Instant,
}

impl Session {
    /// Casting from `pos` of a `dur`-second video: the TV seeks there once it has it.
    pub fn new(device: Value, pos: f64, dur: f64) -> Session {
        let (id, name) = (s_(&device["id"]).to_string(), device["name"].as_str().unwrap_or(s_(&device["id"])).to_string());
        Session { id, name, device, pos, dur, paused: false, seek: (pos > 5.0).then_some(pos), asked: Instant::now() }
    }

    /// Driven by the app (`dlna`) rather than through the streaming server.
    fn direct(&self) -> bool { self.device["control"].is_string() }
}

/// Open or close the TV list; opening looks for TVs again.
pub fn toggle_list(s: &mut State) {
    s.cast_open = !s.cast_open;
    (s.subs_open, s.quality_open) = (false, false);
    if s.cast_open { find(s) }
}

fn find(s: &mut State) {
    if cfg!(test) { return }
    spawn(s, || Ok(Msg::CastDevices(discover())));
}

/// The TVs the app finds itself (`dlna`), then the server's (a TV both found, once).
pub fn discover() -> Vec<Value> {
    let (ours, theirs) = std::thread::scope(|sc| {
        let server = sc.spawn(|| get(&format!("{SERVER}/casting/")).ok().and_then(|v| v.as_array().cloned()).unwrap_or_default());
        (dlna::discover(), server.join().unwrap_or_default())
    });
    let same = |d: &Value| ours.iter().any(|o| o["name"] == d["name"]);
    let rest: Vec<Value> = theirs.into_iter().filter(|d| !same(d)).collect();
    ours.into_iter().chain(rest).collect()
}

/// The devices that are TVs (the server also lists local players such as VLC).
pub fn tvs(devices: &[Value]) -> Vec<&Value> {
    devices.iter().filter(|d| matches!(d["type"].as_str(), Some("chromecast" | "tv" | "dlna"))).collect()
}

/// Start playing the current stream on a TV, from where it is now.
pub fn start(s: &mut State, device: Value) {
    let at = s.playback.pos;
    mpv(s, &["set", "pause", "yes"]);
    let mut session = Session::new(device, at, s.playback.dur);
    // A TV the app drives starts there itself (`dlna::play`); the server's seek once playing.
    let direct = session.direct();
    if direct { session.seek = None }
    s.cast = Some(session);
    s.cast_open = false;
    let src = s.playing.clone();
    if direct { send(s, &[("source", src), ("from", at.to_string())]) } else { send(s, &[("source", src)]) }
}

/// Play/pause on the TV.
pub fn toggle_pause(s: &mut State) {
    let Some(c) = s.cast.as_mut() else { return };
    c.paused = !c.paused;
    let v = if c.paused { "1" } else { "0" };
    send(s, &[("paused", v.into())]);
}

/// Seek the TV to `secs`.
pub fn seek(s: &mut State, secs: f64) {
    let Some(c) = s.cast.as_mut() else { return };
    c.pos = secs;
    send(s, &[("time", ((secs * 1000.0) as u64).to_string())]);
}

/// Stop the TV and carry on here from where it was.
pub fn stop(s: &mut State) {
    let Some(c) = s.cast.take() else { return };
    let (id, device, direct) = (c.id.clone(), c.device.clone(), c.direct());
    if !cfg!(test) { spawn(s, move || { if direct { dlna::stop(&device) } else { let _ = player(&id, &[("stop", "1".into())]); } Ok(Msg::Saved) }) }
    if c.pos > 0.0 {
        s.playback.pos = c.pos;
        mpv(s, &["seek", &c.pos.to_string(), "absolute"]);
    }
}

/// Called every poll while casting: ask the TV where it is, every `POLL`.
pub fn tick(s: &mut State) {
    let Some(c) = s.cast.as_mut().filter(|c| c.asked.elapsed() >= POLL) else { return };
    c.asked = Instant::now();
    send(s, &[]);
}

/// The TV's answer: its position, and the pending seek once it has the video.
pub fn landed(s: &mut State, id: String, status: Value) {
    let Some(c) = s.cast.as_mut().filter(|c| c.id == id) else { return };
    let num = |k: &str| status[k].as_f64().or_else(|| status[k].as_str().and_then(|v| v.parse().ok()));
    if let Some(t) = num("time") { c.pos = t / 1000.0 }
    if let Some(p) = status["paused"].as_bool() { c.paused = p }
    // A length from the TV means it has the video: now the seek can land.
    let Some(len) = num("length").filter(|&l| l > 0.0) else { return };
    c.dur = len / 1000.0;
    if let Some(at) = c.seek.take() { seek(s, at) }
}

fn send(s: &mut State, params: &[(&'static str, String)]) {
    #[cfg(test)]
    tests::SENT.with(|log| log.borrow_mut().push(params.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")));
    let Some(c) = &s.cast else { return };
    if cfg!(test) { return }
    let (id, params) = (c.id.clone(), params.to_vec());
    if c.direct() {
        let (device, title) = (c.device.clone(), s.meta.as_ref().map(|m| s_(&m["name"]).to_string()).unwrap_or_default());
        spawn(s, move || {
            let from = params.iter().find(|(k, _)| *k == "from").and_then(|(_, v)| v.parse().ok()).unwrap_or(0.0);
            for (k, v) in &params {
                match *k {
                    "source" => dlna::play(&device, v, &title, from)?,
                    "paused" => dlna::pause(&device, v == "1")?,
                    "time" => dlna::seek(&device, v.parse::<f64>().unwrap_or(0.0) / 1000.0)?,
                    _ => {}
                }
            }
            Ok(Msg::CastStatus(id, dlna::status(&device)?))
        });
        return;
    }
    spawn(s, move || Ok(Msg::CastStatus(id.clone(), player(&id, &params)?)));
}

fn player(id: &str, params: &[(&str, String)]) -> Result<Value, String> {
    // Device ids are the server's own (hostnames, `uuid:…`); escape anything else for the path.
    let id: String = id.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.:~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    let mut req = ureq::get(format!("{SERVER}/casting/{id}/player"));
    for (k, v) in params { req = req.query(*k, v) }
    req.call().map_err(|e| format!("Casting: {e}"))?.body_mut().read_json().map_err(|e| format!("Casting: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// What was sent to the TV (`send`), as query strings.
        pub static SENT: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    fn sent() -> Vec<String> { SENT.with(|log| std::mem::take(&mut *log.borrow_mut())) }
    fn mpv_sent() -> Vec<String> { crate::tests::MPV.with(|log| std::mem::take(&mut *log.borrow_mut())) }

    #[test]
    fn only_tvs_are_offered() {
        let devices = vec![json!({ "id": "a", "type": "chromecast" }), json!({ "id": "b", "type": "dlna" }), json!({ "id": "vlc", "type": "external" })];
        assert_eq!(tvs(&devices).iter().map(|d| d["id"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn casting_plays_on_the_tv_from_here_and_comes_back_where_the_tv_was() {
        let mut s = state();
        s.playing = "http://127.0.0.1:11470/abc/0".into();
        (s.playback.pos, s.playback.dur) = (600.0, 5400.0);
        (sent(), mpv_sent());
        start(&mut s, json!({ "id": "Living-Room", "name": "Living Room TV", "type": "chromecast" }));
        assert_eq!(mpv_sent(), ["set pause yes"], "the local player waits");
        assert_eq!(sent(), ["source=http://127.0.0.1:11470/abc/0"]);
        // Before the TV has the video, no seek; once it reports a length, to where it was here.
        landed(&mut s, "Living-Room".into(), json!({ "time": 0, "length": 0 }));
        assert!(sent().is_empty());
        landed(&mut s, "Living-Room".into(), json!({ "time": 1000, "length": 5400000, "paused": false }));
        assert_eq!(sent(), ["time=600000"], "seeks once");
        landed(&mut s, "Living-Room".into(), json!({ "time": 612000, "length": 5400000 }));
        assert!(sent().is_empty());
        assert_eq!(s.cast.as_ref().map(|c| (c.pos, c.dur)), Some((612.0, 5400.0)));
        landed(&mut s, "Other".into(), json!({ "time": 1 }));
        assert_eq!(s.cast.as_ref().unwrap().pos, 612.0, "another device's answer is ignored");
        toggle_pause(&mut s);
        assert_eq!(sent(), ["paused=1"]);
        stop(&mut s);
        assert!(s.cast.is_none());
        assert_eq!(mpv_sent(), ["seek 612 absolute"], "picks up where the TV was");
        assert_eq!(s.playback.pos, 612.0);
    }
}
