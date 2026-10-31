//! The app icon (assets/icon.ico) as the exe's icon resource: Explorer shows it, and Windows
//! uses it for the taskbar and title since the app's windows set none of their own.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
