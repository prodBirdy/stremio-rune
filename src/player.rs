//! Playback through the libmpv Stremio ships. On Windows mpv draws into a child window covering
//! the app window; the loader and controls float above it in their own window (`ui::Ctl`), and
//! read [`Playback`].

use rune::Waker;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

/// An audio or subtitle track mpv knows: embedded, or added from an addon (`external` = its URL).
#[derive(Clone, Debug, PartialEq)]
pub struct Track { pub id: String, pub label: String, pub external: String }

/// What the controls show, kept current from mpv's events.
#[derive(Clone, Default, PartialEq)]
pub struct Playback {
    pub pos: f64,
    pub dur: f64,
    pub paused: bool,
    /// The first frame is up; until then the app shows its loader.
    pub started: bool,
    /// Stalled waiting for the network, and how full the cache is (0–100).
    pub buffering: bool,
    pub cache_pct: f64,
    pub tracks: Vec<Track>,
    /// Selected subtitle track id, or `no`.
    pub sid: String,
    pub audio: Vec<Track>,
    /// Selected audio track id, or `no`.
    pub aid: String,
    /// `script-message`s from mpv's key bindings (`fullscreen`, `escape`) for the app to act on.
    pub messages: Vec<String>,
}

impl Playback {
    pub fn loading(&self) -> bool { !self.started || self.buffering }
}

/// mpv core, the boxed waker its wakeup callback holds, and the child window it draws in.
/// Fields drop in order after `Drop::drop` destroys the core.
pub struct Mpv {
    ctx: *mut c_void,
    waker: *mut Waker,
    #[cfg(windows)]
    host: Option<win::Host>,
}

fn libmpv() -> Result<&'static libloading::Library, String> {
    static LIB: OnceLock<Result<libloading::Library, String>> = OnceLock::new();
    LIB.get_or_init(|| {
        let path = std::env::var_os("MPV_LIB").map(PathBuf::from).unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join(r"Programs\Stremio\libmpv-2.dll")
        });
        // Altered search path: the DLL's own dependencies sit beside it.
        #[cfg(windows)]
        let lib = unsafe {
            libloading::os::windows::Library::load_with_flags(&path, libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH).map(Into::into)
        };
        #[cfg(not(windows))]
        let lib = unsafe { libloading::Library::new(&path) };
        lib.map_err(|e| format!("Couldn't load libmpv from {} ({e}); set MPV_LIB", path.display()))
    }).as_ref().map_err(Clone::clone)
}

pub fn sym<T: Copy>(name: &[u8]) -> Result<T, String> {
    unsafe { libmpv()?.get::<T>(name).map(|f| *f).map_err(|e| e.to_string()) }
}

unsafe extern "C" fn on_mpv_wakeup(data: *mut c_void) {
    unsafe { (*(data as *const Waker))() }
}

fn cs(s: &str) -> CString { CString::new(s).unwrap_or_default() }

// mpv_format values and event ids used here (client.h).
const STRING: c_int = 1;
const FLAG: c_int = 3;
const INT64: c_int = 4;
const DOUBLE: c_int = 5;

