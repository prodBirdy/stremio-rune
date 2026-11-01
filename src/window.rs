//! Borderless fullscreen for the app window. Rune doesn't expose its window, so this goes to
//! Win32 directly (Raymond Chen's style swap); on macOS to AppKit (`mac` below: native
//! fullscreen, the title strip under the traffic lights); elsewhere it's a no-op for now.

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
        let hwnd = Some(chrome::main()).filter(|h| !h.is_null()).unwrap_or_else(|| GetAncestor(GetActiveWindow(), GA_ROOT));
        if hwnd.is_null() { return saved.is_some() }
        // The window hides only while its frame changes (else a bare system frame flashes). It has
        // to be visible when it takes the monitor's size: the shell only hides the taskbar for a
        // fullscreen window it can see at that moment.
        if let Some((style, place)) = saved.take() {
            cloak(hwnd, true);
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            cloak(hwnd, false);
            SetWindowPlacement(hwnd, &place);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
            taskbar::mark_fullscreen(hwnd, false);
            return false;
        }
        let mut place: WINDOWPLACEMENT = std::mem::zeroed();
        place.length = size_of::<WINDOWPLACEMENT>() as u32;
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetWindowPlacement(hwnd, &mut place) == 0 || GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
            return false;
        }
        let r = mi.rcMonitor;
        cloak(hwnd, true);
        // A maximized window isn't taken for fullscreen by the shell, so un-maximize first, as
        // Chromium does. The saved placement re-maximizes it on the way back.
        if windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd) != 0 {
            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::SW_RESTORE);
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW as isize));
        cloak(hwnd, false);
        SetWindowPos(hwnd, HWND_TOP, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
        taskbar::mark_fullscreen(hwnd, true);
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
        _delete_tab: usize,
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

    /// The shell's own fullscreen detection can miss the window (it's cloaked for a moment as
    /// its frame changes), so say it.
    pub fn mark_fullscreen(hwnd: HWND, on: bool) {
        if !hwnd.is_null() { with(|v, l| unsafe { (v.mark_fullscreen_window)(l, hwnd, on as i32); }) }
    }
}

/// Hide or show a window on screen (DWM cloaking) without changing its state.
#[cfg(windows)]
fn cloak(h: windows_sys::Win32::Foundation::HWND, on: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
    if h.is_null() { return }
    let on = on as i32;
    unsafe { DwmSetWindowAttribute(h, DWMWA_CLOAK as _, (&raw const on).cast(), 4) };
}

/// Linux: Hyprland's own fullscreen on the focused window (this one: the key or double-click that
/// asked came to it). Elsewhere on Linux there's no way through Rune yet.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn toggle_fullscreen() -> bool {
    let pid = std::process::id();
    // Hyprland 0.56+ dispatches Lua; older ones take the plain dispatcher (on the focused window).
    let lua = format!(r#"hl.dsp.window.fullscreen({{ mode = "fullscreen", action = "toggle", window = "pid:{pid}" }})"#);
    if linux::hyprctl(&["dispatch", &lua]).is_none() && linux::hyprctl(&["dispatch", "fullscreen", "0"]).is_none() { return false }
    // What Hyprland says now, so a Super+F in between doesn't leave it the wrong way round.
    let clients: Option<serde_json::Value> = linux::hyprctl(&["clients", "-j"]).and_then(|s| serde_json::from_str(&s).ok());
    clients.as_ref().and_then(|c| c.as_array()).into_iter().flatten()
        .find(|c| c["pid"].as_u64() == Some(pid as u64))
        .is_some_and(|c| c["fullscreen"].as_u64().is_some_and(|f| f != 0))
}

/// Whether the window is fullscreen now, where the system can change that on its own (macOS:
/// the green button, the menu bar); `None` where only `toggle_fullscreen` does.
#[cfg(not(target_os = "macos"))]
pub fn fullscreen_now() -> Option<bool> { None }

