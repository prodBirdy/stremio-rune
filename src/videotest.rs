//! `STREMIO_RUNE_VIDEOTEST=<video>`: a window that is only the video, played through mpv's GPU
//! render path into a Rune `NativeSurface` (video.rs). After `VIDEOTEST_SECS` (default 20) of
//! playback it writes CPU use, mpv's dropped and delayed frames and the renderer's counters to
//! `%TEMP%\stremio-rune-videotest.txt`, then quits. `--child` plays the same file the old way, in
//! mpv's own child window, for comparison.

use crate::player::{Mpv, Playback};
use rune::{AppSpec, Cx, El, NativeSurface, SurfaceFit, Tx, Waker, el};
use std::time::{Duration, Instant};

pub struct T {
    clip: String,
    child: bool,
    surface: NativeSurface,
    mpv: Option<Mpv>,
    pb: Playback,
    size: (u32, u32),
    started: Option<(Instant, f64, i64, i64)>,
    secs: f64,
    done: bool,
}

fn cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut a, mut b, mut k, mut u) = (z, z, z, z);
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
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
    let surface = (!t.child).then(|| t.surface.clone());
    match Mpv::play(&t.clip, "videotest", Some(waker), None, vec![("hwdec", "auto-safe".into()), ("mute", "yes".into())] /* silent: test clips carry a tone */, surface) {
        Ok(m) => t.mpv = Some(m),
        Err(e) => { log(&format!("play failed: {e}")); t.done = true }
    }
}

fn poll(t: &mut T, _: f64, tx: &mut Tx) -> bool {
    tx.wake_after(Duration::from_millis(100));
    if t.done { tx.quit(); return false }
    let Some(m) = &t.mpv else { return false };
    if let Some(e) = m.pump(&mut t.pb) { log(&format!("ended early: {e:?}")); t.done = true; return false }
    if t.child { m.fit() } else if t.size.0 > 0 { m.set_video_size(t.size.0, t.size.1) }
    // Measure from 3 s into playback, past start-up.
    let count = |n: &str| m.property(n).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    if t.pb.started && t.pb.pos > 3.0 && t.started.is_none() {
        t.started = Some((Instant::now(), cpu_seconds(), count("frame-drop-count"), count("vo-delayed-frame-count")));
    }
    if let Some((at, cpu, drops, delayed)) = t.started && at.elapsed().as_secs_f64() >= t.secs {
        let wall = at.elapsed().as_secs_f64();
        let used = cpu_seconds() - cpu;
        let p = |n: &str| m.property(n).unwrap_or_default();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let mut line = format!("{} | {} | {}x{} {} | {:.1} s: CPU {:.1}% of one core ({:.2}% of {} cores) | dropped {} delayed {} (in the window; since start {}) avsync {} | hwdec {} | fps {}",
            t.clip.rsplit(['\\', '/']).next().unwrap_or(""), if t.child { "child window" } else { "Rune NativeSurface (GPU)" },
            p("width"), p("height"), p("video-params/pixelformat"), wall, used / wall * 100.0, used / wall * 100.0 / cores, cores,
            count("frame-drop-count") - drops, count("vo-delayed-frame-count") - delayed, p("frame-drop-count"), p("avsync"), p("hwdec-current"), p("estimated-vf-fps"));
        if let Some(s) = m.video_stats() {
            line += &format!(" | rendered {} at {}x{}, {:.2} ms/frame on the render thread, starved {} | surface draws {:?} error {:?}",
                s.frames, s.size.0, s.size.1, s.render_ms / s.frames.max(1) as f64, s.starved, t.surface.counts(), t.surface.last_error());
        }
        log(&line);
        t.done = true;
    }
    false
}

fn view(t: &T, _: &mut Cx<T>) -> El {
    let video = el().w_full().h_full().fill(rune::Color::BLACK)
        .on_layout_tx(|t: &mut T, r, tx| {
            tx.live_only();
            let s = crate::window::scale();
            t.size = ((r[2] * s).round() as u32, (r[3] * s).round() as u32);
        });
    if t.child { video } else { video.native_surface(&t.surface, SurfaceFit::Fit) }
}

/// Run the test window if `STREMIO_RUNE_VIDEOTEST` names a video; true when it ran.
pub fn run() -> bool {
    let Ok(clip) = std::env::var("STREMIO_RUNE_VIDEOTEST") else { return false };
    let child = std::env::args().any(|a| a == "--child");
    // Shared D3D textures import on DX12 only; the child window keeps today's backend choice.
    if !child && std::env::var_os("WGPU_BACKEND").is_none() { unsafe { std::env::set_var("WGPU_BACKEND", "dx12") } }
    let secs = std::env::var("VIDEOTEST_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(20.0);
    let init = move || T { clip: clip.clone(), child, surface: NativeSurface::new(), mpv: None, pb: Playback::default(), size: (0, 0), started: None, secs, done: false };
    let app = rune::App::new(AppSpec { setup: Some(setup), poll: Some(poll), ..AppSpec::new("Video test", (1280.0, 720.0), || unreachable!(), view) });
    let app = if child { app.on_window(crate::window::overlay::set_main) } else { app };
    app.init(init).run();
    true
}