impl Mpv {
    /// Start `url`, at `start` seconds if given.
    pub fn play(url: &str, title: &str, waker: Option<Waker>, start: Option<f64>) -> Result<Mpv, String> {
        let create: unsafe extern "C" fn() -> *mut c_void = sym(b"mpv_create\0")?;
        let set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int = sym(b"mpv_set_option_string\0")?;
        let init: unsafe extern "C" fn(*mut c_void) -> c_int = sym(b"mpv_initialize\0")?;
        let on_wake: unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(*mut c_void), *mut c_void) =
            sym(b"mpv_set_wakeup_callback\0")?;
        let observe: unsafe extern "C" fn(*mut c_void, u64, *const c_char, c_int) -> c_int = sym(b"mpv_observe_property\0")?;
        let ctx = unsafe { create() };
        if ctx.is_null() { return Err("mpv_create failed".into()) }
        let waker = Box::into_raw(Box::new(waker.unwrap_or_else(|| std::sync::Arc::new(|| {}))));
        let mpv = Mpv { ctx, waker, #[cfg(windows)] host: win::Host::new() }; // cleaned up on any early return
        // hwdec: mpv defaults to CPU decoding, which can't keep up with 4K/8K HEVC.
        // cursor-autohide matches the app's controls, which hide after the same idle time.
        let mut opts = vec![("force-window", "yes".to_string()), ("osc", "no".into()), ("input-default-bindings", "yes".into()),
            ("input-vo-keyboard", "yes".into()), ("ytdl", "no".into()), ("hwdec", "auto-safe".into()),
            ("cursor-autohide", "2500".into()), ("title", title.into())];
        if let Some(at) = start { opts.push(("start", format!("{at}"))) }
        #[cfg(windows)]
        if let Some(h) = &mpv.host { opts.push(("wid", (h.0 as isize).to_string())) }
        unsafe {
            on_wake(ctx, on_mpv_wakeup, waker.cast());
            for (k, v) in &opts { set(ctx, cs(k).as_ptr(), cs(v).as_ptr()); }
            if init(ctx) < 0 { return Err("mpv_initialize failed".into()) }
            // userdata ids match `pump`.
            for (id, name, format) in [(1, "time-pos", DOUBLE), (2, "duration", DOUBLE), (3, "pause", FLAG),
                (4, "paused-for-cache", FLAG), (5, "cache-buffering-state", INT64), (6, "sid", STRING), (7, "track-list/count", INT64), (8, "aid", STRING)] {
                observe(ctx, id, cs(name).as_ptr(), format);
            }
        }
        // Keys and clicks land in mpv's window while the video has focus; hand the
        // app-level ones back to the app.
        for (key, cmd) in [("ESC", "script-message escape"), ("f", "script-message fullscreen"),
            ("F11", "script-message fullscreen"), ("MBTN_LEFT_DBL", "script-message fullscreen")] {
            mpv.command(&["keybind", key, cmd])?;
        }
        mpv.command(&["loadfile", url]).map_err(|_| format!("mpv couldn't load {url}"))?;
        Ok(mpv)
    }

