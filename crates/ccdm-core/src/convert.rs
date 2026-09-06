//! Media conversion via system ffmpeg (cf. XDM video converter).
//!
//! Shells out to an `ffmpeg` on PATH — no bundling, no new crates. Missing
//! binary and ffmpeg failures surface as ordinary errors.

use std::path::{Path, PathBuf};

use crate::{CcdmError, Result};

/// Conversion target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertTarget {
    Mp3,
    Mp4,
}

impl ConvertTarget {
    /// File extension of the output.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Mp4 => "mp4",
        }
    }

    /// Parse `mp3` / `mp4` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "mp3" => Some(Self::Mp3),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }
}

/// `<config>/ccdm/bin` — home for auto-installed helpers.
pub fn tools_dir() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|dir| dir.join("ccdm").join("bin"))
}

fn probe_binary(binary: &str) -> bool {
    std::process::Command::new(binary)
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// ffmpeg binary: tools dir first (auto-setup), then PATH.
pub fn ffmpeg_binary() -> Option<String> {
    if let Some(dir) = tools_dir() {
        let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    ["ffmpeg", "ffmpeg.exe"]
        .into_iter()
        .find(|bin| probe_binary(bin))
        .map(str::to_string)
}

/// Whether `ffmpeg` runs (probed once per call — cheap enough for UI use).
pub fn ffmpeg_available() -> bool {
    ffmpeg_binary().is_some()
}

/// Output path next to `input` with the target extension.
pub fn output_path(input: &Path, target: ConvertTarget) -> PathBuf {
    let mut out = input.to_path_buf();
    out.set_extension(target.extension());
    out
}

/// Extensions worth offering an MP3 conversion for (video + audio).
pub fn convertible_to_mp3(file_name: &str) -> bool {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_lowercase();
    matches!(
        ext.as_str(),
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ts" | "mp3"
            | "wav" | "flac" | "ogg" | "m4a" | "opus" | "wma" | "aac"
    )
}

/// Convert `input` to `target` beside it; returns the output path.
pub fn convert(input: &Path, target: ConvertTarget) -> Result<PathBuf> {
    let ffmpeg = ffmpeg_binary().ok_or_else(|| {
        CcdmError::Other("ffmpeg not found — Setup video tools or install it".to_string())
    })?;
    let output = output_path(input, target);
    let mut args: Vec<String> = vec![
        "-y".to_string(),
        "-i".to_string(),
        input.display().to_string(),
    ];
    match target {
        ConvertTarget::Mp3 => args.extend(
            ["-vn", "-codec:a", "libmp3lame", "-q:a", "4"]
                .iter()
                .map(|s| s.to_string()),
        ),
        ConvertTarget::Mp4 => args.extend(["-c", "copy"].iter().map(|s| s.to_string())),
    }
    args.push(output.display().to_string());
    let result = std::process::Command::new(&ffmpeg)
        .args(&args)
        .output()
        .map_err(CcdmError::from)?;
    if result.status.success() {
        Ok(output)
    } else {
        let tail = String::from_utf8_lossy(&result.stderr);
        let tail: Vec<&str> = tail.lines().collect();
        let start = tail.len().saturating_sub(5);
        Err(CcdmError::Other(format!(
            "ffmpeg failed: {}",
            tail[start..].join(" | ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_roundtrip() {
        assert_eq!(ConvertTarget::parse("MP3"), Some(ConvertTarget::Mp3));
        assert_eq!(ConvertTarget::parse("mp4"), Some(ConvertTarget::Mp4));
        assert_eq!(ConvertTarget::parse("avi"), None);
        assert_eq!(ConvertTarget::Mp3.extension(), "mp3");
    }

    #[test]
    fn output_beside_input() {
        let out = output_path(Path::new("/dl/a.mkv"), ConvertTarget::Mp3);
        assert_eq!(out, PathBuf::from("/dl/a.mp3"));
    }

    #[test]
    fn convertible_matrix() {
        assert!(convertible_to_mp3("movie.MKV"));
        assert!(convertible_to_mp3("song.flac"));
        assert!(!convertible_to_mp3("doc.pdf"));
        assert!(!convertible_to_mp3("noext"));
    }
}
