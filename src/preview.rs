//! Seek-bar previews, YouTube-style: a frame of the playing video at the time under the
//! pointer. A second, hidden mpv core opens the same stream on the first hover (no audio, no
//! cache, hardware decoding where there is some), jumps to the keyframe at or before the time and draws that frame small with the
//! player's own renderer (`video::Snapshot`), so it looks like the video; it lands in the app's
//! images like a poster (`Msg::Image`, under `key`), so the image budget covers it.
//! Only the newest request is worked on: sweeping the bar doesn't queue up seeks. Between
//! requests it makes the ones within `AHEAD` buckets of the last, outward, so nearby hovers find
//! theirs ready; never while the player is waiting for data (`hold`), so it doesn't compete.

use crate::Msg;
use crate::player::{command, sym};
use rune::Waker;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Previews are made every this many seconds of the video.
pub const BUCKET: f64 = 10.0;
/// Size of a preview frame, in pixels: 16:9, as the player's (other shapes letterboxed alike).
const WIDTH: u32 = 320;
const HEIGHT: u32 = 180;
/// Rest between previews made ahead, so they take a share of the bandwidth, not all of it.
const AHEAD_PAUSE: Duration = Duration::from_secs(1);
/// Previews made ahead reach this many buckets either side of the last request: as far as the
/// seek bar looks for a nearby one to show meanwhile (controls.rs).
pub const AHEAD: i64 = 6;

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
    (0..=AHEAD).flat_map(|k| [c + k, c - k]).find(|i| (0..n).contains(i) && !made.contains(&(*i as u64))).map(|i| i as f64 * BUCKET)
}

/// Previews of one stream, made on a thread of their own. Drop it to stop.
pub struct Preview {
    want: Arc<(Mutex<Option<f64>>, Condvar)>,
    quit: Arc<AtomicBool>,
    /// The player is waiting for data: make no previews ahead meanwhile.
    hold: Arc<AtomicBool>,
    /// The last time asked for: the pointer moving within one bucket asks once.
    asked: std::cell::Cell<f64>,
}

impl Preview {
    /// Start opening `url`; previews land in `tx` as `Msg::Image`, then `wake` runs.
    pub fn start(url: String, tx: Sender<Msg>, wake: Option<Waker>) -> Preview {
        let want = Arc::new((Mutex::new(None), Condvar::new()));
        let (quit, hold) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (w, q, h) = (want.clone(), quit.clone(), hold.clone());
        let thread = std::thread::spawn(move || if let Err(e) = run(&url, &w, &q, &h, &tx, wake.as_ref()) { eprintln!("preview: {e}") });
        let mut threads = THREADS.lock().unwrap();
        threads.retain(|t| !t.2.is_finished());
        threads.push((quit.clone(), want.clone(), thread));
        Preview { want, quit, hold, asked: std::cell::Cell::new(-1.0) }
    }

    /// Whether the player is waiting for data (buffering, seeking): previews ahead wait for it.
    pub fn hold(&self, on: bool) {
        if self.hold.swap(on, Ordering::Relaxed) && !on { self.want.1.notify_one() }
    }

    /// Make the preview at `t` (a bucket start) next, instead of any still waiting.
    pub fn want(&self, t: f64) {
        if self.asked.replace(t) == t { return }
        *self.want.0.lock().unwrap() = Some(t);
        self.want.1.notify_one();
    }
}

/// Every preview thread not known to have ended: its quit flag, its wake-up, its handle.
type Thread = (Arc<AtomicBool>, Arc<(Mutex<Option<f64>>, Condvar)>, std::thread::JoinHandle<()>);
static THREADS: Mutex<Vec<Thread>> = Mutex::new(Vec::new());

/// Stop every preview thread and wait for them to tear their mpv down, for up to `limit`: a
/// process exiting under one (its decoder's driver unloading first) can crash.
pub fn shutdown(limit: Duration) {
    let threads = std::mem::take(&mut *THREADS.lock().unwrap());
    for (quit, want, _) in &threads {
        quit.store(true, Ordering::Relaxed);
        want.1.notify_one();
    }
    let until = Instant::now() + limit;
    for (_, _, t) in threads {
        while !t.is_finished() && Instant::now() < until { std::thread::sleep(Duration::from_millis(5)) }
        if t.is_finished() { let _ = t.join(); }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.want.1.notify_one();
    }
}

