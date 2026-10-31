//! Updates from GitHub Releases. At start the app asks for the newest release and, when it's
//! newer than this build, offers it; Update installs it in place and restarts the app. On
//! Windows the release's Setup does it (`--update`: no window; it closes the app, installs and
//! starts the new one). On macOS the app swaps its own bundle for the release zip's and reopens.

use serde_json::Value;

const RELEASES: &str = "https://api.github.com/repos/prodBirdy/stremio-rune/releases?per_page=20";
/// The release asset this platform updates from (the name's ending).
const ASSET: &str = if cfg!(windows) { "-windows-setup.exe" } else { "-macos-arm64.zip" };

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

/// The newest release, if it's newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let list: Value = ureq::get(RELEASES).header("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string())?
        .body_mut().read_json().map_err(|e| e.to_string())?;
    Ok(newest(&list, env!("CARGO_PKG_VERSION"), ASSET))
}

fn download(url: &str, to: &std::path::Path) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("Couldn't download the update: {e}");
    let mut res = ureq::get(url).call().map_err(|e| fail(&e))?;
    let mut file = std::fs::File::create(to).map_err(|e| fail(&e))?;
    std::io::copy(&mut res.body_mut().as_reader(), &mut file).map_err(|e| fail(&e))?;
    Ok(())
}

/// Download the release's Setup and start it with `--update`: it closes this app, installs over
/// it and starts the new version.
#[cfg(windows)]
pub fn install(r: &Release) -> Result<(), String> {
    let url = r.asset.as_deref().ok_or("This release has no Windows Setup")?;
    let path = std::env::temp_dir().join(format!("stremio-rune-{}-setup.exe", r.version));
    download(url, &path)?;
    std::process::Command::new(&path).arg("--update").spawn().map_err(|e| format!("Couldn't start the update: {e}"))?;
    Ok(())
}

/// Swap this app's bundle for the release's and have it reopen once this process has quit
/// (the caller quits). The new bundle is unpacked beside the old one, so the swap is two renames
/// on one volume, and the old bundle comes back if the second fails.
#[cfg(target_os = "macos")]
pub fn install(r: &Release) -> Result<(), String> {
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
    download(url, &zip)?;
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
    // Reopen once this process is gone, then clear the old bundle away.
    Command::new("/bin/sh").args(["-c", r#"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; open "$2"; rm -rf "$3""#, "sh"])
        .arg(std::process::id().to_string()).arg(&bundle).arg(&staging).spawn().map_err(|e| fail(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
