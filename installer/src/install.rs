//! Everything Setup does to the machine, all per user (no admin): the app exe and the uninstaller
//! in `%LOCALAPPDATA%\Programs\stremio-rune`, Start-menu and desktop shortcuts, and the HKCU
//! "Apps" entry. Install and uninstall run on a worker thread and report their phase.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

const PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/payload.bin"));
pub const APP_VERSION: &str = env!("APP_VERSION");
pub const PRODUCT: &str = "stremio-rune";
const EXE: &str = "stremio-rune.exe";
pub const UNINSTALLER: &str = "uninstall.exe";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\stremio-rune";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const STREMIO_URL: &str = "https://www.stremio.com/downloads";
pub const SOURCE_URL: &str = "https://github.com/prodBirdy/stremio-rune";
pub const LICENSE: &str = include_str!("../../LICENSE");

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Failure {
    /// Dev build: no app inside.
    Payload,
    /// A file couldn't be written or removed (still in use, antivirus, full drive).
    Files,
}

/// Install: Closing → Files → Shortcuts → Register. Uninstall: Closing → Files → Shortcuts
/// (shortcuts and the Apps entry) → Data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Closing,
    Files,
    Shortcuts,
    Register,
    Data,
    Finished(Result<(), Failure>),
}

pub type Shared = Arc<Mutex<Phase>>;

fn env_dir(var: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(var).unwrap_or_default())
}

pub fn default_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join("Programs").join(PRODUCT)
}

pub fn data_dir() -> PathBuf {
    env_dir("APPDATA").join(PRODUCT)
}

pub fn stremio_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").join(r"Programs\Stremio")
}

/// What the app needs from Stremio: (Stremio folder, libmpv, streaming server) present.
pub fn stremio_check() -> [bool; 3] {
    let dir = stremio_dir();
    [dir.is_dir(), dir.join("libmpv-2.dll").is_file(), dir.join("stremio-runtime.exe").is_file()]
}

fn start_menu_link() -> PathBuf {
    env_dir("APPDATA").join(r"Microsoft\Windows\Start Menu\Programs").join(format!("{PRODUCT}.lnk"))
}

pub fn desktop_link() -> PathBuf {
    env_dir("USERPROFILE").join("Desktop").join(format!("{PRODUCT}.lnk"))
}

fn reg_string(value: &str) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(UNINSTALL_KEY),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..len])).filter(|s| !s.is_empty())
}

pub fn installed_version() -> Option<String> {
    reg_string("DisplayVersion")
}

/// Semver ordering: numeric `major.minor.patch`; a pre-release (`-alpha.2`) sorts below its
/// release, and pre-release parts compare numerically when both are numbers.
pub fn is_newer(installed: &str, setup: &str) -> bool {
    use std::cmp::Ordering;
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        let mut nums: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
        nums.resize(3, 0);
        (nums, pre)
    }
    let ((a, ap), (b, bp)) = (split(installed), split(setup));
    let order = a.cmp(&b).then_with(|| match (ap, bp) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let (mut xs, mut ys) = (x.split('.'), y.split('.'));
            loop {
                match (xs.next(), ys.next()) {
                    (None, None) => break Ordering::Equal,
                    (None, Some(_)) => break Ordering::Less,
                    (Some(_), None) => break Ordering::Greater,
                    (Some(p), Some(q)) => {
                        let o = match (p.parse::<u64>(), q.parse::<u64>()) {
                            (Ok(m), Ok(n)) => m.cmp(&n),
                            _ => p.cmp(q),
                        };
                        if o != Ordering::Equal {
                            break o;
                        }
                    }
                }
            }
        }
    });
    order == Ordering::Greater
}

