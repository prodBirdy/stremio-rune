//! Borderless fullscreen for the app window. Rune doesn't expose its window, so this goes to
//! Win32 directly (Raymond Chen's style swap); elsewhere it's a no-op for now.

/// Toggle the active top-level window between fullscreen and how it was. Returns whether it
/// is fullscreen now. Call on the UI thread.
#[cfg(windows)]
pub fn toggle_fullscreen() -> bool {
    use std::sync::Mutex;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GA_ROOT, GWL_STYLE, GetAncestor, GetWindowLongPtrW, GetWindowPlacement, HWND_TOP, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, WINDOWPLACEMENT,
        WS_OVERLAPPEDWINDOW,
    };
    /// The style and placement to restore, while fullscreen.
    static SAVED: Mutex<Option<(isize, WINDOWPLACEMENT)>> = Mutex::new(None);

    let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        // The app window itself: with the controls in their own window, the active window can be
        // that one, and fullscreening it would leave the app as it was.
        let hwnd = Some(overlay::main()).filter(|h| !h.is_null()).unwrap_or_else(|| GetAncestor(GetActiveWindow(), GA_ROOT));
        if hwnd.is_null() { return saved.is_some() }
        // Hide the windows from the screen while they change, so the switch shows as one cut
        // instead of a frame repaint, a video at its old size and controls smeared mid-resize.
        cloak_briefly(&[hwnd, overlay::window()]);
        if let Some((style, place)) = saved.take() {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowPlacement(hwnd, &place);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
            for h in [hwnd, overlay::window()] { taskbar::mark_fullscreen(h, false) }
            return false;
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let mut place: WINDOWPLACEMENT = std::mem::zeroed();
        place.length = size_of::<WINDOWPLACEMENT>() as u32;
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetWindowPlacement(hwnd, &mut place) == 0 || GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
            return false;
        }
        let r = mi.rcMonitor;
        SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW as isize));
        SetWindowPos(hwnd, HWND_TOP, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        for h in [hwnd, overlay::window()] { taskbar::mark_fullscreen(h, true) }
        // The fullscreen window has to be the one in front, not the controls that were clicked.
        windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        *saved = Some((style, place));
        true
    }
}

/// The taskbar, through `ITaskbarList2`'s vtable (windows-sys has no COM wrappers).
#[cfg(windows)]
mod taskbar {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows_sys::Win32::UI::Shell::TaskbarList;
    const IID_ITASKBARLIST2: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x602d4995_b13a_429b_a66e_1935e44f4317);

    #[repr(C)]
    struct Vtbl {
        _query_interface: usize,
        _add_ref: usize,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        hr_init: unsafe extern "system" fn(*mut c_void) -> i32,
        _add_tab: usize,
        delete_tab: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
        _activate_tab: usize,
        _set_active_alt: usize,
        mark_fullscreen_window: unsafe extern "system" fn(*mut c_void, HWND, i32) -> i32,
    }

    fn with(f: impl FnOnce(&Vtbl, *mut c_void)) {
        unsafe {
            CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32); // winit already did: harmless
            let mut list: *mut c_void = std::ptr::null_mut();
            if CoCreateInstance(&TaskbarList, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBARLIST2, &mut list) < 0 || list.is_null() { return }
            let vtbl = &**(list as *const *const Vtbl);
            if (vtbl.hr_init)(list) >= 0 { f(vtbl, list) }
            (vtbl.release)(list);
        }
    }

    /// The shell's own fullscreen detection misses the window (it's cloaked at the moment it
    /// covers the monitor, and the controls' window may be the one in front), so say it.
    pub fn mark_fullscreen(hwnd: HWND, on: bool) {
        if !hwnd.is_null() { with(|v, l| unsafe { (v.mark_fullscreen_window)(l, hwnd, on as i32); }) }
    }

    /// No taskbar button for this window (the controls': one app, one button).
    pub fn drop_tab(hwnd: HWND) {
        if !hwnd.is_null() { with(|v, l| unsafe { (v.delete_tab)(l, hwnd); }) }
    }
}