    pub fn command(&self, args: &[&str]) -> Result<(), String> {
        let command: unsafe extern "C" fn(*mut c_void, *const *const c_char) -> c_int = sym(b"mpv_command\0")?;
        let args: Vec<CString> = args.iter().map(|a| cs(a)).collect();
        let argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).chain([std::ptr::null()]).collect();
        if unsafe { command(self.ctx, argv.as_ptr()) } < 0 { return Err(format!("mpv {} failed", args[0].to_string_lossy())) }
        Ok(())
    }

    /// Play `url` instead, from `at` seconds, keeping the audio and subtitle languages. Track ids
    /// belong to the old file, so the new one picks by language; an addon subtitle comes along.
    pub fn switch(&self, url: &str, at: f64) -> Result<(), String> {
        let lang = |kind: &str| self.get(&format!("current-tracks/{kind}/lang"));
        if let Some(l) = lang("audio") { self.command(&["set", "alang", &l])? }
        self.command(&["change-list", "sub-files", "clr", ""])?;
        match (self.get("current-tracks/sub/external-filename"), lang("sub")) {
            (Some(file), _) => self.command(&["change-list", "sub-files", "append", &file])?,
            (None, Some(l)) => self.command(&["set", "slang", &l])?,
            _ => {}
        }
        let sub_on = self.get("current-tracks/sub/id").is_some();
        self.command(&["set", "aid", "auto"])?;
        self.command(&["set", "sid", if sub_on { "auto" } else { "no" }])?;
        self.command(&["set", "start", &at.to_string()])?;
        self.command(&["loadfile", url, "replace"]).map_err(|_| "mpv couldn't load that stream".to_string())
    }

    fn get(&self, name: &str) -> Option<String> {
        let get: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char = sym(b"mpv_get_property_string\0").ok()?;
        let free: unsafe extern "C" fn(*mut c_void) = sym(b"mpv_free\0").ok()?;
        let p = unsafe { get(self.ctx, cs(name).as_ptr()) };
        if p.is_null() { return None }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        unsafe { free(p.cast()) };
        Some(s)
    }

    /// The tracks of one `kind` (`sub`, `audio`), read when mpv's track count changes.
    fn tracks(&self, kind: &str) -> Vec<Track> {
        let n: usize = self.get("track-list/count").and_then(|n| n.parse().ok()).unwrap_or(0);
        (0..n).filter(|i| self.get(&format!("track-list/{i}/type")).as_deref() == Some(kind)).map(|i| {
            let field = |f: &str| self.get(&format!("track-list/{i}/{f}")).unwrap_or_default();
            let (lang, title) = (field("lang"), field("title"));
            let label = if kind == "audio" {
                audio_label(&lang, &title, field("demux-channel-count").parse().unwrap_or(0), &field("codec"))
            } else {
                [lang.to_uppercase(), title].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
            };
            Track { id: field("id"), label: if label.is_empty() { format!("Track {}", field("id")) } else { label }, external: field("external-filename") }
        }).collect()
    }

    /// Size mpv's window to the app window. Cheap when nothing changed; call every poll.
    pub fn fit(&self) {
        #[cfg(windows)]
        if let Some(h) = &self.host { h.fit() }
    }

    /// Drain mpv's events into `p`. `Some(error)` once playback is over: Esc or `q`, end of
    /// file, or a failure (`Some(Some(message))`).
    pub fn pump(&self, p: &mut Playback) -> Option<Option<String>> {
        #[repr(C)] struct Event { id: c_int, error: c_int, reply: u64, data: *mut c_void }
        #[repr(C)] struct EndFile { reason: c_int, error: c_int }
        #[repr(C)] struct Prop { name: *const c_char, format: c_int, data: *mut c_void }
        #[repr(C)] struct ClientMessage { num_args: c_int, args: *const *const c_char }
        let wait: unsafe extern "C" fn(*mut c_void, f64) -> *const Event = sym(b"mpv_wait_event\0").ok()?;
        let describe: unsafe extern "C" fn(c_int) -> *const c_char = sym(b"mpv_error_string\0").ok()?;
        let mut ended = None;
        loop {
            let e = unsafe { &*wait(self.ctx, 0.0) };
            match e.id {
                0 => return ended,       // NONE: queue drained
                1 => ended = Some(None), // SHUTDOWN: q, or the window closed
                7 => {                   // END_FILE
                    let end = unsafe { &*(e.data as *const EndFile) };
                    if end.reason == 2 { continue } // STOP: `switch` replaced the file
                    let msg = (end.reason == 4).then(|| unsafe { CStr::from_ptr(describe(end.error)) });
                    ended = Some(msg.map(|m| format!("Playback failed: {}", m.to_string_lossy())));
                }
                16 => {                  // CLIENT_MESSAGE: a `script-message` binding fired
                    let m = unsafe { &*(e.data as *const ClientMessage) };
                    if m.num_args > 0 {
                        p.messages.push(unsafe { CStr::from_ptr(*m.args) }.to_string_lossy().into_owned());
                    }
                }
                21 => p.started = true,  // PLAYBACK_RESTART: the first frame (or a seek) landed
                22 => {                  // PROPERTY_CHANGE; format 0 = no value yet
                    let prop = unsafe { &*(e.data as *const Prop) };
                    if prop.format == 0 || prop.data.is_null() { continue }
                    unsafe {
                        match e.reply {
                            1 => p.pos = *(prop.data as *const f64),
                            2 => p.dur = *(prop.data as *const f64),
                            3 => p.paused = *(prop.data as *const c_int) != 0,
                            4 => p.buffering = *(prop.data as *const c_int) != 0,
                            5 => p.cache_pct = *(prop.data as *const i64) as f64,
                            6 => p.sid = CStr::from_ptr(*(prop.data as *const *const c_char)).to_string_lossy().into_owned(),
                            7 => (p.tracks, p.audio) = (self.tracks("sub"), self.tracks("audio")),
                            8 => p.aid = CStr::from_ptr(*(prop.data as *const *const c_char)).to_string_lossy().into_owned(),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// `English · Commentary · 5.1 · TrueHD`: what tells audio tracks apart. Empty parts drop out.
pub fn audio_label(lang: &str, title: &str, channels: u32, codec: &str) -> String {
    let lang = crate::ui::language(lang).to_string();
    let channels = match channels { 0 => String::new(), 1 => "Mono".into(), 2 => "Stereo".into(), 6 => "5.1".into(), 8 => "7.1".into(), n => format!("{n} ch") };
    let codec = match codec {
        "truehd" => "TrueHD".into(), "eac3" => "E-AC-3".into(), "ac3" => "AC-3".into(), "dts" => "DTS".into(),
        "opus" => "Opus".into(), "vorbis" => "Vorbis".into(), c => c.to_uppercase(),
    };
    // Titles often repeat the language ("English"); keep them only when they add something.
    let title = if title.eq_ignore_ascii_case(&lang) { String::new() } else { title.to_string() };
    [lang, title, channels, codec].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ")
}

impl Drop for Mpv {
    fn drop(&mut self) {
        if let Ok(destroy) = sym::<unsafe extern "C" fn(*mut c_void)>(b"mpv_terminate_destroy\0") {
            unsafe { destroy(self.ctx) }
        }
        drop(unsafe { Box::from_raw(self.waker) }); // after destroy: mpv can't call it any more
    }
}

#[cfg(windows)]
mod win {
    use std::cell::Cell;
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetClientRect, GetParent, MoveWindow, SW_SHOWNA, ShowWindow, WS_CHILD,
        WS_CLIPCHILDREN,
    };

    /// Black until mpv's own window paints (the STATIC class's SS_BLACKRECT).
    const SS_BLACKRECT: u32 = 0x4;

    /// A child of the app window that mpv draws into. Created and destroyed on the UI thread.
    /// Starts hidden; `fit` remembers what it last applied so repeat calls do nothing.
    pub struct Host(pub HWND, Cell<Option<(i32, i32, i32)>>);

    impl Host {
        /// Under the app window (its handle from Rune at start-up; else the active window).
        pub fn new() -> Option<Host> {
            let parent = Some(crate::window::overlay::main()).filter(|h| !h.is_null()).unwrap_or_else(|| unsafe { GetActiveWindow() });
            if parent.is_null() { return None }
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let hwnd = unsafe {
                CreateWindowExW(0, class.as_ptr(), std::ptr::null(), WS_CHILD | WS_CLIPCHILDREN | SS_BLACKRECT, 0, 0, 0, 0, parent,
                    std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null())
            };
            (!hwnd.is_null()).then(|| Host(hwnd, Cell::new(None)))
        }

        /// Cover the parent's client area below its title strip, and show.
        pub fn fit(&self) {
            unsafe {
                let parent = GetParent(self.0);
                let mut r: RECT = std::mem::zeroed();
                GetClientRect(parent, &mut r);
                let top = crate::window::overlay::strip_px(parent, windows_sys::Win32::UI::HiDpi::GetDpiForWindow(parent));
                let (w, h) = (r.right, (r.bottom - top).max(0));
                if self.1.replace(Some((top, w, h))) == Some((top, w, h)) { return }
                MoveWindow(self.0, 0, top, w, h, 1);
                ShowWindow(self.0, SW_SHOWNA);
            }
        }
    }

    impl Drop for Host {
        fn drop(&mut self) { unsafe { DestroyWindow(self.0); } }
    }
}