/// Process ids of the installed app running from `dir` (a copy elsewhere, e.g. a dev build,
/// locks nothing here and is left alone).
fn app_pids(dir: &Path) -> Vec<u32> {
    let exe = dir.join(EXE);
    let mut pids = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(EXE)
                && process_path(e.th32ProcessID).is_some_and(|p| same_file(&p, &exe))
            {
                pids.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    pids
}

fn process_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

pub fn app_running(dir: &Path) -> bool {
    !app_pids(dir).is_empty()
}

/// Asks the app to close (WM_CLOSE, so it stops its streaming server), then forces it after 5 s.
fn close_app(dir: &Path) {
    let taskkill = |force: bool| {
        for pid in app_pids(dir) {
            let mut c = Command::new("taskkill.exe");
            if force {
                c.arg("/F");
            }
            let _ = c.args(["/PID".to_string(), pid.to_string()]).creation_flags(CREATE_NO_WINDOW).status();
        }
    };
    if !app_running(dir) {
        return;
    }
    taskkill(false);
    let since = Instant::now();
    while app_running(dir) && since.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(200));
    }
    if app_running(dir) {
        taskkill(true);
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Retries a file operation for a moment: a just-closed exe stays locked briefly.
fn retry(mut f: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut last = Ok(());
    for _ in 0..20 {
        last = f();
        if last.is_ok() {
            return last;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    last
}

fn shortcut(link: &Path, target: &Path) -> windows::core::Result<()> {
    unsafe {
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        sl.SetPath(&HSTRING::from(target.as_os_str()))?;
        if let Some(dir) = target.parent() {
            sl.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()))?;
        }
        sl.SetDescription(&HSTRING::from("Stremio client for Windows"))?;
        sl.cast::<IPersistFile>()?.Save(&HSTRING::from(link.as_os_str()), true)
    }
}

/// Re-creates or removes the desktop shortcut to match the Done screen's switch.
pub fn set_desktop_shortcut(dir: &Path, on: bool) {
    if on {
        let _ = shortcut(&desktop_link(), &dir.join(EXE));
    } else {
        let _ = std::fs::remove_file(desktop_link());
    }
}

fn register(dir: &Path) -> windows::core::Result<()> {
    let exe = dir.join(EXE).display().to_string();
    let size_kb = (PAYLOAD.len() + std::fs::metadata(dir.join(UNINSTALLER)).map_or(0, |m| m.len() as usize)) / 1024;
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None).ok()?;
        let set_sz = |name: &str, value: &str| {
            let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
            let bytes = std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2);
            RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(bytes))
        };
        let set_dword = |name: &str, value: u32| RegSetValueExW(key, &HSTRING::from(name), None, REG_DWORD, Some(&value.to_le_bytes()));
        let results = [
            set_sz("DisplayName", PRODUCT),
            set_sz("DisplayIcon", &format!("{exe},0")),
            set_sz("DisplayVersion", APP_VERSION),
            set_sz("Publisher", "prodBirdy"),
            set_sz("InstallLocation", &dir.display().to_string()),
            set_sz("UninstallString", &format!("\"{}\" --uninstall", dir.join(UNINSTALLER).display())),
            set_sz("URLInfoAbout", SOURCE_URL),
            set_dword("EstimatedSize", size_kb as u32),
            set_dword("NoModify", 1),
            set_dword("NoRepair", 1),
        ];
        let _ = RegCloseKey(key);
        results.into_iter().find(|r| *r != ERROR_SUCCESS).map_or(Ok(()), |r| r.ok())
    }
}

/// Phase changes are spaced out a little so each task row is seen ticking off.
/// A cosmetic pause, the real work takes well under a second.
fn set_after(out: &Shared, p: Phase, since: &mut Instant) {
    let min = Duration::from_millis(350);
    if let Some(wait) = min.checked_sub(since.elapsed()) {
        std::thread::sleep(wait);
    }
    *out.lock().unwrap() = p;
    *since = Instant::now();
}

