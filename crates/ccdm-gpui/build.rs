//! Windows-only build script: embed the application icon.
//!
//! `assets/icon.ico` is rendered from `assets/logo.svg` by CI
//! (`tools/render_icons.py`). Missing file = build continues without an
//! embedded icon (e.g. first local build before CI ever ran).

fn main() {
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/app.rc");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        if std::path::Path::new("assets/icon.ico").exists() {
            embed_resource::compile("assets/app.rc");
        } else {
            println!("cargo:warning=assets/icon.ico missing, building without embedded icon");
        }
    }
}
