//! Minimal string tables (cf. XDM translations).
//!
//! English is embedded; other languages drop a flat JSON object
//! (`{"app.title": "..."}`) into `<config-dir>/ccdm/lang/<code>.json`
//! (loaded once at startup via [`load_available`]). Missing keys fall back
//! to English, then to the key itself. Operator logs stay English on
//! purpose — only user-facing UI strings go through [`t`]/[`format`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{CcdmError, Result};

/// Language used when nothing else is configured.
pub const DEFAULT_LANG: &str = "en";

/// External `<code>.json` overrides, keyed by language then key.
static OVERRIDES: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = OnceLock::new();

fn overrides() -> &'static Mutex<HashMap<String, HashMap<String, String>>> {
    OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn override_get(lang: &str, key: &str) -> Option<String> {
    overrides()
        .lock()
        .ok()?
        .get(lang)?
        .get(key)
        .cloned()
}

/// Directory holding `<code>.json` language files, if known.
pub fn lang_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("ccdm").join("lang"))
}

/// Load every `<code>.json` in [`lang_dir`]; returns files loaded.
pub fn load_available() -> usize {
    let Some(dir) = lang_dir() else {
        return 0;
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut loaded = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(code) = path.file_stem().and_then(|s| s.to_str()) {
            if load_file(code, &path).is_ok() {
                loaded += 1;
            }
        }
    }
    loaded
}

/// Merge one language file over the embedded table.
pub fn load_file(lang: &str, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(CcdmError::from)?;
    let table: HashMap<String, String> =
        serde_json::from_str(&text).map_err(CcdmError::from)?;
    if let Ok(mut all) = overrides().lock() {
        all.entry(lang.to_string()).or_default().extend(table);
        Ok(())
    } else {
        Err(CcdmError::Other("language store poisoned".to_string()))
    }
}

/// Translate `key` (English fallback, then the key itself).
pub fn t(lang: &str, key: &str) -> String {
    if lang != DEFAULT_LANG {
        if let Some(text) = override_get(lang, key) {
            return text;
        }
    }
    en(key).unwrap_or(key).to_string()
}