fn run(url: &str, want: &(Mutex<Option<f64>>, Condvar), quit: &AtomicBool, hold: &AtomicBool, tx: &Sender<Msg>, wake: Option<&Waker>) -> Result<(), String> {
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
        // Hardware decoding: a 4K HEVC keyframe on the CPU takes cores. vd-lavc-skiploopfilter:
        // software decoding (no hardware for the codec) skips deblocking, unseen at this size.
        for (k, v) in [("vo", "libmpv"), ("aid", "no"), ("sid", "no"), ("pause", "yes"), ("keep-open", "always"), ("cache", "no"),
            ("hr-seek", "no"), ("hwdec", "auto-safe"), ("video-latency-hacks", "yes"), ("vd-lavc-skiploopfilter", "all"), ("ytdl", "no"),
            ("load-scripts", "no"), ("osc", "no"), ("input-default-bindings", "no")] {
            let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
            unsafe { set(ctx, k.as_ptr(), v.as_ptr()) };
        }
        if unsafe { init(ctx) } < 0 { return Err("mpv_initialize failed".to_string()) }
        // Drawn by the player's own renderer, so a preview looks like the video (HDR included).
        let snap = RefCell::new(crate::video::Snapshot::new(ctx, WIDTH, HEIGHT)?);
        // Whether `snap` has drawn a frame since this was last cleared.
        let drew = Cell::new(false);
        command(ctx, &["loadfile", url])?;
        let prop = |name: &std::ffi::CStr| unsafe {
            let p = get(ctx, name.as_ptr());
            let v = (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned());
            if !p.is_null() { free(p.cast()) }
            v
        };
        // Wait for event `id` (or, with `id` 0, a frame drawn), drawing mpv's frames meanwhile;
        // false on the stream ending, quitting, `secs` passing or, when `yield_to_request`, a
        // request coming in (the newest request wins over the work at hand).
        let until = |id: c_int, secs: f64, yield_to_request: bool| {
            let end = Instant::now() + Duration::from_secs_f64(secs);
            while !quit.load(Ordering::Relaxed) && !(yield_to_request && want.0.lock().unwrap().is_some()) {
                if snap.borrow_mut().pump() { drew.set(true) }
                if id == 0 && drew.get() { return true }
                let left = end.saturating_duration_since(Instant::now()).as_secs_f64();
                if left <= 0.0 { return false }
                match unsafe { (*wait(ctx, left.min(0.01))).id } {
                    e if e == id => return true,
                    SHUTDOWN | END_FILE => return false,
                    _ => {}
                }
            }
            false
        };
        if !until(FILE_LOADED, 30.0, false) { return Err("the stream didn't open".to_string()) }
        let dur: f64 = prop(c"duration").and_then(|d| d.parse().ok()).unwrap_or(0.0);
        let (mut made, mut center) = (HashSet::new(), None);
        loop {
            // A request first; else, after a rest, the next one ahead of the last request, unless
            // the player is waiting for data.
            let ahead = center.filter(|_| !hold.load(Ordering::Relaxed)).and_then(|c| next_ahead(c, dur, &made));
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
            if command(ctx, &["seek", &t.to_string(), "absolute+keyframes"]).is_err() { made.insert((t / BUCKET) as u64); continue }
            drew.set(false);
            // A seek given up for a newer request may still land first: wait for this one's frame
            // (a keyframe at or before `t`, within a bucket), then for it to be drawn.
            let landed = (0..3).any(|_| until(PLAYBACK_RESTART, 10.0, true)
                && prop(c"time-pos").and_then(|p| p.parse::<f64>().ok()).is_some_and(|p| p > t - BUCKET && p < t + BUCKET));
            if !landed || !until(0, 2.0, true) { continue } // a newer request: this one is asked for again if still wanted
            made.insert((t / BUCKET) as u64);
            let Some(img) = snap.borrow().grab() else { continue };
            if tx.send(Msg::Image(key(url, t), img)).is_err() { break }
            if let Some(w) = wake { w() }
        }
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
        let near: HashSet<u64> = (94..=106).collect();
        assert_eq!(next_ahead(1000.0, 7200.0, &near), None, "only within AHEAD buckets of the last request");
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
        drop(p);
        shutdown(Duration::from_secs(5));
    }
}