/// Cloak windows now and uncloak them once the app, mpv and the controls have redrawn at the
/// new size (a few frames). Null handles are skipped.
#[cfg(windows)]
fn cloak_briefly(windows: &[windows_sys::Win32::Foundation::HWND]) {
    fn cloak(h: isize, on: i32) {
        use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
        unsafe { DwmSetWindowAttribute(h as _, DWMWA_CLOAK as _, (&raw const on).cast(), 4) };
    }
    let hs: Vec<isize> = windows.iter().filter(|h| !h.is_null()).map(|h| *h as isize).collect();
    for &h in &hs { cloak(h, 1) }
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        for h in hs { cloak(h, 0) }
    });
}

#[cfg(not(windows))]
pub fn toggle_fullscreen() -> bool { false }

/// The mouse position on screen, wherever it is (over mpv's window too, which Rune can't see).
#[cfg(windows)]
pub fn cursor() -> (i32, i32) {
    let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) };
    (p.x, p.y)
}

#[cfg(not(windows))]
pub fn cursor() -> (i32, i32) { (0, 0) }

/// Device pixels per point on the primary display, and its width in points (the widest the
/// window gets), for sizing images.
#[cfg(windows)]
pub fn scale() -> f32 { unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0 } }

#[cfg(windows)]
pub fn screen_w() -> f32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN};
    unsafe { GetSystemMetrics(SM_CXSCREEN) as f32 / scale() }
}

#[cfg(not(windows))]
pub fn scale() -> f32 { 2.0 }

#[cfg(not(windows))]
pub fn screen_w() -> f32 { 1920.0 }

/// Width of each caption button (minimize, maximize, close), in points, as Windows 11 draws them.
pub const CAPTION_BUTTON_W: f32 = 46.0;

/// The app's own title strip, where the system title bar was.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Caption {
    /// Its height in points: the system caption's (0 in fullscreen).
    pub h: f32,
    pub zoomed: bool,
    /// The pointer is over the maximize button, which the system hit-tests (snap layouts), so
    /// Rune doesn't see it.
    pub max_hover: bool,
}

/// The app window's title strip, and the window behind it: the system draws no title bar (see
/// `overlay::frame`); the app draws the strip and its buttons, and the system still drags,
/// snaps, double-click maximizes and resizes from every edge.
#[cfg(windows)]
pub fn caption() -> Caption {
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed;
    let main = overlay::main();
    let dpi = if main.is_null() { 96 } else { unsafe { GetDpiForWindow(main) } };
    Caption {
        h: overlay::strip_px(main, dpi) as f32 * 96.0 / dpi as f32,
        zoomed: !main.is_null() && unsafe { IsZoomed(main) } != 0,
        max_hover: overlay::MAX_HOVER.load(std::sync::atomic::Ordering::Relaxed),
    }
}

#[cfg(not(windows))]
pub fn caption() -> Caption { Caption { h: 32.0, ..Default::default() } }

/// A caption button's command (`SC_MINIMIZE`, `SC_CLOSE`…) to the app window, as the system's
/// own buttons send it.
#[cfg(windows)]
pub fn syscommand(sc: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_SYSCOMMAND};
    unsafe { PostMessageW(overlay::main(), WM_SYSCOMMAND, sc as usize, 0) };
}

#[cfg(windows)]
pub use windows_sys::Win32::UI::WindowsAndMessaging::{SC_CLOSE, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE};

#[cfg(not(windows))]
pub fn syscommand(_: u32) {}
#[cfg(not(windows))]
pub const SC_CLOSE: u32 = 0; #[cfg(not(windows))]
pub const SC_MAXIMIZE: u32 = 0; #[cfg(not(windows))]
pub const SC_MINIMIZE: u32 = 0; #[cfg(not(windows))]
pub const SC_RESTORE: u32 = 0;

/// The app window's frame (title bar removed, see `frame`), and the floating controls: a
/// transparent Rune window owned by the app window, so it stays above it (and mpv's child
/// window), hides with it when minimized, and gets no taskbar entry. The app window's moves and
/// resizes, fullscreen included, carry it along.
#[cfg(windows)]
pub mod overlay {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GetClientRect, GetForegroundWindow, GetWindowLongPtrW, HTCAPTION, HTCLIENT,
        HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER, SM_CYCAPTION,
        SM_CYSIZEFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow,
        SetWindowLongPtrW, SetWindowPos, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP,
        WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE, WM_SYSCOMMAND, WM_WINDOWPOSCHANGED, WS_CAPTION, WS_EX_APPWINDOW,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    /// The app window, from `App::on_window`, and the controls' window once shown.
    static MAIN: AtomicIsize = AtomicIsize::new(0);
    pub(super) static MAX_HOVER: AtomicBool = AtomicBool::new(false);
    /// Wakes the app's `poll` when the caption changes behind Rune's back.
    static WAKE: OnceLock<rune::Waker> = OnceLock::new();