pub fn start_install(dir: PathBuf) -> Shared {
    let shared: Shared = Arc::new(Mutex::new(Phase::Closing));
    let out = shared.clone();
    std::thread::spawn(move || {
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let mut t = Instant::now();
        let result = (|| {
            if PAYLOAD.is_empty() {
                return Err(Failure::Payload);
            }
            close_app(&dir);
            set_after(&out, Phase::Files, &mut t);
            std::fs::create_dir_all(&dir).map_err(|_| Failure::Files)?;
            retry(|| std::fs::write(dir.join(EXE), PAYLOAD)).map_err(|_| Failure::Files)?;
            // The uninstaller is this Setup itself; skip when it's the one running (re-run from there).
            let me = std::env::current_exe().map_err(|_| Failure::Files)?;
            let uninstaller = dir.join(UNINSTALLER);
            if !same_file(&me, &uninstaller) {
                retry(|| std::fs::copy(&me, &uninstaller).map(drop)).map_err(|_| Failure::Files)?;
            }
            set_after(&out, Phase::Shortcuts, &mut t);
            shortcut(&start_menu_link(), &dir.join(EXE)).map_err(|_| Failure::Files)?;
            set_after(&out, Phase::Register, &mut t);
            register(&dir).map_err(|_| Failure::Files)?;
            Ok(())
        })();
        set_after(&out, Phase::Finished(result), &mut t);
    });
    shared
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

pub fn start_uninstall(dir: PathBuf, delete_data: bool) -> Shared {
    let shared: Shared = Arc::new(Mutex::new(Phase::Closing));
    let out = shared.clone();
    std::thread::spawn(move || {
        let mut t = Instant::now();
        let result = (|| {
            close_app(&dir);
            set_after(&out, Phase::Files, &mut t);
            for f in [EXE, UNINSTALLER] {
                let path = dir.join(f);
                if path.exists() {
                    retry(|| std::fs::remove_file(&path)).map_err(|_| Failure::Files)?;
                }
            }
            // Only goes if now empty, so anything else put there stays.
            let _ = std::fs::remove_dir(&dir);
            set_after(&out, Phase::Shortcuts, &mut t);
            let _ = std::fs::remove_file(start_menu_link());
            let _ = std::fs::remove_file(desktop_link());
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL_KEY)) };
            if delete_data {
                set_after(&out, Phase::Data, &mut t);
                let data = data_dir();
                if data.exists() {
                    std::fs::remove_dir_all(&data).map_err(|_| Failure::Files)?;
                }
            }
            Ok(())
        })();
        set_after(&out, Phase::Finished(result), &mut t);
    });
    shared
}

/// Bar position per phase, eased toward the next one.
pub fn progress(phase: Phase, uninstall: bool) -> f32 {
    match (phase, uninstall) {
        (Phase::Closing, _) => 0.08,
        (Phase::Files, false) => 0.45,
        (Phase::Shortcuts, false) => 0.75,
        (Phase::Register, _) => 0.92,
        (Phase::Files, true) => 0.4,
        (Phase::Shortcuts, true) => 0.7,
        (Phase::Data, _) => 0.9,
        (Phase::Finished(_), _) => 1.0,
    }
}

/// Free space on the drive of `dir` as (`"12.3"` GB, `"C:"`).
pub fn free_space(dir: &Path) -> Option<(String, String)> {
    let drive: String = dir.to_string_lossy().chars().take(2).collect();
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(format!("{drive}\\")), Some(&mut free), None, None) }.ok()?;
    Some((format!("{:.1}", free as f64 / 1_073_741_824.0), drive))
}

pub fn launch_app(dir: &Path) {
    let _ = Command::new(dir.join(EXE)).current_dir(dir).spawn();
}

pub fn open_url(url: &str) {
    let _ = Command::new("explorer.exe").arg(url).spawn();
}

/// The uninstaller lives in the folder it removes, and a running exe can't delete itself: it
/// hands over to a temp copy (`--dir=` the folder). Returns the folder to remove, or None once
/// the copy has taken over.
pub fn uninstall_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::args().find_map(|a| a.strip_prefix("--dir=").map(PathBuf::from)) {
        return Some(dir);
    }
    let me = std::env::current_exe().ok()?;
    let dir = me.parent()?.to_path_buf();
    let copy = std::env::temp_dir().join(format!("{PRODUCT}-uninstall-{}.exe", std::process::id()));
    if std::fs::copy(&me, &copy).is_ok()
        && Command::new(&copy).args(["--uninstall".to_string(), format!("--dir={}", dir.display())]).spawn().is_ok()
    {
        return None;
    }
    Some(dir)
}

/// A running exe cannot delete itself, so a hidden cmd waits a moment and removes it.
pub fn delete_self_later() {
    let Ok(me) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new("cmd.exe")
        .raw_arg(format!("/c ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", me.display()))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_order() {
        assert!(is_newer("1.5.0", "1.4.2"));
        assert!(is_newer("1.4.10", "1.4.9"));
        assert!(!is_newer("1.4.2", "1.4.2"));
        assert!(!is_newer("0.1.1", "0.1.2-alpha"));
        assert!(is_newer("0.1.2", "0.1.2-alpha"));
        assert!(is_newer("0.1.2-alpha.10", "0.1.2-alpha.9"));
        assert!(!is_newer("0.1.2-alpha", "0.1.2"));
    }
}
