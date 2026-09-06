//! Video-page resolution via yt-dlp (cf. XDM `YDLWrapper`).
//!
//! Watch pages (YouTube & co.) are not files: player responses need
//! signature deciphering that changes constantly, so — like XDM — we
//! delegate *resolving* to a `yt-dlp` binary and keep *downloading* in our
//! engine (segmented, resumable, speed-capped). yt-dlp is not bundled:
//! install it once and optionally point `ytdlp_path` at it.

use serde_json::Value;

use crate::{CcdmError, Result};

/// Watch-page hosts we hand to yt-dlp instead of probing as files.
const VIDEO_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "vimeo.com",
    "dailymotion.com",
    "twitch.tv",
    "facebook.com",
    "instagram.com",
    "tiktok.com",
    "x.com",
];

/// Whether `url` looks like a watch page (not a direct file).
pub fn is_video_page(url: &str) -> bool {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_lowercase))
        .unwrap_or_default();
    VIDEO_HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// yt-dlp `-f` specs preferring a **single file** (GUI auto-resolve).
pub fn single_file_spec(quality: &str) -> &str {
    match quality.to_lowercase().as_str() {
        "1080p" | "1080" => "b[height<=1080]/b",
        "720p" | "720" => "b[height<=720]/b",
        "480p" | "480" => "b[height<=480]/b",
        "audio" | "bestaudio" | "mp3" => "ba",
        _ => "b",
    }
}

/// yt-dlp `-f` specs allowing merged streams (CLI `video` muxes them).
/// Unknown values pass through untouched as custom specs.
pub fn full_spec(quality: &str) -> &str {
    match quality.to_lowercase().as_str() {
        "best" => "bv*+ba/b",
        "1080p" | "1080" => "bv*[height<=1080]+ba/b",
        "720p" | "720" => "bv*[height<=720]+ba/b",
        "480p" | "480" => "bv*[height<=480]+ba/b",
        "audio" | "bestaudio" | "mp3" => "ba/b",
        custom => custom,
    }
}

/// Qualities cycled by the GUI button.
pub const QUALITIES: &[&str] = &["best", "1080p", "720p", "480p", "audio"];

/// A resolved watch page: direct download URL(s) + title.
#[derive(Debug, Clone)]
pub struct ResolvedMedia {
    pub title: String,
    pub ext: String,
    pub direct_url: Option<String>,
    pub video_url: Option<String>,
    pub audio_url: Option<String>,
}

impl ResolvedMedia {
    /// Single download URL when no muxing is required, else the
    /// video-only stream (GUI fallback; CLI merges properly).
    pub fn playback_url(&self) -> Option<&str> {
        self.direct_url
            .as_deref()
            .or(self.video_url.as_deref())
            .or(self.audio_url.as_deref())
    }

    /// True when only separate video+audio streams exist.
    pub fn needs_mux(&self) -> bool {
        self.direct_url.is_none() && self.video_url.is_some() && self.audio_url.is_some()
    }

    /// File extension for the GUI auto-resolve download.
    pub fn playback_ext(&self) -> &str {
        if self.direct_url.is_some() {
            &self.ext
        } else {
            "mp4"
        }
    }
}

/// Locate the yt-dlp binary: configured path first, else PATH probe.
pub fn find_ytdlp(configured: Option<&str>) -> Option<String> {
    if let Some(path) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        if std::path::Path::new(path).exists() {
            return Some(path.to_string());
        }
        // Might still resolve via PATH (e.g. bare `yt-dlp.exe`).
        if probe_ytdlp(path) {
            return Some(path.to_string());
        }
        return None;
    }
    ["yt-dlp", "yt-dlp.exe"]
        .into_iter()
        .find(|bin| probe_ytdlp(bin))
        .map(str::to_string)
}

