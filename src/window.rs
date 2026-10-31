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
        let hwnd = GetAncestor(GetActiveWindow(), GA_ROOT);
        if hwnd.is_null() { return saved.is_some() }
        if let Some((style, place)) = saved.take() {
            SetWindowLongPtrW(hwnd, GWL_STYLE, style);
            SetWindowPlacement(hwnd, &place);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED);
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
        *saved = Some((style, place));
        true
    }
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

/// The floating controls: a transparent Rune window owned by the app window, so it stays above
/// it (and mpv's child window), hides with it when minimized, and gets no taskbar entry. The
/// app window's moves and resizes, fullscreen included, carry it along.
#[cfg(windows)]
pub mod overlay {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWLP_HWNDPARENT, GetClientRect, GetForegroundWindow, GetWindowLongPtrW, SWP_NOACTIVATE, SWP_NOZORDER,
        SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, WM_WINDOWPOSCHANGED, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };

    /// The app window, from `App::on_window`.
    static MAIN: AtomicIsize = AtomicIsize::new(0);

    pub fn set_main(w: &rune::WindowHandle) {
        if let Some(h) = hwnd(w) { MAIN.store(h as isize, Ordering::Relaxed) }
    }

    fn main() -> HWND { MAIN.load(Ordering::Relaxed) as HWND }

    /// Show the controls' window over the app window, or hide it.
    pub fn show(w: &rune::WindowHandle, on: bool) {
        if let (true, Some(o)) = (on, hwnd(w)) { if !main().is_null() { attach(main(), o) } }
        w.set_visible(on);
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

    /// Own `overlay` by `main` (the app window) and keep it over `main`'s client area.
    /// Idempotent; call on the UI thread while `overlay` is hidden (the style change needs it).
    pub fn attach(main: HWND, overlay: HWND) {
        unsafe {
            SetWindowLongPtrW(overlay, GWLP_HWNDPARENT, main as isize);
            let ex = GetWindowLongPtrW(overlay, GWL_EXSTYLE);
            SetWindowLongPtrW(overlay, GWL_EXSTYLE, (ex | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize) & !(WS_EX_APPWINDOW as isize));
            SetWindowSubclass(main, Some(follow), 0x5354, overlay as usize); // same id: replaces, no stacking
        }
        place(main, overlay);
    }

    fn place(main: HWND, overlay: HWND) {
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(main, &mut r);
            let mut at = POINT { x: 0, y: 0 };
            ClientToScreen(main, &mut at);
            SetWindowPos(overlay, std::ptr::null_mut(), at.x, at.y, r.right, r.bottom, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    unsafe extern "system" fn follow(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, overlay: usize) -> LRESULT {
        if msg == WM_WINDOWPOSCHANGED { place(hwnd, overlay as HWND) }
        unsafe { DefSubclassProc(hwnd, msg, wp, lp) }
    }
}

#[cfg(not(windows))]
pub mod overlay {
    pub fn set_main(_: &rune::WindowHandle) {}
    pub fn show(w: &rune::WindowHandle, on: bool) { w.set_visible(on) }
    pub fn keep_keys(_: &rune::WindowHandle) {}
}
