//! Browser native-messaging host protocol (cf. XDM browser monitoring).
//!
//! Chrome / Edge / Firefox launch the host with packet-framed JSON over
//! stdio: `u32` little-endian length + UTF-8 JSON, in both directions.
//! The extension forwards taken-over downloads; the host appends them to
//! the shared queue store, which the GUI and CLI pick up.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::model::{guess_file_name, new_id, sanitize_file_name};
use crate::{CcdmError, DownloadEntry, Result, Store};

/// Must match the `name` in the host manifest and the extension code.
pub const HOST_NAME: &str = "com.ccdm.native";
/// Bumped when the JSON shapes below change incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;
/// Largest single message we accept (64 MiB — playlists never come here).
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// A request from the browser extension. `{"type":"ping"}` /
/// `{"type":"download","url":"...","filename":"..."}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HostRequest {
    Ping,
    Download {
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        referrer: Option<String>,
    },
}

/// Reply to the extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HostResponse {
    Pong { version: String },
    Accepted { id: String },
    Rejected { reason: String },
}

/// Read one length-prefixed JSON message. EOF surfaces as an IO error so
/// the host loop can shut down when the browser goes away.
pub fn read_message<R: Read>(reader: &mut R) -> Result<serde_json::Value> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(CcdmError::from)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MESSAGE_BYTES {
        return Err(CcdmError::Other(format!("bad message length: {len}")));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(CcdmError::from)?;
    serde_json::from_slice(&buf).map_err(CcdmError::from)
}

/// Write one length-prefixed JSON message.
pub fn write_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(CcdmError::from)?;
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .map_err(CcdmError::from)?;
    writer.write_all(&bytes).map_err(CcdmError::from)?;
    writer.flush().map_err(CcdmError::from)
}

/// Answer a request; accepted downloads land in the shared queue store.
pub fn handle_request(
    request: HostRequest,
    store: &mut Store,
    max_connections: usize,
    version: &str,
) -> HostResponse {
    match request {
        HostRequest::Ping => HostResponse::Pong {
            version: version.to_string(),
        },
        HostRequest::Download {
            url,
            filename,
            referrer: _,
        } => {
            if url::Url::parse(&url).is_err() {
                return HostResponse::Rejected {
                    reason: format!("bad url: {url}"),
                };
            }
            let name = match filename {
                Some(n) => sanitize_file_name(&n),
                None => sanitize_file_name(&guess_file_name(&url)),
            };
            let id = new_id("host");
            let entry =
                DownloadEntry::with_plan(id.clone(), url, name, None, max_connections.max(1));
            store.queue_mut().add(entry);
            match store.save() {
                Ok(()) => HostResponse::Accepted { id },
                Err(e) => HostResponse::Rejected {
                    reason: e.to_string(),
                },
            }
        }
    }
}

/// Host manifest for Chromium browsers (`allowed_origins` gets the real
/// extension id filled in by `ccdm-host install`).
pub fn chrome_manifest(exe_path: &str, extension_id: &str) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": "ccdwonloadmanager native messaging host",
        "path": exe_path,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{extension_id}/")],
    })
}

/// Host manifest for Firefox (`allowed_extensions` gets the real id).
pub fn firefox_manifest(exe_path: &str, extension_id: &str) -> serde_json::Value {
    serde_json::json!({
        "name": HOST_NAME,
        "description": "ccdwonloadmanager native messaging host",
        "path": exe_path,
        "type": "stdio",
        "allowed_extensions": [extension_id],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let value = serde_json::json!({"type": "ping"});
        let mut buf = Vec::new();
        write_message(&mut buf, &value).unwrap();
        assert_eq!(buf.len(), 4 + value.to_string().len());
        let back = read_message(&mut &buf[..]).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn rejects_empty_and_huge_lengths() {
        assert!(read_message(&mut &[0u8, 0, 0, 0][..]).is_err());
        assert!(read_message(&mut &[0xFFu8, 0xFF, 0xFF, 0xFF][..]).is_err());
    }

    #[test]
    fn ping_pongs_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::load_from(&dir.path().join("q.json")).unwrap();
        let resp = handle_request(HostRequest::Ping, &mut store, 8, "0.7.0");
        assert!(matches!(resp, HostResponse::Pong { .. }));
    }

    #[test]
    fn download_queues_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::load_from(&dir.path().join("q.json")).unwrap();
        let resp = handle_request(
            HostRequest::Download {
                url: "https://h.com/file.zip".to_string(),
                filename: None,
                referrer: None,
            },
            &mut store,
            8,
            "0.7.0",
        );
        let HostResponse::Accepted { id } = resp else {
            panic!("expected accept, got {resp:?}");
        };
        let back = Store::load_from(&dir.path().join("q.json")).unwrap();
        let entry = back.queue().get(&id).unwrap();
        assert_eq!(entry.file_name, "file.zip");
    }

    #[test]
    fn bad_url_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::load_from(&dir.path().join("q.json")).unwrap();
        let resp = handle_request(
            HostRequest::Download {
                url: "not a url".to_string(),
                filename: None,
                referrer: None,
            },
            &mut store,
            8,
            "0.7.0",
        );
        assert!(matches!(resp, HostResponse::Rejected { .. }));
    }
}
