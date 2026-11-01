//! Stremio's streaming server: torrents play through it at 127.0.0.1:11470. On launch the app
//! starts it hidden from the Stremio install when nothing is listening there yet; one started
//! here ends with the app (Windows: a kill-on-close job object; macOS: a shell that watches the
//! app's process). One already running (the Stremio app's) is used as is.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Where the Stremio install keeps its streaming server and libmpv: `%LOCALAPPDATA%\Programs\Stremio`
/// on Windows, `/Applications/Stremio.app/Contents/MacOS` on macOS (where build.rs points libmpv's
/// own libraries).
pub fn stremio_dir() -> PathBuf {
    #[cfg(windows)]
    return PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join(r"Programs\Stremio");
    #[cfg(target_os = "macos")]
    return PathBuf::from("/Applications/Stremio.app/Contents/MacOS");
    // stremio-linux-shell (Arch) and the Flatpak-less packages put server.js here, and run it on the system node.
    #[cfg(target_os = "linux")]
    PathBuf::from("/usr/share/stremio")
}

pub fn running() -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], 11470)), Duration::from_millis(300)).is_ok()
}

/// Start the server unless it's up. Blocks briefly (a port check); call off the UI thread.
pub fn ensure() -> Result<(), String> {
    if running() { return Ok(()) }
    let dir = stremio_dir();
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new(dir.join("stremio-runtime.exe"));
        cmd.arg(dir.join("server.js")).creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    };
    // The server under a shell that stops it once this process is gone, however it ends.
    #[cfg(not(windows))]
    let mut cmd = {
        let watch = r#"n="$1/node"; [ -x "$n" ] || n=node; "$n" "$1/server.js" & s=$!; while kill -0 "$2" 2>/dev/null && kill -0 $s 2>/dev/null; do sleep 1; done; kill $s 2>/dev/null"#;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(watch).arg("sh").arg(&dir).arg(std::process::id().to_string());
        cmd
    };
    cmd.current_dir(&dir).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _child = cmd.spawn().map_err(|e| format!("Couldn't start Stremio's streaming server from {} ({e})", dir.display()))?;
    #[cfg(windows)]
    end_with_app(&_child);
    Ok(())
}

/// Put the server in a job that Windows kills when this process exits, however it exits. The
/// job handle is deliberately never closed: the process exiting closes it.
#[cfg(windows)]
fn end_with_app(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() { return }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(job, JobObjectExtendedLimitInformation, (&raw const info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32);
        AssignProcessToJobObject(job, child.as_raw_handle());
    }
}
