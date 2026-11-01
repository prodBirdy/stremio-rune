//! Seek-bar previews, YouTube-style: a frame of the playing video at the time under the
//! pointer. A second, hidden mpv core opens the same stream (no audio, no cache, software
//! decoding), jumps to the keyframe at or before the time and saves that frame small; it lands
//! in the app's images like a poster (`Msg::Image`, under `key`), so the image budget covers it.
//! Only the newest request is worked on: sweeping the bar doesn't queue up seeks. Between
//! requests it makes the rest, outward from the last one, so most hovers find theirs ready.

use crate::Msg;
use crate::player::{command, sym};
use rune::Waker;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Previews are made every this many seconds of the video.
const BUCKET: f64 = 10.0;
/// Width of a preview frame, in pixels.
const WIDTH: u32 = 320;
/// Rest between previews made ahead, so they take a share of the bandwidth, not all of it.
// ponytail: a fixed pause, not paced by playback's cache; make it wait while the player buffers if previews ever starve it.
const AHEAD_PAUSE: Duration = Duration::from_secs(1);

// mpv event ids (client.h).
const SHUTDOWN: c_int = 1;
const END_FILE: c_int = 7;
const FILE_LOADED: c_int = 8;
const PLAYBACK_RESTART: c_int = 21;

#[repr(C)]
struct Event { id: c_int, error: c_int, reply: u64, data: *mut c_void }

/// The image key of `url`'s preview at `t` (a bucket start, see `bucket`).
pub fn key(url: &str, t: f64) -> String { format!("{url}#preview={t}") }

/// The start of the bucket `t` falls in: the time a preview is made for.
pub fn bucket(t: f64) -> f64 { (t.max(0.0) / BUCKET).floor() * BUCKET }

/// The next preview to make ahead: the bucket nearest `center` (after, then before) that isn't
/// `made`, within a video of `dur` seconds.
fn next_ahead(center: f64, dur: f64, made: &HashSet<u64>) -> Option<f64> {
    let n = (dur / BUCKET).ceil() as i64;
    let c = (center / BUCKET) as i64;
    (0..=n.max(c)).flat_map(|k| [c + k, c - k]).find(|i| (0..n).contains(i) && !made.contains(&(*i as u64))).map(|i| i as f64 * BUCKET)
}

/// Previews of one stream, made on a thread of their own. Drop it to stop.
pub struct Preview {
    want: Arc<(Mutex<Option<f64>>, Condvar)>,
    quit: Arc<AtomicBool>,
    /// The last time asked for: the pointer moving within one bucket asks once.
    asked: std::cell::Cell<f64>,
}

impl Preview {
    /// Start opening `url`; previews land in `tx` as `Msg::Image`, then `wake` runs.
    pub fn start(url: String, tx: Sender<Msg>, wake: Option<Waker>) -> Preview {
        let want = Arc::new((Mutex::new(None), Condvar::new()));
        let quit = Arc::new(AtomicBool::new(false));
        let (w, q) = (want.clone(), quit.clone());
        std::thread::spawn(move || if let Err(e) = run(&url, &w, &q, &tx, wake.as_ref()) { eprintln!("preview: {e}") });
        Preview { want, quit, asked: std::cell::Cell::new(-1.0) }
    }

    /// Make the preview at `t` (a bucket start) next, instead of any still waiting.
    pub fn want(&self, t: f64) {
        if self.asked.replace(t) == t { return }
        *self.want.0.lock().unwrap() = Some(t);
        self.want.1.notify_one();
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.want.1.notify_one();
    }
}

