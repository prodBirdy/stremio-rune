//! The app icon (assets/icon.ico) as the exe's icon resource: Explorer shows it, and Windows
//! uses it for the taskbar and title since the app's windows set none of their own.
//!
//! macOS: the libmpv the app loads from Stremio.app finds its own libraries (libav*, libass…)
//! through `@rpath`, which resolves through this binary's run paths: Stremio.app's MacOS folder.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,/Applications/Stremio.app/Contents/MacOS");
    }
}
