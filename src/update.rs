//! Updates from GitHub Releases. At start the app asks for the newest release and, when it's
//! newer than this build, offers it. On Windows, Update downloads the release's Setup and runs
//! it: Setup closes the app and installs over it. Elsewhere it opens the release's page.

use serde_json::Value;

const RELEASES: &str = "https://api.github.com/repos/prodBirdy/stremio-rune/releases?per_page=20";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release on GitHub, for where there's no Setup to run.
    pub page: String,
    /// The Windows Setup's download URL.
    pub setup: Option<String>,
}

/// `major.minor.patch` of a version or tag (`v0.2.1-alpha` → [0, 2, 1]).
// ponytail: pre-release suffixes are ignored, so 0.2.2-alpha → 0.2.2-beta isn't offered; compare them if we ever ship two of one version.
fn core(v: &str) -> [u64; 3] {
    let mut n = [0; 3];
    let v = v.trim_start_matches('v');
    for (slot, part) in n.iter_mut().zip(v.split('-').next().unwrap_or("").split('.')) { *slot = part.parse().unwrap_or(0) }
    n
}

/// The newest release in GitHub's `list` when it's newer than `current`.
fn newest(list: &Value, current: &str) -> Option<Release> {
    let tag = |r: &Value| r["tag_name"].as_str().unwrap_or("").to_string();
    let r = list.as_array()?.iter().filter(|r| r["draft"] != true).max_by_key(|r| core(&tag(r)))?;
    if core(&tag(r)) <= core(current) { return None }
    let setup = r["assets"].as_array().into_iter().flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.ends_with("-windows-setup.exe")))
        .and_then(|a| a["browser_download_url"].as_str()).map(String::from);
    Some(Release { version: tag(r).trim_start_matches('v').into(), page: r["html_url"].as_str().unwrap_or_default().into(), setup })
}

/// The newest release, if it's newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let list: Value = ureq::get(RELEASES).header("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string())?
        .body_mut().read_json().map_err(|e| e.to_string())?;
    Ok(newest(&list, env!("CARGO_PKG_VERSION")))
}

/// Download the release's Setup into the temp folder and start it.
pub fn install(r: &Release) -> Result<(), String> {
    let url = r.setup.as_deref().ok_or("This release has no Windows Setup")?;
    let path = std::env::temp_dir().join(format!("stremio-rune-{}-setup.exe", r.version));
    let fail = |e: &dyn std::fmt::Display| format!("Couldn't download the update: {e}");
    let mut res = ureq::get(url).call().map_err(|e| fail(&e))?;
    let mut file = std::fs::File::create(&path).map_err(|e| fail(&e))?;
    std::io::copy(&mut res.body_mut().as_reader(), &mut file).map_err(|e| fail(&e))?;
    drop(file);
    std::process::Command::new(&path).spawn().map_err(|e| format!("Couldn't start the update: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn offers_only_a_newer_release_with_its_setup() {
        let list = json!([
            { "tag_name": "v0.2.1-alpha", "html_url": "p021", "assets": [] },
            { "tag_name": "v0.3.0-alpha", "html_url": "p030", "assets": [
                { "name": "stremio-rune-v0.3.0-alpha-macos-arm64.zip", "browser_download_url": "zip" },
                { "name": "stremio-rune-0.3.0-alpha-windows-setup.exe", "browser_download_url": "exe" }] },
            { "tag_name": "v9.0.0", "draft": true },
        ]);
        let r = newest(&list, "0.2.2").unwrap();
        assert_eq!(r, Release { version: "0.3.0-alpha".into(), page: "p030".into(), setup: Some("exe".into()) });
        assert_eq!(newest(&list, "0.3.0"), None, "same version: nothing");
        assert_eq!(newest(&list, "0.10.0"), None, "numeric, not text, order");
        assert_eq!(newest(&json!([]), "0.1.0"), None);
    }
}
