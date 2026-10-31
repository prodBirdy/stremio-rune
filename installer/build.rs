//! Bakes into Setup at build time:
//! - the app exe (`STREMIO_RUNE_PAYLOAD`, a path set by build.ps1; empty in dev builds),
//! - the app version from the app's Cargo.toml,
//! - the icon and manifest (setup.rc).

use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-env-changed=STREMIO_RUNE_PAYLOAD");
    let payload = out.join("payload.bin");
    match env::var("STREMIO_RUNE_PAYLOAD").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            println!("cargo:rerun-if-changed={path}");
            fs::copy(&path, &payload).unwrap_or_else(|e| panic!("STREMIO_RUNE_PAYLOAD {path}: {e}"));
        }
        None => fs::write(&payload, []).unwrap(),
    }

    println!("cargo:rerun-if-changed=../Cargo.toml");
    let manifest = fs::read_to_string("../Cargo.toml").unwrap();
    let version = manifest
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = \""))
        .and_then(|v| v.strip_suffix('"'))
        .expect("version in ../Cargo.toml");
    println!("cargo:rustc-env=APP_VERSION={version}");

    println!("cargo:rerun-if-changed=setup.rc");
    println!("cargo:rerun-if-changed=setup.manifest");
    println!("cargo:rerun-if-changed=../assets/icon.ico");
    embed_resource::compile("setup.rc", embed_resource::NONE).manifest_required().unwrap();
}
