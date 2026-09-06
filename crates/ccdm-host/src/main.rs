//! `ccdm-host`: browser native-messaging host.
//!
//! The browser launches this over stdio (`ccdm-host --stdio`, the default)
//! and speaks length-prefixed JSON (see `ccdm_core::browser`). Accepted
//! downloads land in the shared queue store for the GUI/CLI.
//!
//! ```sh
//! ccdm-host --stdio            # serve one browser session on stdio
//! ccdm-host manifest --chrome  # print the host manifest template
//! ccdm-host manifest --firefox # print the Firefox variant
//! ccdm-host install            # write manifest + register (Windows reg)
//! ```

use std::path::PathBuf;

use ccdm_core::{browser, AppConfig, Store};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["manifest", "--firefox"] => {
            let exe = exe_string();
            let manifest = browser::firefox_manifest(&exe, "REPLACE-WITH-EXTENSION-ID");
            println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
        }
        ["manifest", _] | ["manifest"] => {
            let exe = exe_string();
            let manifest = browser::chrome_manifest(&exe, "REPLACE-WITH-EXTENSION-ID");
            println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
        }
        ["install"] => install(),
        _ => serve_stdio(),
    }
}

fn exe_string() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ccdm-host".to_string())
}

/// Serve one browser session: read framed JSON from stdin, write replies.
fn serve_stdio() {
    let config = match AppConfig::config_path() {
        Some(path) => AppConfig::load(&path).unwrap_or_default(),
        None => AppConfig::default(),
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    loop {
        let message = match browser::read_message(&mut input) {
            Ok(message) => message,
            Err(_) => break, // EOF / broken pipe: browser went away.
        };
        // Reload every round: the GUI/CLI may have changed the queue.
        let mut store = Store::load().unwrap_or_else(|_| Store::new(PathBuf::from("queue.json")));
        let response = match serde_json::from_value::<browser::HostRequest>(message) {
            Ok(request) => browser::handle_request(
                request,
                &mut store,
                config.max_connections,
                env!("CARGO_PKG_VERSION"),
            ),
            Err(e) => browser::HostResponse::Rejected {
                reason: format!("bad message: {e}"),
            },
        };
        let value = serde_json::to_value(&response).unwrap();
        if browser::write_message(&mut output, &value).is_err() {
            break;
        }
    }
}

/// Write the manifest next to this exe and register it (Windows).
/// Prints the manual steps for other platforms.
fn install() {
    let exe = std::env::current_exe().expect("own exe path");
    let dir = exe.parent().expect("exe dir").to_path_buf();
    let manifest_path = dir.join(format!("{}.json", browser::HOST_NAME));
    let manifest =
        browser::chrome_manifest(&exe.display().to_string(), "REPLACE-WITH-EXTENSION-ID");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("write manifest");
    println!("wrote {}", manifest_path.display());
    println!("edit allowed_origins to your extension id, then register:");
    if cfg!(windows) {
        for (root, key) in [
            (
                "HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts",
                "Chrome",
            ),
            (
                "HKCU\\Software\\Microsoft\\Edge\\NativeMessagingHosts",
                "Edge",
            ),
            ("HKCU\\Software\\Mozilla\\NativeMessagingHosts", "Firefox"),
        ] {
            let full = format!("{root}\\{}", browser::HOST_NAME);
            let status = std::process::Command::new("reg")
                .args([
                    "add",
                    &full,
                    "/ve",
                    "/d",
                    &manifest_path.display().to_string(),
                    "/f",
                ])
                .status();
            println!("  [{key}] {full} -> {status:?}");
        }
        println!("note: Firefox uses the Chrome-shape manifest only if");
        println!("`allowed_extensions` is used instead — regenerate with:");
        println!(
            "  ccdm-host manifest --firefox > {}",
            manifest_path.display()
        );
    } else {
        println!(
            "  Linux:   ~/.config/google-chrome/NativeMessagingHosts/{}.json",
            browser::HOST_NAME
        );
        println!(
            "  macOS:   ~/Library/Application Support/Google/Chrome/NativeMessagingHosts/{}.json",
            browser::HOST_NAME
        );
        println!(
            "  Firefox: ~/.mozilla/native-messaging-hosts/{}.json",
            browser::HOST_NAME
        );
        println!("copy the manifest printed by `ccdm-host manifest [--firefox]` there.");
    }
}