/// What decides whether the taskbar steps aside: the app window against its monitor, its
/// style, who's in front, cloaking, and where the taskbar sits in the z-order. For diagnosis.
#[cfg(windows)]
pub fn diag() -> String {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GW_HWNDNEXT, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetTopWindow, GetWindow, GetWindowLongPtrW,
        GetWindowRect, IsZoomed,
    };
    unsafe {
        let main = chrome::main();
        let rect = |h| { let mut r: RECT = std::mem::zeroed(); GetWindowRect(h, &mut r); (r.left, r.top, r.right, r.bottom) };
        let cloaked = |h| { let mut c: u32 = 0; DwmGetWindowAttribute(h, DWMWA_CLOAKED as _, (&raw mut c).cast(), 4); c };
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromWindow(main, MONITOR_DEFAULTTONEAREST), &mut mi);
        let m = mi.rcMonitor;
        let tray_class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        let tray = FindWindowW(tray_class.as_ptr(), std::ptr::null());
        // z-order from the top: where the app and the taskbar are.
        let (mut at, mut i, mut h) = ((None, None), 0, GetTopWindow(std::ptr::null_mut()));
        while !h.is_null() && i < 5000 {
            if h == main { at.0 = Some(i) } else if h == tray { at.1 = Some(i) }
            h = GetWindow(h, GW_HWNDNEXT);
            i += 1;
        }
        let fg = GetForegroundWindow();
        format!(
            "main rect {:?} monitor ({}, {}, {}, {}) style {:#x} ex {:#x} zoomed {} cloaked {} | foreground {} | z-order main {:?} taskbar {:?} (taskbar rect {:?})",
            rect(main), m.left, m.top, m.right, m.bottom, GetWindowLongPtrW(main, GWL_STYLE), GetWindowLongPtrW(main, GWL_EXSTYLE),
            IsZoomed(main) != 0, cloaked(main),
            if fg == main { "main" } else if fg == tray { "taskbar" } else { "other" },
            at.0, at.1, if tray.is_null() { None } else { Some(rect(tray)) },
        )
    }
}

#[cfg(not(windows))]
pub fn diag() -> String { String::new() }

/// For the fullscreen self-test: bring the app window to the front and maximize it, the state
/// the user's window was in. The Alt tap lets a process started in the background take the
/// foreground (Windows' focus-stealing guard).
#[cfg(windows)]
pub fn selftest_front_maximized() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, VK_MENU, keybd_event};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_MAXIMIZE, SetForegroundWindow, ShowWindow};
    unsafe {
        keybd_event(VK_MENU as u8, 0, 0, 0);
        keybd_event(VK_MENU as u8, 0, KEYEVENTF_KEYUP, 0);
        SetForegroundWindow(chrome::main());
        // STREMIO_RUNE_FSTEST_RECT=x,y,w,h: start from a normal window there instead (pixels).
        let rect: Option<Vec<i32>> = std::env::var("STREMIO_RUNE_FSTEST_RECT").ok()
            .map(|v| v.split(',').filter_map(|n| n.trim().parse().ok()).collect());
        match rect.as_deref() {
            Some(&[x, y, w, h]) => {
                use windows_sys::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SWP_NOZORDER, SetWindowPos};
                ShowWindow(chrome::main(), SW_RESTORE);
                SetWindowPos(chrome::main(), std::ptr::null_mut(), x, y, w, h, SWP_NOZORDER);
            }
            _ => { ShowWindow(chrome::main(), SW_MAXIMIZE); }
        }
    }
}

#[cfg(not(windows))]
pub fn selftest_front_maximized() {}