    /// The app window was created: take over its frame.
    pub fn set_main(w: &rune::WindowHandle) {
        let Some(h) = hwnd(w) else { return };
        MAIN.store(h as isize, Ordering::Relaxed);
        unsafe {
            SetWindowSubclass(h, Some(frame), 0x5354, 0); // same id: replaces, no stacking
            SetWindowPos(h, std::ptr::null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    pub fn set_waker(w: rune::Waker) { let _ = WAKE.set(w); }

    fn wake() { if let Some(w) = WAKE.get() { w() } }

    /// The app window (null until Rune reports it).
    pub fn main() -> HWND { MAIN.load(Ordering::Relaxed) as HWND }

    /// The controls' window, once attached (null before).
    static OVERLAY: AtomicIsize = AtomicIsize::new(0);
    pub fn window() -> HWND { OVERLAY.load(Ordering::Relaxed) as HWND }

    /// Show the controls' window over the app window, or hide it.
    pub fn show(w: &rune::WindowHandle, on: bool) {
        if let (true, Some(o)) = (on, hwnd(w)) { if !main().is_null() { attach(main(), o) } }
        w.set_visible(on);
        // Showing can restyle the window (winit applies its own flags) and give it a taskbar
        // button; own it and drop the button again so the app shows as one window.
        if let (true, Some(o)) = (on, hwnd(w)) {
            if !main().is_null() { attach(main(), o) }
            super::taskbar::drop_tab(o);
        }
    }

    /// Hand activation back to the app window if showing the controls took it: keys go there.
    pub fn keep_keys(w: &rune::WindowHandle) {
        if hwnd(w).is_some_and(|o| unsafe { GetForegroundWindow() } == o) { unsafe { SetForegroundWindow(main()); } }
    }

    /// A Rune window's HWND.
    pub fn hwnd(w: &rune::WindowHandle) -> Option<HWND> {
        w.with_raw_handle(|h| match h.as_raw() {
            rune::raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }).ok().flatten()
    }

    /// Own `overlay` by `main` (the app window) and keep it over `main`'s video area.
    /// Idempotent; call on the UI thread while `overlay` is hidden (the style change needs it).
    pub fn attach(main: HWND, overlay: HWND) {
        unsafe {
            SetWindowLongPtrW(overlay, GWLP_HWNDPARENT, main as isize);
            let ex = GetWindowLongPtrW(overlay, GWL_EXSTYLE);
            SetWindowLongPtrW(overlay, GWL_EXSTYLE, (ex | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize) & !(WS_EX_APPWINDOW as isize));
        }
        OVERLAY.store(overlay as isize, Ordering::Relaxed);
        place(main, overlay);
    }

    /// Cover `main`'s client area below the title strip.
    fn place(main: HWND, overlay: HWND) {
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(main, &mut r);
            let top = strip_px(main, GetDpiForWindow(main));
            let mut at = POINT { x: 0, y: top };
            ClientToScreen(main, &mut at);
            SetWindowPos(overlay, std::ptr::null_mut(), at.x, at.y, r.right, (r.bottom - top).max(0), SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// mpv's host window, a child of `main`, over the same area (same as `Host::fit`).
    fn place_video(main: HWND, video: HWND) {
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(main, &mut r);
            let top = strip_px(main, GetDpiForWindow(main));
            SetWindowPos(video, std::ptr::null_mut(), 0, top, r.right, (r.bottom - top).max(0), SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    /// The resize border the system keeps above the caption, at `dpi`.
    fn frame_px(dpi: u32) -> i32 {
        unsafe { GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) }
    }

    fn captioned(hwnd: HWND) -> bool {
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) };
        style & WS_CAPTION as isize == WS_CAPTION as isize
    }

    /// Height of the app's title strip in `hwnd`'s pixels: where the system's title bar and top
    /// border were (only the caption when maximized, where the border is off screen), 0 without
    /// a title bar (fullscreen). A null `hwnd` (headless) gets the windowed height.
    pub fn strip_px(hwnd: HWND, dpi: u32) -> i32 {
        if !hwnd.is_null() && !captioned(hwnd) { return 0 }
        let caption = unsafe { GetSystemMetricsForDpi(SM_CYCAPTION, dpi) };
        if !hwnd.is_null() && unsafe { IsZoomed(hwnd) } != 0 { caption } else { caption + frame_px(dpi) }
    }

    fn set_max_hover(on: bool) {
        if MAX_HOVER.swap(on, Ordering::Relaxed) != on { wake() }
    }

    /// The app window's frame without the title bar: the client area grows over it (the app
    /// draws the strip), the sides and bottom keep the system's resize borders, and hit-testing
    /// gives the strip back to the system (drag, snap, double-click) except the caption
    /// buttons; maximize answers as the system's own, for Windows 11's snap layouts.
    unsafe extern "system" fn frame(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
        let def = || unsafe { DefSubclassProc(hwnd, msg, wp, lp) };
        let custom = captioned(hwnd);
        match msg {
            WM_WINDOWPOSCHANGED => {
                if !window().is_null() { place(hwnd, window()) }
                // The video's window too, in the same instant: waiting for the app's next poll
                // lags a resize (leaving fullscreen most visibly).
                let video = VIDEO.load(Ordering::Relaxed) as HWND;
                if !video.is_null() { place_video(hwnd, video) }
            }
            WM_SIZE => wake(), // maximized or restored: the strip's height and icon change
            WM_NCCALCSIZE if custom && wp != 0 => unsafe {
                let p = lp as *mut NCCALCSIZE_PARAMS;
                let top = (*p).rgrc[0].top;
                let r = def();
                // Maximized windows hang their border off screen; keep the content on it.
                (*p).rgrc[0].top = top + if IsZoomed(hwnd) != 0 { frame_px(GetDpiForWindow(hwnd)) } else { 0 };
                return r;
            },
            WM_NCHITTEST if custom => {
                let hit = def();
                if hit != HTCLIENT as LRESULT { return hit }
                let mut pt = POINT { x: (lp & 0xFFFF) as i16 as i32, y: ((lp >> 16) & 0xFFFF) as i16 as i32 };
                let mut r: RECT = unsafe { std::mem::zeroed() };
                let dpi = unsafe { ScreenToClient(hwnd, &mut pt); GetClientRect(hwnd, &mut r); GetDpiForWindow(hwnd) };
                let edge = frame_px(dpi);
                if unsafe { IsZoomed(hwnd) } == 0 && pt.y < edge {
                    return (if pt.x < 2 * edge { HTTOPLEFT } else if pt.x >= r.right - 2 * edge { HTTOPRIGHT } else { HTTOP }) as LRESULT;
                }
                if pt.y < strip_px(hwnd, dpi) {
                    let button = (super::CAPTION_BUTTON_W * dpi as f32 / 96.0).round() as i32;
                    return match (r.right - 1 - pt.x) / button.max(1) { 0 | 2 => HTCLIENT, 1 => HTMAXBUTTON, _ => HTCAPTION } as LRESULT;
                }
                return hit;
            }
            WM_NCMOUSEMOVE => {
                let on = wp == HTMAXBUTTON as usize;
                if on && !MAX_HOVER.load(Ordering::Relaxed) {
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
                    unsafe { TrackMouseEvent(&mut t) };
                }
                set_max_hover(on);
            }
            WM_NCMOUSELEAVE => set_max_hover(false),
            // The maximize button is ours to draw: the system's would paint its own over it.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wp == HTMAXBUTTON as usize => return 0,
            WM_NCLBUTTONUP if wp == HTMAXBUTTON as usize => {
                let sc = if unsafe { IsZoomed(hwnd) } != 0 { super::SC_RESTORE } else { super::SC_MAXIMIZE };
                return unsafe { DefSubclassProc(hwnd, WM_SYSCOMMAND, sc as usize, 0) };
            }
            _ => {}
        }
        def()
    }

    /// mpv's host window while one exists (see `player.rs`), resized by `frame`.
    static VIDEO: AtomicIsize = AtomicIsize::new(0);
    pub fn set_video(h: HWND) { VIDEO.store(h as isize, Ordering::Relaxed) }
}

#[cfg(not(windows))]
pub mod overlay {
    pub fn set_main(_: &rune::WindowHandle) {}
    pub fn set_waker(_: rune::Waker) {}
    pub fn show(w: &rune::WindowHandle, on: bool) { w.set_visible(on) }
    pub fn keep_keys(_: &rune::WindowHandle) {}
}
