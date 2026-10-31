//! `STREMIO_RUNE_VIDEOTEST=<video>`: a window that is only the video, played through mpv's GPU
//! render path into a Rune `NativeSurface` (video.rs, video_mac.rs). After `VIDEOTEST_SECS`
//! (default 20) of playback it writes CPU use, mpv's dropped and delayed frames and the
//! renderer's counters to `stremio-rune-videotest.txt` in the temp folder, then quits.

use crate::player::{Mpv, Playback};
use rune::{AppSpec, Cx, El, NativeSurface, SurfaceFit, Tx, Waker, el};
use std::time::{Duration, Instant};

pub struct T {
    clip: String,
    surface: NativeSurface,
    mpv: Option<Mpv>,
    pb: Playback,
    size: (u32, u32),
    started: Option<(Instant, f64, i64, i64)>,
    secs: f64,
    done: bool,
}

/// A diagnostic switch: set and not empty or "0".
fn knob(name: &str) -> bool { std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0") }

static FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
fn cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
}

#[cfg(target_os = "macos")]
fn cpu_seconds() -> f64 {
    #[repr(C)]
    struct TimeVal { sec: i64, usec: i32 }
    #[repr(C)]
    struct RUsage { user: TimeVal, system: TimeVal, rest: [i64; 14] }
    unsafe extern "C" { fn getrusage(who: i32, usage: *mut RUsage) -> i32; }
    let mut u: RUsage = unsafe { std::mem::zeroed() };
    unsafe { getrusage(0 /* RUSAGE_SELF */, &mut u) };
    let t = |v: &TimeVal| v.sec as f64 + v.usec as f64 / 1e6;
    t(&u.user) + t(&u.system)
}

fn log(line: &str) {
    use std::io::Write;
    eprintln!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("stremio-rune-videotest.txt")) {
        let _ = writeln!(f, "{line}");
    }
}

fn setup(t: &mut T, waker: Waker) {
    t.surface.set_waker(waker.clone());
    let surface = Some(t.surface.clone());
    match Mpv::play(&t.clip, "videotest", (!knob("VIDEOTEST_NO_MPV_WAKE")).then_some(waker), None, vec![("hwdec", "auto-safe".into()), ("mute", "yes".into())] /* silent: test clips carry a tone */, surface) {
        Ok(m) => t.mpv = Some(m),
        Err(e) => { log(&format!("play failed: {e}")); t.done = true }
    }
}

fn poll(t: &mut T, _: f64, tx: &mut Tx) -> bool {
    tx.wake_after(Duration::from_millis(if knob("VIDEOTEST_SLOW_POLL") { 1000 } else { 100 }));
    if t.done { tx.quit(); return false }
    let Some(m) = &t.mpv else { return false };
    if let Some(e) = m.pump(&mut t.pb) { log(&format!("ended early: {e:?}")); t.done = true; return false }
    if t.size.0 > 0 { m.set_video_size(t.size.0, t.size.1) }
    // Measure from 3 s into playback, past start-up.
    let count = |n: &str| m.property(n).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    if t.pb.started && t.pb.pos > 3.0 && t.started.is_none() {
        t.started = Some((Instant::now(), cpu_seconds(), count("frame-drop-count"), count("vo-delayed-frame-count")));
        FRAMES.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((at, cpu, drops, delayed)) = t.started && at.elapsed().as_secs_f64() >= t.secs {
        let wall = at.elapsed().as_secs_f64();
        let used = cpu_seconds() - cpu;
        let p = |n: &str| m.property(n).unwrap_or_default();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let mut line = format!("{} | {} | {}x{} {} | {:.1} s: CPU {:.1}% of one core ({:.2}% of {} cores) | dropped {} delayed {} (in the window; since start {}) avsync {} | hwdec {} | fps {}",
            t.clip.rsplit(['\\', '/']).next().unwrap_or(""), "Rune NativeSurface (GPU)",
            p("width"), p("height"), p("video-params/pixelformat"), wall, used / wall * 100.0, used / wall * 100.0 / cores, cores,
            count("frame-drop-count") - drops, count("vo-delayed-frame-count") - delayed, p("frame-drop-count"), p("avsync"), p("hwdec-current"), p("estimated-vf-fps"));
        if let Some(s) = m.video_stats() {
            line += &format!(" | rendered {} at {}x{}, {:.2} ms/frame on the render thread, starved {} | surface draws {:?} error {:?}",
                s.frames, s.size.0, s.size.1, s.render_ms / s.frames.max(1) as f64, s.starved, t.surface.counts(), t.surface.last_error());
        }
        line += &format!(" | Rune frames {:.1}/s", FRAMES.load(std::sync::atomic::Ordering::Relaxed) as f64 / wall);
        log(&line);
        t.done = true;
    }
    false
}

fn view(t: &T, _: &mut Cx<T>) -> El {
    let mut video = el().w_full().h_full().fill(rune::Color::BLACK);
    if knob("VIDEOTEST_NO_LAYOUT") {
        // Fixed render size, for telling apart what keeps the window drawing.
        return video.native_surface(&t.surface, SurfaceFit::Fit);
    }
    // Fires on the first layout and on a resize only. (`on_layout_tx` with `live_only` would
    // keep the window drawing at the display rate.)
    video = video.on_layout(|t: &mut T, r| {
        let s = crate::window::scale();
        t.size = ((r[2] * s).round() as u32, (r[3] * s).round() as u32);
    });
    video.native_surface(&t.surface, SurfaceFit::Fit)
}

/// Run the test window if `STREMIO_RUNE_VIDEOTEST` names a video; true when it ran.
pub fn run() -> bool {
    let Ok(clip) = std::env::var("STREMIO_RUNE_VIDEOTEST") else { return false };
    let secs = std::env::var("VIDEOTEST_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let fixed = knob("VIDEOTEST_NO_LAYOUT").then_some((1920, 1080));
    let init = move || T { clip: clip.clone(), surface: NativeSurface::new(), mpv: None, pb: Playback::default(), size: fixed.unwrap_or((0, 0)), started: None, secs, done: false };
    let app = rune::App::new(AppSpec { setup: Some(setup), poll: Some(poll), ..AppSpec::new("Video test", (1280.0, 720.0), || unreachable!(), view) })
        .on_frame_encoded(|_| { FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed); });
    app.init(init).run();
    true
}