/// For the cursor self-test: the system cursor as `GetCursorInfo` sees it (showing flag, and
/// whether its image is the arrow, none, or another), and which window is under it.
#[cfg(windows)]
pub fn cursor_diag() -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GA_ROOT, GetAncestor, GetCursorInfo, IDC_ARROW, LoadCursorW, WindowFromPoint,
    };
    unsafe {
        let mut ci: CURSORINFO = std::mem::zeroed();
        ci.cbSize = size_of::<CURSORINFO>() as u32;
        GetCursorInfo(&mut ci);
        let arrow = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        let image = if ci.hCursor.is_null() { "none".to_string() } else if ci.hCursor == arrow { "arrow".into() } else { format!("other {:?}", ci.hCursor) };
        let under = WindowFromPoint(ci.ptScreenPos);
        let who = if under == chrome::main() { "app window" }
            else if !under.is_null() && GetAncestor(under, GA_ROOT) == chrome::main() { "mpv (child of the app window)" } else { "another window" };
        format!("showing {} image {image} | under the pointer: {who}", ci.flags & CURSOR_SHOWING != 0)
    }
}

#[cfg(not(windows))]
pub fn cursor_diag() -> String { String::new() }

/// For the self-tests: put the pointer at a fraction of the app window's client area.
#[cfg(windows)]
pub fn selftest_point_at(fx: f32, fy: f32) {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, SetCursorPos};
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(chrome::main(), &mut r);
        let mut p = POINT { x: (r.right as f32 * fx) as i32, y: (r.bottom as f32 * fy) as i32 };
        ClientToScreen(chrome::main(), &mut p);
        SetCursorPos(p.x, p.y);
    }
}

#[cfg(not(windows))]
pub fn selftest_point_at(_: f32, _: f32) {}

/// For the fullscreen self-test: nudge the mouse so the controls show for a screenshot.
#[cfg(windows)]
pub fn selftest_jiggle() {
    let (x, y) = cursor();
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x + 3, y + 3) };
}

#[cfg(not(windows))]
pub fn selftest_jiggle() {}

/// The mouse position on screen, wherever it is (over mpv's window too, which Rune can't see).
#[cfg(windows)]
pub fn cursor() -> (i32, i32) {
    let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) };
    (p.x, p.y)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn cursor() -> (i32, i32) {
    let v = linux::POINTER.load(std::sync::atomic::Ordering::Relaxed);
    ((v >> 32) as i32, v as i32)
}

/// Linux: the pointer as the player view saw it move (the video is Rune's own surface here, so
/// Rune sees the pointer over it); `cursor` reads it back.
pub fn pointer_moved(_x: f32, _y: f32) {
    #[cfg(not(any(windows, target_os = "macos")))]
    linux::POINTER.store(((_x as i32 as u32 as u64) << 32) | _y as i32 as u32 as u64, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(any(windows, target_os = "macos")))]
mod linux {
    use std::sync::OnceLock;
    use std::sync::atomic::AtomicU64;
    pub static POINTER: AtomicU64 = AtomicU64::new(0);

    pub fn hyprctl(args: &[&str]) -> Option<String> {
        std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let out = std::process::Command::new("hyprctl").args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The focused monitor's scale and width in points, from Hyprland; 1.0 and 1920 elsewhere.
    pub fn monitor() -> (f32, f32) {
        static M: OnceLock<(f32, f32)> = OnceLock::new();
        *M.get_or_init(|| {
            let mons: Option<serde_json::Value> = hyprctl(&["monitors", "-j"]).and_then(|s| serde_json::from_str(&s).ok());
            let list = mons.as_ref().and_then(|m| m.as_array()).cloned().unwrap_or_default();
            let m = list.iter().find(|m| m["focused"].as_bool() == Some(true)).or(list.first());
            let scale = m.and_then(|m| m["scale"].as_f64()).unwrap_or(1.0) as f32;
            let width = m.and_then(|m| m["width"].as_f64()).map_or(1920.0, |w| w as f32 / scale);
            (scale, width)
        })
    }
}

/// Device pixels per point on the primary display, and its width in points (the widest the
/// window gets), for sizing images.
#[cfg(windows)]
pub fn scale() -> f32 { unsafe { windows_sys::Win32::UI::HiDpi::GetDpiForSystem() as f32 / 96.0 } }

#[cfg(windows)]
pub fn screen_w() -> f32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN};
    unsafe { GetSystemMetrics(SM_CXSCREEN) as f32 / scale() }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn scale() -> f32 { linux::monitor().0 }