fn run(url: &str, want: &(Mutex<Option<f64>>, Condvar), quit: &AtomicBool, tx: &Sender<Msg>, wake: Option<&Waker>) -> Result<(), String> {
    let create: unsafe extern "C" fn() -> *mut c_void = sym(b"mpv_create\0")?;
    let set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int = sym(b"mpv_set_option_string\0")?;
    let init: unsafe extern "C" fn(*mut c_void) -> c_int = sym(b"mpv_initialize\0")?;
    let wait: unsafe extern "C" fn(*mut c_void, f64) -> *const Event = sym(b"mpv_wait_event\0")?;
    let destroy: unsafe extern "C" fn(*mut c_void) = sym(b"mpv_terminate_destroy\0")?;
    let get: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char = sym(b"mpv_get_property_string\0")?;
    let free: unsafe extern "C" fn(*mut c_void) = sym(b"mpv_free\0")?;
    let ctx = unsafe { create() };
    if ctx.is_null() { return Err("mpv_create failed".into()) }
    let result = (|| {
        let scale = format!("scale={WIDTH}:-2");
        for (k, v) in [("vo", "null"), ("aid", "no"), ("sid", "no"), ("pause", "yes"), ("keep-open", "always"), ("cache", "no"),
            ("hr-seek", "no"), ("hwdec", "no"), ("video-latency-hacks", "yes"), ("ytdl", "no"), ("load-scripts", "no"), ("osc", "no"), ("input-default-bindings", "no"),
            ("vf", &scale), ("screenshot-format", "jpg")] {
            let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
            unsafe { set(ctx, k.as_ptr(), v.as_ptr()) };
        }
        if unsafe { init(ctx) } < 0 { return Err("mpv_initialize failed".to_string()) }
        command(ctx, &["loadfile", url])?;
        // Wait for event `id`; false on the stream ending, quitting or `secs` passing.
        let until = |id: c_int, secs: f64| {
            let end = Instant::now() + Duration::from_secs_f64(secs);
            while !quit.load(Ordering::Relaxed) {
                let left = end.saturating_duration_since(Instant::now()).as_secs_f64();
                if left <= 0.0 { return false }
                match unsafe { (*wait(ctx, left.min(0.25))).id } {
                    e if e == id => return true,
                    SHUTDOWN | END_FILE => return false,
                    _ => {}
                }
            }
            false
        };
        if !until(FILE_LOADED, 30.0) { return Err("the stream didn't open".to_string()) }
        let dur = unsafe {
            let p = get(ctx, c"duration".as_ptr());
            let d = if p.is_null() { 0.0 } else { CStr::from_ptr(p).to_str().ok().and_then(|d| d.parse().ok()).unwrap_or(0.0) };
            if !p.is_null() { free(p.cast()) }
            d
        };
        let (mut made, mut center) = (HashSet::new(), None);
        let file = std::env::temp_dir().join(format!("stremio-rune-preview-{}.jpg", std::process::id()));
        let file_arg = file.to_string_lossy().into_owned();
        loop {
            // A request first; else, after a rest, the next one ahead of the last request.
            let ahead = center.and_then(|c| next_ahead(c, dur, &made));
            let t = {
                let mut w = want.0.lock().unwrap();
                if w.is_none() && ahead.is_some() { w = want.1.wait_timeout(w, AHEAD_PAUSE).unwrap().0 }
                while w.is_none() && ahead.is_none() && !quit.load(Ordering::Relaxed) { w = want.1.wait(w).unwrap() }
                if quit.load(Ordering::Relaxed) { break }
                match w.take() {
                    Some(t) => { center = Some(t); t }
                    None => match ahead { Some(t) => t, None => continue },
                }
            };
            made.insert((t / BUCKET) as u64);
            if command(ctx, &["seek", &t.to_string(), "absolute+keyframes"]).is_err() || !until(PLAYBACK_RESTART, 10.0) { continue }
            if command(ctx, &["screenshot-to-file", &file_arg, "video"]).is_err() { continue }
            let Ok(img) = image::open(&file) else { continue };
            if tx.send(Msg::Image(key(url, t), img.to_rgba8())).is_err() { break }
            if let Some(w) = wake { w() }
        }
        let _ = std::fs::remove_file(&file);
        Ok(())
    })();
    unsafe { destroy(ctx) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets() {
        assert_eq!(bucket(0.0), 0.0);
        assert_eq!(bucket(19.9), 10.0);
        assert_eq!(bucket(-3.0), 0.0);
    }

    #[test]
    fn makes_ahead_outward_from_the_last_request() {
        let mut made = HashSet::from([3]);
        assert_eq!(next_ahead(30.0, 60.0, &made), Some(40.0), "30 made: after first");
        made.insert(4);
        assert_eq!(next_ahead(30.0, 60.0, &made), Some(20.0), "then before");
        made.extend([0, 1, 2, 5]);
        assert_eq!(next_ahead(30.0, 60.0, &made), None, "all six made");
        assert_eq!(next_ahead(100.0, 60.0, &HashSet::new()), Some(50.0), "a center past the end");
        assert_eq!(next_ahead(0.0, 0.0, &HashSet::new()), None, "no duration: nothing ahead");
    }

    /// TEST_STREAM: a video (file or URL). Previews at 60 s (opening the stream first) and then
    /// 30 min arrive as small images; the last is saved to the temp folder to look at.
    #[test]
    #[ignore]
    fn makes_previews() {
        let url = std::env::var("TEST_STREAM").expect("TEST_STREAM: a video");
        let (tx, rx) = std::sync::mpsc::channel();
        let p = Preview::start(url.clone(), tx, None);
        for at in [60.0, 1800.0] {
            p.want(at);
            let t = Instant::now();
            // Previews made ahead may land first.
            let img = loop {
                match rx.recv_timeout(Duration::from_secs(60)) {
                    Ok(Msg::Image(k, img)) if k == key(&url, at) => break img,
                    Ok(_) => {}
                    Err(_) => panic!("no preview at {at}"),
                }
            };
            assert_eq!(img.width(), WIDTH);
            eprintln!("preview at {at} s: {}x{} in {:.2} s", img.width(), img.height(), t.elapsed().as_secs_f64());
            img.save(std::env::temp_dir().join("stremio-rune-preview-test.png")).unwrap();
        }
    }
}
