//! stremio-rune Setup: the installer window.
//!
//! Five steps (Welcome, License, Requirements, Install, Done) on a layered, borderless window
//! drawn with Direct2D, driven by mouse and keyboard. `--uninstall` (the copy in the install
//! folder, which the Apps list runs) shows Confirm, Removing and Done in the same window.
//! `--update` (the app's Update button) installs with no window and starts the new app.
//! What it does to the machine is in `install.rs`.

#![windows_subsystem = "windows"]

mod gfx;
mod install;

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_ESCAPE, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, GetWindowRect,
    LoadCursorW, LoadIconW, PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowPos,
    ShowWindow, TranslateMessage, HTCAPTION, HTCLIENT, IDC_ARROW, MSG, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW,
    WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_NCHITTEST, WM_TIMER,
    WNDCLASSW, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_POPUP,
};

use gfx::{icon, rect, Align, Gfx, Icon, Rect};
use install::{Failure, Phase, Shared, APP_VERSION, PRODUCT};

const CLASS: PCWSTR = w!("StremioRuneSetup");
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 33;

const WIN_W: f32 = 744.0;
const WIN_H: f32 = 600.0;
const WIN_R: f32 = 24.0;
const PAD_X: f32 = 72.0;
const PAD_TOP: f32 = 48.0;
const PAD_BOTTOM: f32 = 96.0;
const SHADOW_DY: f32 = 24.0;
const SHADOW_SIGMA: f32 = 30.0;
const TITLE_H: f32 = 64.0;
const RAIL_X: f32 = 24.0;
const RAIL_Y: f32 = 68.0;
const RAIL_W: f32 = 180.0;
const X: f32 = 224.0;
const W: f32 = 496.0;
const FOOTER_Y: f32 = 516.0;
const NOTE: Rect = rect(X, FOOTER_Y - 24.0 - 36.0, W, 36.0);
const CLOSE: Rect = rect(702.0, 23.0, 18.0, 18.0);

// The app's palette (ui.rs), as 0xRRGGBB: its background oklch(0.15 0.012 265), the settings
// switch's green and the sign-out red. Everything else is white at some alpha, as in the app.
const BG: u32 = 0x090B10;
const WHITE: u32 = 0xFFFFFF;
const BLACK: u32 = 0x000000;
const GREEN: u32 = 0x53BE70;
const RED: u32 = 0xF47B74;