/// Translate a `{placeholder}` template.
pub fn format(lang: &str, key: &str, args: &[(&str, &str)]) -> String {
    let mut text = t(lang, key);
    for (name, value) in args {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// Embedded English table.
fn en(key: &str) -> Option<&'static str> {
    Some(match key {
        // App chrome (GUI).
        "app.title" => "ccdwonloadmanager",
        "app.live" => "{t} — live queue",
        "app.stats" => "{total} row(s), {active} active, {cats} categories • {cfg}",
        "app.empty" => "Queue is empty — copy a download link, then hit “Add from clipboard”.",
        "app.hint_settings" => "apply to newly started downloads",
        "app.hint_auto" => "monitor adds copied links • sched gates starts",
        "app.clip_hint" => "Copy a download link, then hit “Add from clipboard”.",
        "tb.add" => "Add from clipboard",
        "tb.speed" => "Speed: {v}",
        "tb.connections" => "Connections: {n}",
        "tb.organize" => "Organize: {v}",
        "tb.monitor" => "Monitor: {v}",
        "tb.sched" => "Sched: {v}",
        "tb.sched_off" => "Sched: off",
        "tb.shutdown" => "Shutdown: {v}",
        "tb.theme" => "Theme: {v}",
        "tb.quality" => "Quality: {v}",
        "tb.unlimited" => "unlimited",
        "cm.on" => "on",
        "cm.off" => "off",
        "cm.dark" => "dark",
        "cm.light" => "light",
        // Row buttons + states.
        "row.start" => "Start",
        "row.pause" => "Pause",
        "row.resume" => "Resume",
        "row.retry" => "Retry",
        "row.remove" => "Remove",
        "row.folder" => "Folder",
        "row.again" => "Again",
        "row.mp3" => "MP3",
        "row.done" => "done",
        "st.queued" => "queued",
        "st.downloading" => "downloading",
        "st.paused" => "paused",
        "st.finished" => "finished",
        "st.failed" => "failed",
        // GUI notices.
        "n.added" => "added {file} — hit Start",
        "n.probing" => "probing {url} …",
        "n.pausing" => "pausing {name} …",
        "n.browser" => "browser added {file}",
        "n.no_text" => "clipboard has no text — copy a link first",
        "n.dup" => "that URL is already in the list",
        "n.busy" => "pause it before removing",
        "n.save_q" => "queue save failed: {e}",
        "n.save_c" => "config save failed: {e}",
        "n.no_cfgdir" => "no config dir on this platform",
        "n.probe_fail" => "probe failed: {e}",
        "n.rt_err" => "runtime error: {e}",
        "n.notfound" => "row not found",
        "n.reveal" => "cannot open folder: {e}",
        "n.converting" => "converting {file}…",
        "n.converted" => "converted → {out}",
        "n.convert_fail" => "convert failed: {e}",
        "n.no_ffmpeg" => "ffmpeg not found on PATH",
        "n.update" => "update available: {tag} ({url})",
        "n.outside" => "outside scheduled window ({w}); toggle Sched to run now",
        "n.shutting" => "queue complete, shutting down in 60s…",
        "n.shut_fail" => "shutdown failed: {e}",
        // CLI messages.
        "c.probe_url" => "final url : {v}",
        "c.probe_name" => "file name : {v}",
        "c.probe_size" => "size      : {v}",
        "c.probe_unknown" => "unknown",
        "c.probe_ranges" => "ranges    : {v}",
        "c.probe_mime" => "mime      : {v}",
        "c.probe_media" => "media     : {v}",
        "c.downloading" => "downloading {url} -> {dest} ({n} segments)",
        "c.done" => "done: {dest} ({n} bytes)",
        "c.added" => "added {id}  {file}  {url}",
        "c.empty" => "queue is empty",
        "c.stored" => "stored in {p}",
        "c.none" => "nothing queued",
        "c.starting" => "starting {id} -> {dest} ({n} segments)",
        "c.retry" => "attempt {n} failed ({e}); retrying in {w}s...",
        "c.row_done" => "done: {dest}",
        "c.row_fail" => "failed {dest}: {e}",
        "c.summary" => "finished: {ok} ok, {failed} failed",
        "c.unknown" => "unknown id: {id}",
        "c.outside" => "outside scheduled window ({w}); rerun with --force or --wait",
        "c.waiting" => "waiting for scheduled window ({w})…",
        "c.shutting" => "queue complete, shutting down in 60s…",
        "c.shut_fail" => "shutdown failed: {e}",
        "c.converting" => "converting {src} -> {fmt}…",
        "c.converted" => "converted: {out}",
        "c.upd_none" => "set update_repo in config.json to enable update checks",
        "c.upd_cur" => "up to date ({v})",
        "c.upd_new" => "update available: {tag} — {url}",
        "c.lang_cur" => "language: {l}",
        "c.lang_set" => "language set to {l} (add lang/{l}.json for translations)",
        "c.yt_noytdlp" => "yt-dlp not found — install it (https://github.com/yt-dlp/yt-dlp) or set ytdlp_path in config.json",
        "c.yt_nostreams" => "yt-dlp returned no downloadable streams",
        "c.yt_resolving" => "resolving {url}…",
        "c.yt_found" => "found: {title}",
        "c.yt_muxing" => "muxing video + audio…",
        "n.video_only" => "{file} is video-only (CLI `video` merges audio)",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_defaults() {
        assert_eq!(t("en", "row.pause"), "Pause");
        assert_eq!(t("xx", "row.pause"), "Pause");
        assert_eq!(t("en", "no.such.key"), "no.such.key");
    }

    #[test]
    fn templates_fill() {
        let text = format("en", "c.summary", &[("ok", "2"), ("failed", "0")]);
        assert_eq!(text, "finished: 2 ok, 0 failed");
    }

    #[test]
    fn external_file_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fr.json");
        std::fs::write(&path, r#"{"row.pause": "PauseFR", "only.fr": "x"}"#).unwrap();
        load_file("fr", &path).unwrap();
        assert_eq!(t("fr", "row.pause"), "PauseFR");
        assert_eq!(t("fr", "row.start"), "Start");
    }
}