#[cfg(not(any(windows, target_os = "macos")))]
pub fn screen_w() -> f32 { linux::monitor().1 }

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
/// `chrome::frame`); the app draws the strip and its buttons, and the system still drags,
/// snaps, double-click maximizes and resizes from every edge.
#[cfg(windows)]
pub fn caption() -> Caption {
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed;
    let main = chrome::main();
    let dpi = if main.is_null() { 96 } else { unsafe { GetDpiForWindow(main) } };
    Caption {
        h: chrome::strip_px(main, dpi) as f32 * 96.0 / dpi as f32,
        zoomed: !main.is_null() && unsafe { IsZoomed(main) } != 0,
        max_hover: chrome::MAX_HOVER.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Linux: the window manager draws the frame (Hyprland: none), so the app draws no strip.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn caption() -> Caption { Caption::default() }

/// macOS: the strip is the transparent title bar's, 28 points as AppKit draws it, with the
/// traffic lights at its left; AppKit drags and zooms from it.
#[cfg(target_os = "macos")]
pub fn caption() -> Caption { Caption { h: 28.0, ..Default::default() } }

/// A caption button's command (`SC_MINIMIZE`, `SC_CLOSE`…) to the app window, as the system's
/// own buttons send it.
#[cfg(windows)]
pub fn syscommand(sc: u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_SYSCOMMAND};
    unsafe { PostMessageW(chrome::main(), WM_SYSCOMMAND, sc as usize, 0) };
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

/// The app window's frame: the title bar removed (see `frame`), the app's own strip, its icon.
#[cfg(windows)]
pub mod chrome {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT, TrackMouseEvent};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_STYLE, GetClientRect, GetWindowLongPtrW, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT,
        IsZoomed, NCCALCSIZE_PARAMS, SM_CXPADDEDBORDER, SM_CYCAPTION, SM_CYSIZEFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK,
        WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SIZE, WM_SYSCOMMAND, WS_CAPTION,
    };

    /// The app window, from `App::on_window`.
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
        set_icon(h);
    }

    /// The app icon (the exe's icon resource 1, see build.rs) on the window: the taskbar and
    /// Alt+Tab use the window's icon, and winit gives windows a generic one.
    fn set_icon(h: HWND) {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SM_CXICON, SM_CXSMICON, SendMessageW, WM_SETICON,
        };
        unsafe {
            let dpi = GetDpiForWindow(h);
            for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
                let px = GetSystemMetricsForDpi(metric, dpi);
                // MAKEINTRESOURCE(1): the id as a pointer.
                let icon = LoadImageW(GetModuleHandleW(std::ptr::null()), 1 as _, IMAGE_ICON, px, px, LR_DEFAULTCOLOR);
                if !icon.is_null() { SendMessageW(h, WM_SETICON, which as usize, icon as isize); }
            }
        }
    }

    pub fn set_waker(w: rune::Waker) { let _ = WAKE.set(w); }

    fn wake() { if let Some(w) = WAKE.get() { w() } }

    /// The app window (null until Rune reports it).
    pub fn main() -> HWND { MAIN.load(Ordering::Relaxed) as HWND }

    /// A Rune window's HWND.
    pub fn hwnd(w: &rune::WindowHandle) -> Option<HWND> {
        w.with_raw_handle(|h| match h.as_raw() {
            rune::raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
            _ => None,
        }).ok().flatten()
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

}

#[cfg(not(any(windows, target_os = "macos")))]
pub mod chrome {
    pub fn set_main(_: &rune::WindowHandle) {}
    pub fn set_waker(_: rune::Waker) {}
}

#[cfg(target_os = "macos")]
pub use mac::{chrome, cursor, fullscreen_now, scale, screen_w, toggle_fullscreen};