const STEPS: [&str; 5] = ["Welcome", "License", "Requirements", "Install", "Done"];
const UN_STEPS: [&str; 3] = ["Confirm", "Removing", "Done"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Welcome,
    License,
    Requirements,
    Install,
    Done,
    // Uninstall mode.
    Confirm,
    Removing,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    Doc,
    Source,
    Download,
    Launch,
    Shortcut,
    DeleteData,
    Button(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    Exit,
    Next,
    Back,
    Recheck,
    Install,
    Uninstall,
    Finish,
    None,
}

struct App {
    hwnd: HWND,
    scale: f32,
    uninstall: bool,
    step: Step,
    focus: Target,
    dir: PathBuf,
    /// Version of the stremio-rune already installed, if any.
    existing: Option<String>,
    /// A newer stremio-rune than this Setup is installed: nothing to do, never downgrade.
    newer: Option<String>,
    /// [Stremio folder, libmpv, streaming server].
    stremio: [bool; 3],
    app_running: bool,
    /// The app was open when Install/Uninstall started, so "Close stremio-rune" is a task.
    closing_task: bool,
    run: Option<(Shared, Instant)>,
    phase: Phase,
    shown: f32,
    launch: bool,
    shortcut: bool,
    delete_data: bool,
    scroll: f32,
    doc_max: f32,
    spin: u32,
    hits: Vec<(Target, Rect)>,
    gfx: Option<Gfx>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

impl App {
    fn installing(&self) -> bool {
        matches!(self.step, Step::Install | Step::Removing) && !matches!(self.phase, Phase::Finished(_))
    }

    fn failure(&self) -> Option<Failure> {
        match (self.step, self.phase) {
            (Step::Install | Step::Removing, Phase::Finished(Err(f))) => Some(f),
            _ => None,
        }
    }

    fn stremio_ok(&self) -> bool {
        self.stremio.iter().all(|ok| *ok)
    }

    /// (label, action, primary, enabled), left to right.
    fn buttons(&self) -> Vec<(&'static str, Act, bool, bool)> {
        match self.step {
            Step::Welcome if self.newer.is_some() => vec![("Close", Act::Exit, true, true)],
            Step::Welcome => vec![("Cancel", Act::Exit, false, true), ("Next", Act::Next, true, true)],
            Step::License => vec![("Back", Act::Back, false, true), ("Next", Act::Next, true, true)],
            Step::Requirements if !self.stremio_ok() => vec![
                ("Back", Act::Back, false, true),
                ("Check again", Act::Recheck, true, true),
                ("Install", Act::None, false, false),
            ],
            Step::Requirements => {
                let label = if self.app_running { "Close and install" } else { "Install" };
                vec![("Back", Act::Back, false, true), (label, Act::Install, true, true)]
            }
            Step::Install if self.failure().is_some() => {
                vec![("Close", Act::Exit, false, true), ("Try again", Act::Install, true, true)]
            }
            Step::Removing if self.failure().is_some() => {
                vec![("Close", Act::Exit, false, true), ("Try again", Act::Uninstall, true, true)]
            }
            Step::Install | Step::Removing => vec![("Back", Act::None, false, false), ("Next", Act::None, true, false)],
            Step::Done => vec![("Finish", Act::Finish, true, true)],
            Step::Confirm => {
                let label = if self.app_running { "Close and uninstall" } else { "Uninstall" };
                vec![("Cancel", Act::Exit, false, true), (label, Act::Uninstall, true, true)]
            }
            Step::Removed => vec![("Close", Act::Exit, true, true)],
        }
    }

    fn targets(&self) -> Vec<Target> {
        let mut t = match self.step {
            Step::License => vec![Target::Doc, Target::Source],
            Step::Requirements if !self.stremio_ok() => vec![Target::Download],
            Step::Done => vec![Target::Launch, Target::Shortcut],
            Step::Confirm => vec![Target::DeleteData],
            _ => Vec::new(),
        };
        t.extend(self.buttons().iter().enumerate().filter(|(_, b)| b.3).map(|(i, _)| Target::Button(i)));
        t
    }

    fn default_focus(&self) -> Target {
        self.buttons()
            .iter()
            .position(|b| b.2 && b.3)
            .map(Target::Button)
            .or_else(|| self.targets().first().copied())
            .unwrap_or(Target::Button(0))
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        if matches!(step, Step::Requirements | Step::Confirm) {
            self.stremio = install::stremio_check();
            self.app_running = install::app_running(&self.dir);
        }
        self.scroll = 0.0;
        self.focus = self.default_focus();
    }

    fn move_focus(&mut self, forward: bool) {
        let order = self.targets();
        if order.is_empty() {
            return;
        }
        let i = order.iter().position(|t| *t == self.focus).unwrap_or(0);
        let n = order.len();
        self.focus = order[if forward { (i + 1) % n } else { (i + n - 1) % n }];
    }

    /// Up/Down on the license scroll it first and only leave it at the ends, like a TV list.
    fn vertical(&mut self, down: bool) {
        if self.focus == Target::Doc {
            let before = self.scroll;
            self.scroll_by(if down { 48.0 } else { -48.0 });
            if self.scroll != before {
                return;
            }
        }
        self.move_focus(down);
    }

    fn scroll_by(&mut self, dy: f32) {
        if self.step == Step::License {
            self.scroll = (self.scroll + dy).clamp(0.0, self.doc_max);
        }
    }

    /// Returns true when Setup should close.
    fn act(&mut self, act: Act) -> bool {
        match act {
            Act::Exit => return true,
            Act::Next => self.go(if self.step == Step::Welcome { Step::License } else { Step::Requirements }),
            Act::Back => self.go(if self.step == Step::Requirements { Step::License } else { Step::Welcome }),
            Act::Recheck => {
                self.go(Step::Requirements);
                if self.stremio_ok() {
                    // Straight to Install, the button that just appeared.
                    self.focus = self.default_focus();
                }
            }
            Act::Install | Act::Uninstall => {
                let uninstall = act == Act::Uninstall;
                self.closing_task = install::app_running(&self.dir);
                self.phase = Phase::Closing;
                self.shown = 0.0;
                let run = if uninstall {
                    install::start_uninstall(self.dir.clone(), self.delete_data)
                } else {
                    install::start_install(self.dir.clone())
                };
                self.run = Some((run, Instant::now()));
                self.go(if uninstall { Step::Removing } else { Step::Install });
            }
            Act::Finish => {
                self.apply_done(self.launch);
                return true;
            }
            Act::None => {}
        }
        false
    }

    fn apply_done(&self, launch: bool) {
        install::set_desktop_shortcut(&self.dir, self.shortcut);
        if launch {
            install::launch_app(&self.dir);
        }
    }

    fn press(&mut self, target: Target) -> bool {
        match target {
            Target::Doc => self.scroll_by(120.0),
            Target::Source => install::open_url(install::SOURCE_URL),
            Target::Download => install::open_url(install::STREMIO_URL),
            Target::Launch => self.launch = !self.launch,
            Target::Shortcut => self.shortcut = !self.shortcut,
            Target::DeleteData => self.delete_data = !self.delete_data,
            Target::Button(i) => {
                if let Some(&(_, act, _, true)) = self.buttons().get(i) {
                    return self.act(act);
                }
            }
        }
        false
    }

    /// Escape: one step back where that is safe, never mid-install.
    fn back(&mut self) -> bool {
        match self.step {
            Step::Welcome | Step::Confirm => true,
            Step::License => {
                self.go(Step::Welcome);
                false
            }
            Step::Requirements => {
                self.go(Step::License);
                false
            }
            Step::Install | Step::Removing => self.failure().is_some(),
            Step::Done | Step::Removed => false,
        }
    }

    /// The window's ✕ (and Alt+F4): cancel before installing, ignored while installing, Finish
    /// without starting the app afterwards.
    fn close_requested(&mut self) -> bool {
        match self.step {
            _ if self.installing() => false,
            Step::Done => {
                self.apply_done(false);
                true
            }
            _ => true,
        }
    }

    fn click(&mut self, x: f32, y: f32) -> bool {
        if CLOSE.inset(-7.0).contains(x, y) {
            return self.close_requested();
        }
        let hit = self.hits.iter().find(|(_, r)| r.contains(x, y)).map(|(t, _)| *t);
        match hit {
            Some(t) if self.targets().contains(&t) => {
                self.focus = t;
                self.press(t)
            }
            _ => false,
        }
    }

    fn key(&mut self, vk: u16) -> bool {
        match vk {
            v if v == VK_RETURN.0 || v == VK_SPACE.0 => return self.press(self.focus),
            v if v == VK_ESCAPE.0 => return self.back(),
            v if v == VK_UP.0 => self.vertical(false),
            v if v == VK_DOWN.0 => self.vertical(true),
            v if v == VK_LEFT.0 => self.move_focus(false),
            v if v == VK_RIGHT.0 || v == VK_TAB.0 => self.move_focus(true),
            v if v == VK_PRIOR.0 => self.scroll_by(-240.0),
            v if v == VK_NEXT.0 => self.scroll_by(240.0),
            _ => {}
        }
        false
    }

    /// Returns whether to repaint.
    fn tick(&mut self) -> bool {
        let mut dirty = false;
        if let Some((shared, _)) = &self.run {
            let phase = *shared.lock().unwrap();
            if phase != self.phase {
                self.phase = phase;
                dirty = true;
                match phase {
                    Phase::Finished(Ok(())) => {
                        self.run = None;
                        self.go(if self.uninstall { Step::Removed } else { Step::Done });
                    }
                    Phase::Finished(Err(_)) => {
                        self.run = None;
                        self.focus = self.default_focus();
                    }
                    _ => {}
                }
            }
        }
        let target = install::progress(self.phase, self.uninstall);
        if (target - self.shown).abs() > 0.001 {
            self.shown += (target - self.shown) * 0.18;
            dirty = true;
        }
        if self.installing() {
            dirty = true;
        }
        self.spin = self.spin.wrapping_add(1);
        // The app opening or closing changes the button label; checked about once a second.
        if matches!(self.step, Step::Requirements | Step::Confirm) && self.spin.is_multiple_of(30) {
            let running = install::app_running(&self.dir);
            if running != self.app_running {
                self.app_running = running;
                dirty = true;
            }
        }
        dirty
    }
}

fn surface_px(scale: f32) -> (i32, i32) {
    (((WIN_W + 2.0 * PAD_X) * scale).ceil() as i32, ((WIN_H + PAD_TOP + PAD_BOTTOM) * scale).ceil() as i32)
}

fn place(app: &App) {
    let s = app.scale;
    let (w, h) = surface_px(s);
    unsafe {
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(MonitorFromWindow(app.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
        let work = info.rcWork;
        let x = (work.left + work.right - (WIN_W * s) as i32) / 2 - (PAD_X * s) as i32;
        let y = (work.top + work.bottom - (WIN_H * s) as i32) / 2 - (PAD_TOP * s) as i32;
        let _ = SetWindowPos(app.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

fn paint(app: &mut App) {
    let mut g = match app.gfx.take() {
        Some(g) => g,
        None => match unsafe { Gfx::new() } {
            Ok(g) => g,
            Err(_) => return,
        },
    };
    let s = app.scale;
    unsafe {
        if g.begin(surface_px(s)).is_err() {
            return;
        }
        g.draw_shadow(rect(PAD_X * s, (PAD_TOP + SHADOW_DY) * s, WIN_W * s, WIN_H * s), WIN_R * s, SHADOW_SIGMA * s);
        g.transform(s, PAD_X * s, PAD_TOP * s);
        app.hits.clear();
        draw(app, &mut g);
        let _ = g.present(app.hwnd);
    }
    app.gfx = Some(g);
}

fn title_bar(uninstall: bool) -> String {
    if uninstall { format!("Uninstall {PRODUCT}") } else { format!("{PRODUCT} Setup") }
}

unsafe fn draw(app: &mut App, g: &mut Gfx) {
    let card = rect(0.0, 0.0, WIN_W, WIN_H);
    g.fill(card, WIN_R, BG, 1.0);
    g.ring(card, WIN_R, 1.0, WHITE, 0.12);

    // Title bar.
    g.logo(rect(24.0, 23.0, 18.0, 18.0), app.scale);
    g.text(&title_bar(app.uninstall), rect(52.0, 23.0, 300.0, 18.0), 15.0, 600, WHITE, 1.0, Align::Left);
    let pill = format!("v{APP_VERSION}");
    let pw = g.measure(&pill, 12.0, 500) + 24.0;
    let pr = rect(CLOSE.x - 14.0 - pw, 18.0, pw, 28.0);
    g.fill(pr, 14.0, WHITE, 0.08);
    g.text(&pill, pr, 12.0, 500, WHITE, 0.6, Align::Center);
    // The caption's ✕, thin as the app (and Windows) draw it.
    let x = CLOSE.inset(3.5);
    let a = if app.installing() { 0.25 } else { 0.8 };
    g.line((x.x, x.y), (x.x + x.w, x.y + x.h), 1.2, WHITE, a);
    g.line((x.x + x.w, x.y), (x.x, x.y + x.h), 1.2, WHITE, a);

    draw_rail(app, g);

    let (title, sub) = heading(app);
    g.text(&title, rect(X, 70.0, W, 32.0), 26.0, 700, WHITE, 1.0, Align::Left);
    let sub_h = g.paragraph(&sub, X, 108.0, W, 14.0, 500, WHITE, 0.6, true);
    let y = 108.0 + sub_h + 18.0;

    match app.step {
        Step::Welcome if app.newer.is_some() => {}
        Step::Welcome => {
            let path = app.dir.display().to_string();
            let badge = app.existing.as_ref().map(|v| format!("Installed v{v}"));
            let trailing = badge.as_deref().map_or(Trailing::None, Trailing::Badge);
            row(app, g, rect(X, y, W, 64.0), None, Some(&icon::FOLDER), "Install location", Some(&path), trailing);
            note(g, &icon::INFO, "Installs for you only. No administrator rights needed.", false);
        }
        Step::License => draw_license(app, g, y),
        Step::Requirements => draw_requirements(app, g, y),
        Step::Install | Step::Removing => draw_progress(app, g, y),
        Step::Done => {
            let mut ry = y;
            for (target, label, on) in [
                (Target::Launch, "Start stremio-rune now", app.launch),
                (Target::Shortcut, "Add a desktop shortcut", app.shortcut),
            ] {
                let r = rect(X, ry, W, 56.0);
                row(app, g, r, Some(target), None, label, None, Trailing::Toggle(on));
                ry += r.h + 8.0;
            }
        }
        Step::Confirm => {
            let caption = "Sign-in and settings in %APPDATA%\\stremio-rune.";
            let r = rect(X, y, W, 64.0);
            row(app, g, r, Some(Target::DeleteData), None, "Also remove my settings", Some(caption), Trailing::Toggle(app.delete_data));
            if app.app_running {
                note(g, &icon::WARNING, "stremio-rune is open. Uninstall closes it first.", true);
            } else {
                note(g, &icon::INFO, "Your library and watch progress stay in your Stremio account.", false);
            }
        }
        Step::Removed => {
            let text = if app.delete_data {
                "Your sign-in and settings were deleted from this PC."
            } else {
                "Your sign-in and settings were kept for a reinstall."
            };
            note(g, &icon::INFO, text, false);
        }
    }
    draw_footer(app, g);
}

fn heading(app: &App) -> (String, String) {
    let (title, sub): (&str, String) = match app.step {
        Step::Welcome => match &app.newer {
            Some(v) => (
                "Welcome to stremio-rune",
                format!("stremio-rune {v} is already installed. This Setup has the older version {APP_VERSION}, so there is nothing to install."),
            ),
            None => ("Welcome to stremio-rune", "A fast, native Windows app for Stremio, with your account, add-ons and library.".into()),
        },
        Step::License => ("License", "stremio-rune is free and open source under the MIT license: use it, copy it and change it as you like.".into()),
        Step::Requirements if app.stremio_ok() => (
            "Stremio is ready",
            "stremio-rune plays through the video player and streaming server that the Stremio app installs. Both are on this PC.".into(),
        ),
        Step::Requirements => (
            "Stremio is needed",
            "stremio-rune plays through the video player and streaming server that the Stremio app installs. Install Stremio first, then check again.".into(),
        ),
        Step::Install => match app.failure() {
            Some(Failure::Payload) => ("Setup couldn't finish", "This Setup file has no app inside. Download it again.".into()),
            Some(Failure::Files) => ("Setup couldn't finish", "A file couldn't be written. Close stremio-rune if it is open, then try again.".into()),
            None => ("Installing stremio-rune", "This takes a few seconds.".into()),
        },
        Step::Done => ("stremio-rune is ready", "Start it from the Start menu any time. Sign in with your Stremio account, or reuse the Stremio app's session.".into()),
        Step::Confirm => ("Uninstall stremio-rune?", "stremio-rune leaves this PC. The Stremio app and your account stay as they are.".into()),
        Step::Removing => match app.failure() {
            Some(_) => ("stremio-rune couldn't be removed", "Close stremio-rune if it is open and try again.".into()),
            None => ("Removing stremio-rune", "This takes a few seconds.".into()),
        },
        Step::Removed => ("stremio-rune was removed", "Thanks for watching. The Stremio app and your account are untouched.".into()),
    };
    (title.into(), sub)
}

/// The step list, styled like the app's nav: the current step is the white pill.
unsafe fn draw_rail(app: &App, g: &mut Gfx) {
    let (labels, cur): (&[&str], usize) = if app.uninstall {
        (&UN_STEPS, app.step as usize - Step::Confirm as usize)
    } else {
        (&STEPS, app.step as usize)
    };
    for (i, label) in labels.iter().enumerate() {
        let r = rect(RAIL_X, RAIL_Y + i as f32 * 48.0, RAIL_W, 44.0);
        let done = i < cur || matches!(app.step, Step::Done | Step::Removed);
        let current = i == cur && !done;
        if current {
            g.fill(r, 22.0, WHITE, 1.0);
        }
        let (mx, my) = (r.x + 25.0, r.y + 22.0);
        let marker = rect(mx - 11.0, my - 11.0, 22.0, 22.0);
        if done {
            g.circle(mx, my, 11.0, WHITE, 0.1);
            g.icon(&icon::CHECK, marker.inset(4.0), WHITE, 0.9);
        } else if current {
            g.circle(mx, my, 11.0, BLACK, 1.0);
            g.text(&(i + 1).to_string(), marker, 12.0, 600, WHITE, 1.0, Align::Center);
        } else {
            g.ring(marker, 11.0, 1.5, WHITE, 0.25);
            g.text(&(i + 1).to_string(), marker, 12.0, 500, WHITE, 0.5, Align::Center);
        }
        let (c, a, weight) = match (done, current) {
            (_, true) => (BLACK, 1.0, 600),
            (true, _) => (WHITE, 0.85, 500),
            _ => (WHITE, 0.5, 500),
        };
        g.text(label, rect(r.x + 48.0, r.y, r.w - 56.0, r.h), 15.0, weight, c, a, Align::Left);
    }
}

enum Trailing<'a> {
    None,
    Toggle(bool),
    Button(&'a str),
    /// Read-only state, e.g. "Found", in the app's outlined badge.
    Badge(&'a str),
    /// The same badge as a warning, e.g. "Missing".
    Missing(&'a str),
}

/// A glass tile (the app's settings line) with an optional icon, a label and caption, and a
/// trailing control. `target` makes it focusable and clickable.
#[allow(clippy::too_many_arguments)]
unsafe fn row(app: &mut App, g: &mut Gfx, r: Rect, target: Option<Target>, icon: Option<&Icon>, label: &str, caption: Option<&str>, trailing: Trailing) {
    let focused = target.is_some_and(|t| app.focus == t);
    glass(g, r, 14.0, focused);
    let cy = r.y + r.h / 2.0;
    let mut tx = r.x + 16.0;
    if let Some(icon) = icon {
        let b = rect(tx, cy - 16.0, 32.0, 32.0);
        g.fill(b, 10.0, WHITE, 0.08);
        g.icon(icon, b.inset(7.0), WHITE, 0.9);
        tx += 48.0;
    }
    let trail_w = match trailing {
        Trailing::None => 0.0,
        Trailing::Toggle(_) => 48.0 + 16.0,
        Trailing::Button(l) => g.measure(l, 15.0, 600) + 32.0 + 16.0,
        Trailing::Badge(l) | Trailing::Missing(l) => g.measure(l, 13.0, 600) + 14.0 + 16.0,
    };
    let tw = r.x + r.w - 16.0 - trail_w - tx;
    match caption {
        Some(c) => {
            g.text(label, rect(tx, cy - 20.0, tw, 20.0), 16.0, 500, WHITE, 1.0, Align::Left);
            g.text(c, rect(tx, cy + 2.0, tw, 18.0), 13.0, 500, WHITE, 0.55, Align::Left);
        }
        None => g.text(label, rect(tx, cy - 11.0, tw, 22.0), 16.0, 500, WHITE, 1.0, Align::Left),
    }
    let right = r.x + r.w - 16.0;
    match trailing {
        Trailing::None => {}
        Trailing::Toggle(on) => toggle(g, rect(right - 48.0, cy - 14.0, 48.0, 28.0), on),
        Trailing::Button(l) => {
            let bw = g.measure(l, 15.0, 600) + 32.0;
            let b = rect(right - bw, cy - 18.0, bw, 36.0);
            g.fill(b, 18.0, WHITE, 0.08);
            g.text(l, b, 15.0, 600, WHITE, 1.0, Align::Center);
        }
        Trailing::Badge(l) | Trailing::Missing(l) => {
            let (c, border) = if matches!(trailing, Trailing::Badge(_)) { (WHITE, 0.45) } else { (RED, 0.8) };
            let bw = g.measure(l, 13.0, 600) + 14.0;
            let b = rect(right - bw, cy - 12.0, bw, 24.0);
            g.ring(b, 5.0, 1.0, c, border);
            g.text(l, b, 13.0, 600, c, 0.9, Align::Center);
        }
    }
    if let Some(t) = target {
        app.hits.push((t, r));
    }
}

/// The app's frosted panel: a faint white wash and hairline. Focus (keyboard) lifts the wash and
/// adds a white ring.
unsafe fn glass(g: &mut Gfx, r: Rect, radius: f32, focused: bool) {
    g.fill(r, radius, WHITE, if focused { 0.14 } else { 0.07 });
    if focused {
        g.ring(r, radius, 2.0, WHITE, 0.9);
    } else {
        g.ring(r, radius, 1.0, WHITE, 0.12);
    }
}

/// The app's settings switch: green when on.
unsafe fn toggle(g: &mut Gfx, tr: Rect, on: bool) {
    if on {
        g.fill(tr, 14.0, GREEN, 1.0);
    } else {
        g.fill(tr, 14.0, WHITE, 0.18);
    }
    g.circle(tr.x + if on { 34.0 } else { 14.0 }, tr.y + 14.0, 11.0, WHITE, 1.0);
}

unsafe fn note(g: &mut Gfx, icon: &Icon, text: &str, warn: bool) {
    let n = NOTE;
    g.ring(n, 12.0, 1.0, WHITE, 0.12);
    let (c, a) = if warn { (RED, 1.0) } else { (WHITE, 0.6) };
    g.icon(icon, rect(n.x + 14.0, n.y + 10.0, 16.0, 16.0), c, a);
    g.text(text, rect(n.x + 40.0, n.y, n.w - 54.0, n.h), 13.0, 500, WHITE, 0.6, Align::Left);
}

unsafe fn draw_license(app: &mut App, g: &mut Gfx, y: f32) {
    let links_y = FOOTER_Y - 24.0 - 32.0;
    let panel = rect(X, y, W, links_y - 14.0 - y);
    let focused = app.focus == Target::Doc;
    glass(g, panel, 14.0, focused);
    let view = panel.inset(16.0);
    let text_w = view.w - 16.0;
    // LICENSE is hard-wrapped at 80 columns; rejoined into paragraphs it wraps to the panel.
    let license = install::LICENSE.replace("\r\n", "\n").split("\n\n").map(|p| p.replace('\n', " ")).collect::<Vec<_>>().join("\n\n");
    let doc_h = g.paragraph(&license, 0.0, 0.0, text_w, 13.0, 400, WHITE, 1.0, false);
    app.doc_max = (doc_h - view.h).max(0.0);
    app.scroll = app.scroll.clamp(0.0, app.doc_max);
    g.clip(view);
    g.paragraph(&license, view.x, view.y - app.scroll, text_w, 13.0, 400, WHITE, 0.62, true);
    g.unclip();
    if app.doc_max > 0.0 {
        let track = rect(panel.x + panel.w - 10.0, view.y, 4.0, view.h);
        g.fill(track, 2.0, WHITE, 0.06);
        let th = (track.h * view.h / doc_h).max(24.0);
        let ty = track.y + (track.h - th) * app.scroll / app.doc_max;
        g.fill(rect(track.x, ty, 4.0, th), 2.0, WHITE, if focused { 0.5 } else { 0.3 });
    }
    app.hits.push((Target::Doc, panel));

    // A chip like the app's, opening the repository.
    let label = "Source code";
    let tw = g.measure(label, 14.0, 600);
    let r = rect(X, links_y, tw + 52.0, 32.0);
    g.fill(r, 16.0, WHITE, if app.focus == Target::Source { 0.14 } else { 0.08 });
    if app.focus == Target::Source {
        g.ring(r.inset(-3.0), 19.0, 2.0, WHITE, 0.9);
    }
    g.text(label, rect(r.x + 14.0, r.y, tw + 2.0, r.h), 14.0, 600, WHITE, 1.0, Align::Left);
    g.icon(&icon::OPEN, rect(r.x + 20.0 + tw, r.y + 9.0, 14.0, 14.0), WHITE, 0.7);
    app.hits.push((Target::Source, r));
}

unsafe fn draw_requirements(app: &mut App, g: &mut Gfx, y: f32) {
    let stremio = install::stremio_dir().display().to_string();
    let checks = [
        ("Stremio app", stremio.as_str()),
        ("Video player", "libmpv-2.dll"),
        ("Streaming server", "stremio-runtime.exe"),
    ];
    let mut ry = y;
    for ((label, caption), ok) in checks.into_iter().zip(app.stremio) {
        let trailing = if ok { Trailing::Badge("Found") } else { Trailing::Missing("Missing") };
        row(app, g, rect(X, ry, W, 59.0), None, None, label, Some(caption), trailing);
        ry += 59.0 + 6.0;
    }
    if !app.stremio_ok() {
        let r = rect(X, ry + 8.0, W, 59.0);
        row(app, g, r, Some(Target::Download), Some(&icon::DOWNLOAD), "Get Stremio", Some("stremio.com/downloads"), Trailing::Button("Download"));
        note(g, &icon::WARNING, "Install Stremio and open it once, then press Check again.", true);
    } else if app.app_running {
        note(g, &icon::WARNING, "stremio-rune is open. Setup closes it before installing.", true);
    } else {
        note(g, &icon::INFO, "Your Stremio account, add-ons and library carry over.", false);
    }
}

unsafe fn draw_progress(app: &mut App, g: &mut Gfx, y: f32) {
    if app.failure().is_some() {
        return;
    }
    let status = match (app.phase, app.uninstall) {
        (Phase::Closing, _) => "Closing stremio-rune…",
        (Phase::Files, false) => "Copying stremio-rune…",
        (Phase::Files, true) => "Removing app files…",
        (Phase::Shortcuts, false) => "Creating shortcuts…",
        (Phase::Shortcuts, true) => "Removing shortcuts…",
        (Phase::Register, _) => "Adding stremio-rune to Apps…",
        (Phase::Data, _) => "Deleting your settings…",
        (Phase::Finished(_), _) => "Finishing up…",
    };
    progress_bar(app, g, y, status);
    let order = [Phase::Closing, Phase::Files, Phase::Shortcuts, Phase::Register, Phase::Data];
    let now = order.iter().position(|p| *p == app.phase).unwrap_or(order.len());
    let mut tasks = Vec::new();
    if app.closing_task {
        tasks.push(("Close stremio-rune", 0));
    }
    if app.uninstall {
        tasks.push(("App files", 1));
        tasks.push(("Shortcuts and the Apps entry", 2));
        if app.delete_data {
            tasks.push(("Your settings", 4));
        }
    } else {
        tasks.push(("stremio-rune app", 1));
        tasks.push(("Start menu shortcut", 2));
        tasks.push(("Apps list entry", 3));
    }
    task_list(app, g, y, &tasks, now);
}

/// The app's thin white progress bar, with a soft shimmer running along the filled part.
unsafe fn progress_bar(app: &App, g: &mut Gfx, y: f32, status: &str) {
    g.text(status, rect(X, y, W - 60.0, 20.0), 14.0, 500, WHITE, 1.0, Align::Left);
    g.text(&format!("{} %", (app.shown * 100.0).round() as i32), rect(X + W - 60.0, y, 60.0, 20.0), 14.0, 500, WHITE, 0.5, Align::Right);
    let track = rect(X, y + 31.0, W, 4.0);
    g.fill(track, 2.0, WHITE, 0.28);
    let bar = rect(X, track.y, (W * app.shown).max(4.0), 4.0);
    g.fill(bar, 2.0, WHITE, 1.0);
    g.clip(bar);
    let travel = bar.w + 90.0;
    let sx = bar.x - 90.0 + (app.spin as f32 * 7.0) % travel;
    for (i, a) in [0.05f32, 0.1, 0.15, 0.1, 0.05].iter().enumerate() {
        g.fill(rect(sx + i as f32 * 18.0, bar.y, 18.0, bar.h), 0.0, BG, *a);
    }
    g.unclip();
}

/// One row per task: done (check), current (spinner) or pending (ring). `now` indexes the phase order.
unsafe fn task_list(app: &App, g: &mut Gfx, y: f32, tasks: &[(&str, usize)], now: usize) {
    let mut ty = y + 58.0;
    for &(label, idx) in tasks {
        let (cx, cy) = (X + 25.0, ty + 16.0);
        let (a, weight) = if idx < now {
            g.circle(cx, cy, 10.0, WHITE, 0.1);
            g.icon(&icon::CHECK, rect(cx - 7.0, cy - 7.0, 14.0, 14.0), WHITE, 0.9);
            (1.0, 500)
        } else if idx == now {
            g.spinner(cx, cy, 7.5, (app.spin as f32 * 0.04) % 1.0, WHITE);
            (1.0, 600)
        } else {
            g.ring(rect(cx - 9.0, cy - 9.0, 18.0, 18.0), 9.0, 1.5, WHITE, 0.25);
            (0.5, 500)
        };
        g.text(label, rect(X + 46.0, ty, W - 62.0, 32.0), 15.0, weight, WHITE, a, Align::Left);
        ty += 36.0;
    }
}

unsafe fn draw_footer(app: &mut App, g: &mut Gfx) {
    g.fill(rect(0.0, FOOTER_Y, WIN_W, 1.0), 0.0, WHITE, 0.1);
    let iy = FOOTER_Y + 16.0 + 11.0;
    let dir = app.dir.display().to_string();
    let (ic, text) = match app.step {
        Step::License => (&icon::DOCUMENT, "MIT License · © 2026 prodBirdy".to_string()),
        Step::Install | Step::Removing => (&icon::DRIVE, dir),
        Step::Done => (&icon::CHECK, format!("Installed in {dir}")),
        Step::Confirm => {
            let version = install::installed_version().unwrap_or_else(|| APP_VERSION.into());
            (&icon::DRIVE, format!("{PRODUCT} {version} · {dir}"))
        }
        Step::Removed => (&icon::CHECK, format!("Removed from {dir}")),
        Step::Welcome | Step::Requirements => match install::free_space(&app.dir) {
            Some((gb, drive)) => (&icon::DRIVE, format!("{gb} GB free on {drive}")),
            None => (&icon::DRIVE, String::new()),
        },
    };
    let buttons = app.buttons();
    let mut x = WIN_W - 24.0;
    let mut rects: Vec<Rect> = buttons
        .iter()
        .rev()
        .map(|(label, ..)| {
            let w = g.measure(label, 16.0, 600).ceil() + 52.0;
            x -= w;
            let r = rect(x, FOOTER_Y + 16.0, w, 44.0);
            x -= 10.0;
            r
        })
        .collect();
    rects.reverse();
    g.icon(ic, rect(24.0, iy + 3.0, 16.0, 16.0), WHITE, 0.5);
    g.text(&text, rect(48.0, iy, rects[0].x - 16.0 - 48.0, 22.0), 13.0, 500, WHITE, 0.5, Align::Left);
    // The app's buttons: white with black text for the main action (its Sign in), a faint
    // wash for the rest.
    for (i, ((label, _, primary, enabled), r)) in buttons.into_iter().zip(rects).enumerate() {
        let a = if enabled { 1.0 } else { 0.4 };
        if primary {
            g.fill(r, 12.0, WHITE, a);
        } else {
            g.fill(r, 12.0, WHITE, 0.08 * a);
        }
        if app.focus == Target::Button(i) && enabled {
            g.ring(r.inset(-3.0), 15.0, 2.0, WHITE, 0.9);
        }
        g.text(label, r, 16.0, 600, if primary { BLACK } else { WHITE }, a, Align::Center);
        app.hits.push((Target::Button(i), r));
    }
}


fn design_point(app: &App, lparam: LPARAM) -> (f32, f32) {
    let x = (lparam.0 & 0xffff) as i16 as f32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as f32;
    (x / app.scale - PAD_X, y / app.scale - PAD_TOP)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handle = |f: &dyn Fn(&mut App) -> bool| {
        let close = with_app(|app| {
            let close = f(app);
            if !close {
                paint(app);
            }
            close
        });
        if close == Some(true) {
            let _ = DestroyWindow(hwnd);
        }
        LRESULT(0)
    };
    match msg {
        WM_NCHITTEST => {
            let mut wr = RECT::default();
            let _ = GetWindowRect(hwnd, &mut wr);
            let sx = (lparam.0 & 0xffff) as i16 as i32 - wr.left;
            let sy = ((lparam.0 >> 16) & 0xffff) as i16 as i32 - wr.top;
            let caption = with_app(|app| {
                let (x, y) = design_point(app, LPARAM(((sy as isize) << 16) | (sx as isize & 0xffff)));
                (0.0..WIN_W).contains(&x) && (0.0..TITLE_H).contains(&y) && !CLOSE.inset(-7.0).contains(x, y)
            })
            .unwrap_or(false);
            LRESULT(if caption { HTCAPTION } else { HTCLIENT } as isize)
        }
        WM_LBUTTONDOWN => handle(&|app| {
            let (x, y) = design_point(app, lparam);
            app.click(x, y)
        }),
        WM_MOUSEWHEEL => handle(&|app| {
            let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32;
            app.scroll_by(-delta / 120.0 * 48.0);
            false
        }),
        WM_KEYDOWN => handle(&|app| app.key(wparam.0 as u16)),
        WM_TIMER if wparam.0 == TIMER_ID => {
            with_app(|app| {
                if app.tick() {
                    paint(app);
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => handle(&|app| app.close_requested()),
        WM_DPICHANGED => handle(&|app| {
            app.scale = ((wparam.0 >> 16) & 0xffff).max(96) as f32 / 96.0;
            place(app);
            false
        }),
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    let uninstall = std::env::args().any(|a| a == "--uninstall");
    // The uninstaller first hands over to a temp copy of itself (see `install::uninstall_dir`).
    let dir = if uninstall {
        match install::uninstall_dir() {
            Some(dir) => dir,
            None => return,
        }
    } else {
        install::default_dir()
    };
    unsafe {
        // One Setup at a time: two runs into the same folder would trip over each other.
        let _mutex = CreateMutexW(None, true, w!(r"Local\stremio-rune.Setup"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(other) = FindWindowW(CLASS, PCWSTR::null()) {
                let _ = ShowWindow(other, SW_SHOW);
                let _ = SetForegroundWindow(other);
            }
            return;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // `--update`, from the app's Update: no window unless it fails, then Setup as usual.
        if !uninstall && std::env::args().any(|a| a == "--update") && install::update(&dir) {
            return;
        }
        let instance = GetModuleHandleW(None).expect("module handle");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(Some(instance.into()), PCWSTR(1 as _)).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_APPWINDOW,
            CLASS,
            &HSTRING::from(title_bar(uninstall)),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        let existing = install::installed_version();
        let mut app = App {
            hwnd,
            scale: GetDpiForWindow(hwnd).max(96) as f32 / 96.0,
            uninstall,
            step: Step::Welcome,
            focus: Target::Button(1),
            dir,
            newer: existing.clone().filter(|v| install::is_newer(v, APP_VERSION)),
            // An upgrade keeps the desktop shortcut as it was; a first install offers one.
            shortcut: existing.is_none() || install::desktop_link().exists(),
            existing,
            stremio: [false; 3],
            app_running: false,
            closing_task: false,
            run: None,
            phase: Phase::Closing,
            shown: 0.0,
            launch: true,
            delete_data: false,
            scroll: 0.0,
            doc_max: 0.0,
            spin: 0,
            hits: Vec::new(),
            gfx: None,
        };
        app.go(if uninstall { Step::Confirm } else { Step::Welcome });
        place(&app);
        paint(&mut app);
        APP.with(|a| *a.borrow_mut() = Some(app));
        let _ = SetTimer(Some(hwnd), TIMER_ID, TIMER_MS, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // The uninstaller's temp copy removes itself once the window closes.
    if uninstall && std::env::current_exe().is_ok_and(|me| me.starts_with(std::env::temp_dir())) {
        install::delete_self_later();
    }
}
