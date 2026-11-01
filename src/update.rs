//! Updates from GitHub Releases. At start the app asks for the newest release and, when it's
//! newer than this build, offers it; Update installs it in place and restarts the app. On
//! Windows the release's Setup does it (`--update`: no window; it closes the app, installs and
//! starts the new one). On macOS the app swaps its own bundle for the release zip's and reopens.

use serde_json::Value;

const RELEASES: &str = "https://api.github.com/repos/prodBirdy/stremio-rune/releases?per_page=20";
/// The release asset this platform updates from (the name's ending).
const ASSET: &str = if cfg!(windows) { "-windows-setup.exe" } else if cfg!(target_os = "macos") { "-macos-arm64.zip" } else { "-linux-x86_64.tar.gz" };

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release on GitHub, for when it has nothing to update this platform from.
    pub page: String,
    /// Download URL of this platform's asset (`ASSET`).
    pub asset: Option<String>,
}

/// `major.minor.patch` of a version or tag (`v0.2.1-alpha` → [0, 2, 1]).
// ponytail: pre-release suffixes are ignored, so 0.2.2-alpha → 0.2.2-beta isn't offered; compare them if we ever ship two of one version.
fn core(v: &str) -> [u64; 3] {
    let mut n = [0; 3];
    let v = v.trim_start_matches('v');
    for (slot, part) in n.iter_mut().zip(v.split('-').next().unwrap_or("").split('.')) { *slot = part.parse().unwrap_or(0) }
    n
}

/// The newest release in GitHub's `list` when it's newer than `current`, with its asset whose
/// name ends in `suffix`.
fn newest(list: &Value, current: &str, suffix: &str) -> Option<Release> {
    let tag = |r: &Value| r["tag_name"].as_str().unwrap_or("").to_string();
    let r = list.as_array()?.iter().filter(|r| r["draft"] != true).max_by_key(|r| core(&tag(r)))?;
    if core(&tag(r)) <= core(current) { return None }
    let asset = r["assets"].as_array().into_iter().flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
        .and_then(|a| a["browser_download_url"].as_str()).map(String::from);
    Some(Release { version: tag(r).trim_start_matches('v').into(), page: r["html_url"].as_str().unwrap_or_default().into(), asset })
}

/// The newest release, if it's newer than this build. `STREMIO_RUNE_RELEASES=<url>` asks a
/// release list there instead (GitHub's shape), to try updating without publishing one.
pub fn check() -> Result<Option<Release>, String> {
    let url = std::env::var("STREMIO_RUNE_RELEASES").unwrap_or_else(|_| RELEASES.into());
    let list: Value = ureq::get(&url).header("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string())?
        .body_mut().read_json().map_err(|e| e.to_string())?;
    Ok(newest(&list, env!("CARGO_PKG_VERSION"), ASSET))
}

/// How far an update has got: downloading (the share done, once the size is known), then
/// installing.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub enum Progress { Downloading(Option<f32>), Installing }

/// Download to `to`, telling `progress` at each whole percent.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn download(url: &str, to: &std::path::Path, progress: &dyn Fn(Progress)) -> Result<(), String> {
    use std::io::{Read, Write};
    let fail = |e: &dyn std::fmt::Display| format!("Couldn't download the update: {e}");
    let mut res = ureq::get(url).call().map_err(|e| fail(&e))?;
    let total = res.headers().get("content-length").and_then(|v| v.to_str().ok()?.parse::<u64>().ok()).filter(|&n| n > 0);
    let mut file = std::fs::File::create(to).map_err(|e| fail(&e))?;
    let (mut body, mut buf, mut done, mut shown) = (res.body_mut().as_reader(), vec![0u8; 1 << 16], 0u64, None);
    progress(Progress::Downloading(None));
    loop {
        let n = body.read(&mut buf).map_err(|e| fail(&e))?;
        if n == 0 { break }
        file.write_all(&buf[..n]).map_err(|e| fail(&e))?;
        done += n as u64;
        let pct = total.map(|t| (done * 100 / t).min(100));
        if pct.is_some() && pct != shown {
            shown = pct;
            progress(Progress::Downloading(total.map(|t| done as f32 / t as f32)));
        }
    }
    Ok(())
}

/// Download the release's Setup and start it with `--update`: it closes this app, installs over
/// it and starts the new version.
#[cfg(windows)]
pub fn install(r: &Release, progress: &dyn Fn(Progress)) -> Result<(), String> {
    let url = r.asset.as_deref().ok_or("This release has no Windows Setup")?;
    let path = std::env::temp_dir().join(format!("stremio-rune-{}-setup.exe", r.version));
    download(url, &path, progress)?;
    progress(Progress::Installing);
    std::process::Command::new(&path).arg("--update").spawn().map_err(|e| format!("Couldn't start the update: {e}"))?;
    Ok(())
}