/// macOS: Rune's window is an AppKit `NSWindow` with full-size content under a transparent title
/// bar; fullscreen is AppKit's own (its own Space, the menu bar on hover), and the pointer and
/// display come from CoreGraphics.
#[cfg(target_os = "macos")]
mod mac {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::time::{Duration, Instant};

    #[repr(C)]
    struct CGPoint { x: f64, y: f64 }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
        fn CGDisplayModeGetWidth(mode: *mut c_void) -> usize;
        fn CGDisplayModeGetPixelWidth(mode: *mut c_void) -> usize;
        fn CGDisplayModeRelease(mode: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// `NSWindowStyleMaskFullScreen`.
    const FULLSCREEN: usize = 1 << 14;
    /// How long a requested switch outranks the window's own state: AppKit flips its style mask
    /// only as the animation gets going.
    const SWITCH: Duration = Duration::from_secs(2);

    /// The app window's `NSWindow` (null until Rune reports it).
    static WINDOW: AtomicPtr<AnyObject> = AtomicPtr::new(std::ptr::null_mut());
    /// The fullscreen state last asked for, and when.
    static WANTED: Mutex<Option<(bool, Instant)>> = Mutex::new(None);

    fn window() -> Option<*mut AnyObject> { Some(WINDOW.load(Ordering::Relaxed)).filter(|w| !w.is_null()) }

    pub fn fullscreen_now() -> Option<bool> {
        let w = window()?;
        let mask: usize = unsafe { msg_send![w, styleMask] };
        let on = mask & FULLSCREEN != 0;
        let mut wanted = WANTED.lock().unwrap_or_else(|e| e.into_inner());
        match *wanted {
            Some((want, at)) if want != on && at.elapsed() < SWITCH => Some(want),
            _ => { *wanted = None; Some(on) }
        }
    }

    /// Ask AppKit to switch; returns the state asked for. Call on the main (UI) thread.
    pub fn toggle_fullscreen() -> bool {
        let Some(w) = window() else { return false };
        let want = !fullscreen_now().unwrap_or(false);
        let _: () = unsafe { msg_send![w, toggleFullScreen: std::ptr::null_mut::<AnyObject>()] };
        *WANTED.lock().unwrap_or_else(|e| e.into_inner()) = Some((want, Instant::now()));
        want
    }

    /// The pointer on screen, in points.
    pub fn cursor() -> (i32, i32) {
        unsafe {
            let e = CGEventCreate(std::ptr::null());
            if e.is_null() { return (0, 0) }
            let p = CGEventGetLocation(e);
            CFRelease(e);
            (p.x as i32, p.y as i32)
        }
    }

    /// The main display's mode: its width in points and in pixels.
    fn display() -> Option<(f32, f32)> {
        unsafe {
            let mode = CGDisplayCopyDisplayMode(CGMainDisplayID());
            if mode.is_null() { return None }
            let size = (CGDisplayModeGetWidth(mode) as f32, CGDisplayModeGetPixelWidth(mode) as f32);
            CGDisplayModeRelease(mode);
            Some(size).filter(|(w, px)| *w > 0.0 && *px > 0.0)
        }
    }

    pub fn scale() -> f32 { display().map_or(2.0, |(w, px)| px / w) }

    pub fn screen_w() -> f32 { display().map_or(1920.0, |(w, _)| w) }

    pub mod chrome {
        use super::*;

        /// Keep the app window's `NSWindow`, from its content view.
        pub fn set_main(w: &rune::WindowHandle) {
            let view = w.with_raw_handle(|h| match h.as_raw() {
                rune::raw_window_handle::RawWindowHandle::AppKit(h) => Some(h.ns_view.as_ptr().cast::<AnyObject>()),
                _ => None,
            }).ok().flatten();
            if let Some(view) = view {
                let window: *mut AnyObject = unsafe { msg_send![view, window] };
                WINDOW.store(window, Ordering::Relaxed);
            }
        }

        pub fn set_waker(_: rune::Waker) {}
    }
}