fn probe_ytdlp(binary: &str) -> bool {
    std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Parse `--dump-single-json` output into [`ResolvedMedia`].
pub fn parse_resolved(value: &Value) -> ResolvedMedia {
    let raw_title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("video");
    let title: String = raw_title.chars().take(120).collect();
    let ext = value
        .get("ext")
        .and_then(|v| v.as_str())
        .unwrap_or("mp4")
        .to_string();
    let direct_url = value
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut video_url = None;
    let mut audio_url = None;
    if let Some(formats) = value.get("requested_formats").and_then(|v| v.as_array()) {
        for format in formats {
            let url = format.get("url").and_then(|v| v.as_str());
            let vcodec = format.get("vcodec").and_then(|v| v.as_str()).unwrap_or("none");
            let acodec = format.get("acodec").and_then(|v| v.as_str()).unwrap_or("none");
            if video_url.is_none() && vcodec != "none" {
                video_url = url.map(str::to_string);
            }
            if audio_url.is_none() && acodec != "none" {
                audio_url = url.map(str::to_string);
            }
        }
    }
    ResolvedMedia {
        title,
        ext,
        direct_url,
        video_url,
        audio_url,
    }
}

/// Resolve a watch page through yt-dlp (`format_spec` from
/// [`single_file_spec`]/[`full_spec`]).
pub fn resolve(ytdlp: &str, url: &str, format_spec: &str) -> Result<ResolvedMedia> {
    let output = std::process::Command::new(ytdlp)
        .args([
            "--dump-single-json",
            "--no-playlist",
            "--no-warnings",
            "-f",
            format_spec,
            url,
        ])
        .output()
        .map_err(|e| CcdmError::Other(format!("yt-dlp failed to start ({e}); is it installed?")))?;
    if !output.status.success() {
        let tail = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = tail.lines().collect();
        let start = lines.len().saturating_sub(3);
        return Err(CcdmError::Other(format!(
            "yt-dlp resolve failed: {}",
            lines[start..].join(" | ")
        )));
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| CcdmError::Other(format!("yt-dlp returned bad JSON: {e}")))?;
    Ok(parse_resolved(&value))
}

/// Mux separate video+audio files (stream copy, no re-encode).
pub fn mux_av(video: &std::path::Path, audio: &std::path::Path, output: &std::path::Path) -> Result<()> {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            &video.display().to_string(),
            "-i",
            &audio.display().to_string(),
            "-c",
            "copy",
            &output.display().to_string(),
        ])
        .status()
        .map_err(|e| CcdmError::Other(format!("ffmpeg failed to start ({e}); is it installed?")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CcdmError::Other(format!("ffmpeg mux failed: {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn page_detection() {
        assert!(is_video_page("https://www.youtube.com/watch?v=abc123"));
        assert!(is_video_page("https://youtu.be/abc123"));
        assert!(is_video_page("https://vimeo.com/12345"));
        assert!(!is_video_page("https://cdn.example.com/file.mp4"));
        assert!(!is_video_page("not a url"));
    }

    #[test]
    fn thumbnails_are_not_video_pages() {
        // Host-based detection: thumbnail CDN is not a watch host.
        assert!(!is_video_page("https://i.ytimg.com/vi/abc/hqdefault.jpg"));
    }

    #[test]
    fn spec_tables() {
        assert_eq!(single_file_spec("best"), "b");
        assert_eq!(single_file_spec("720p"), "b[height<=720]/b");
        assert_eq!(single_file_spec("audio"), "ba");
        assert_eq!(full_spec("best"), "bv*+ba/b");
        assert_eq!(full_spec("720p"), "bv*[height<=720]+ba/b");
        assert_eq!(full_spec("ba/best[ext=mp4]"), "ba/best[ext=mp4]");
    }

    #[test]
    fn parses_direct_json() {
        let media = parse_resolved(&json!({
            "title": "Some Video",
            "ext": "mp4",
            "url": "https://r1---example.googlevideo.com/videoplayback?x=1",
        }));
        assert_eq!(media.title, "Some Video");
        assert_eq!(media.direct_url.as_deref(), Some("https://r1---example.googlevideo.com/videoplayback?x=1"));
        assert!(!media.needs_mux());
        assert_eq!(media.playback_url(), media.direct_url.as_deref());
    }

    #[test]
    fn parses_merged_json() {
        let media = parse_resolved(&json!({
            "title": "Merged",
            "ext": "mp4",
            "requested_formats": [
                {"url": "https://v.example/v", "vcodec": "avc1", "acodec": "none"},
                {"url": "https://v.example/a", "vcodec": "none", "acodec": "opus"},
            ],
        }));
        assert!(media.direct_url.is_none());
        assert_eq!(media.video_url.as_deref(), Some("https://v.example/v"));
        assert_eq!(media.audio_url.as_deref(), Some("https://v.example/a"));
        assert!(media.needs_mux());
    }

    #[test]
    fn title_truncates() {
        let long = "x".repeat(500);
        let media = parse_resolved(&json!({"title": long}));
        assert_eq!(media.title.len(), 120);
    }

    #[test]
    fn missing_binary_is_none() {
        assert!(find_ytdlp(Some("/definitely/not/here-12345")).is_none());
    }
}