/// Swap this app's bundle for the release's and have it reopen once this process has quit
/// (the caller quits). The new bundle is unpacked beside the old one, so the swap is two renames
/// on one volume, and the old bundle comes back if the second fails.
#[cfg(target_os = "macos")]
pub fn install(r: &Release, progress: &dyn Fn(Progress)) -> Result<(), String> {
    use std::process::Command;
    let url = r.asset.as_deref().ok_or("This release has no macOS app")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // …/Stremio Rune.app/Contents/MacOS/stremio-rune
    let bundle = exe.ancestors().nth(3).filter(|b| b.extension().is_some_and(|e| e == "app"))
        .ok_or("Updating needs the app bundle (Stremio Rune.app)")?.to_path_buf();
    let fail = |e: &dyn std::fmt::Display| format!("Couldn't install the update: {e}");
    let staging = bundle.with_file_name(".stremio-rune-update");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| fail(&e))?;
    let zip = staging.join("update.zip");
    download(url, &zip, progress)?;
    progress(Progress::Installing);
    let unzip = Command::new("ditto").args(["-x", "-k"]).arg(&zip).arg(&staging).status().map_err(|e| fail(&e))?;
    let new = std::fs::read_dir(&staging).map_err(|e| fail(&e))?.flatten().map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"));
    let Some(new) = new.filter(|_| unzip.success()) else { return Err(fail(&"the download has no app in it")) };
    let old = staging.join("old.app");
    std::fs::rename(&bundle, &old).map_err(|e| fail(&e))?;
    if let Err(e) = std::fs::rename(&new, &bundle) {
        let _ = std::fs::rename(&old, &bundle);
        return Err(fail(&e));
    }
    // Reopen once this process is gone, then clear the old bundle away. In a process group of
    // its own, so whatever started this app (a terminal) doesn't end it along with the app.
    use std::os::unix::process::CommandExt;
    Command::new("/bin/sh").args(["-c", r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; open "$2"; rm -rf "$3""#, "sh"])
        .arg(std::process::id().to_string()).arg(&bundle).arg(&staging).process_group(0).spawn().map_err(|e| fail(&e))?;
    Ok(())
}

/// Linux: no self-update yet; the release page says where to get it.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_: &Release, _: &dyn Fn(Progress)) -> Result<(), String> {
    Err("Updating in place isn't supported on Linux yet: rebuild from the repository".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn download_reports_each_percent_then_the_whole() {
        use std::io::{Read, Write};
        let body: Vec<u8> = (0..1_000_000u32).map(|i| i as u8).collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/update.zip", listener.local_addr().unwrap());
        let served = body.clone();
        std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut head = [0u8; 1024];
            let _ = c.read(&mut head);
            let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", served.len()).as_bytes());
            let _ = c.write_all(&served);
        });
        let to = std::env::temp_dir().join(format!("stremio-rune-update-test-{}", std::process::id()));
        let seen = std::cell::RefCell::new(vec![]);
        download(&url, &to, &|p| seen.borrow_mut().push(p)).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), body);
        let _ = std::fs::remove_file(&to);
        let seen = seen.into_inner();
        assert_eq!(seen.first(), Some(&Progress::Downloading(None)), "started, size not yet shown");
        assert_eq!(seen.last(), Some(&Progress::Downloading(Some(1.0))), "ends whole");
        let shares: Vec<f32> = seen.iter().filter_map(|p| match p { Progress::Downloading(Some(f)) => Some(*f), _ => None }).collect();
        assert!(shares.windows(2).all(|w| w[0] < w[1]), "only forward");
        assert!(shares.len() <= 101, "at most once a percent: {}", shares.len());
    }

    #[test]
    fn offers_only_a_newer_release_with_this_platforms_asset() {
        let list = json!([
            { "tag_name": "v0.2.1-alpha", "html_url": "p021", "assets": [] },
            { "tag_name": "v0.3.0-alpha", "html_url": "p030", "assets": [
                { "name": "stremio-rune-v0.3.0-alpha-macos-arm64.zip", "browser_download_url": "zip" },
                { "name": "stremio-rune-0.3.0-alpha-windows-setup.exe", "browser_download_url": "exe" }] },
            { "tag_name": "v9.0.0", "draft": true },
        ]);
        let r = newest(&list, "0.2.2", "-windows-setup.exe").unwrap();
        assert_eq!(r, Release { version: "0.3.0-alpha".into(), page: "p030".into(), asset: Some("exe".into()) });
        assert_eq!(newest(&list, "0.2.2", "-macos-arm64.zip").unwrap().asset.as_deref(), Some("zip"));
        assert_eq!(newest(&list, "0.2.2", "-linux.tar.gz").unwrap().asset, None, "no asset: the page");
        assert_eq!(newest(&list, "0.3.0", ASSET), None, "same version: nothing");
        assert_eq!(newest(&list, "0.10.0", ASSET), None, "numeric, not text, order");
        assert_eq!(newest(&json!([]), "0.1.0", ASSET), None);
    }
}
