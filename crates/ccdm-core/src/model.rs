//! Download model: entries, chunks, categories, statuses.
//!
//! Mirrors the ideas behind XDM.Core's `DownloadEntries`, `Category`,
//! `Downloader/Chunk` and `Downloader/Progressive/SegmentState`
//! (clean-room re-implementation, see crate docs).

use serde::{Deserialize, Serialize};

use crate::segmented::plan_segments;

/// Lifecycle of one download (like XDM's download state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DownloadStatus {
    /// Waiting in the queue.
    #[default]
    Queued,
    /// Actively downloading.
    Downloading,
    /// Paused by the user (resumable).
    Paused,
    /// Fully downloaded and assembled.
    Finished,
    /// Failed; see logs / error attached by the caller.
    Failed,
}

/// State of a single chunk/segment (cf. XDM `ChunkState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ChunkState {
    /// Ready to be (re)downloaded.
    #[default]
    Ready,
    /// Being downloaded right now.
    InProgress,
    /// Done.
    Finished,
    /// Failed but worth retrying (timeout, reset, ...).
    FailedTransient,
    /// Failed fatally (e.g. HTTP 404 on this segment).
    FailedFatal,
}

/// State of a progressive segment (cf. XDM `SegmentState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SegmentState {
    /// Not started yet.
    #[default]
    NotStarted,
    /// Currently downloading.
    Downloading,
    /// Done.
    Finished,
    /// Failed.
    Failed,
}

/// One byte-range of a download (cf. XDM `Chunk`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable id, e.g. `"<download-id>#3"`.
    pub id: String,
    /// Source URL (mirrors may differ per chunk in later versions).
    pub url: String,
    /// First byte of this chunk in the final file.
    pub offset: u64,
    /// Length in bytes, if known.
    pub size: Option<u64>,
    /// Bytes already written for this chunk.
    pub downloaded: u64,
    /// Current state.
    pub state: ChunkState,
}

impl Chunk {
    /// Create a fresh `Ready` chunk.
    pub fn new(id: String, url: String, offset: u64, size: Option<u64>) -> Self {
        Self {
            id,
            url,
            offset,
            size,
            downloaded: 0,
            state: ChunkState::Ready,
        }
    }

    /// Bytes still missing, if the size is known.
    pub fn remaining(&self) -> Option<u64> {
        self.size.map(|s| s.saturating_sub(self.downloaded))
    }
}

/// One download job (cf. XDM `DownloadEntry`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadEntry {
    /// Stable unique id.
    pub id: String,
    /// Source URL.
    pub url: String,
    /// Target file name (no directory).
    pub file_name: String,
    /// Current lifecycle state.
    pub status: DownloadStatus,
    /// Total size in bytes, if the server told us.
    pub total_bytes: Option<u64>,
    /// Bytes downloaded across all chunks.
    pub downloaded_bytes: u64,
    /// Segments this download is split into.
    pub chunks: Vec<Chunk>,
    /// Per-entry connection override (None = follow current default).
    pub segments: Option<usize>,
}

impl DownloadEntry {
    /// Create a new queued entry.
    pub fn new(id: String, url: String, file_name: String) -> Self {
        Self {
            id,
            url,
            file_name,
            status: DownloadStatus::Queued,
            total_bytes: None,
            downloaded_bytes: 0,
            chunks: Vec::new(),
            segments: None,
        }
    }

    /// Create a queued entry with `segments` planned chunks for a known
    /// size (the Rust equivalent of XDM's piece planning).
    pub fn with_plan(
        id: String,
        url: String,
        file_name: String,
        total_bytes: Option<u64>,
        segments: usize,
    ) -> Self {
        let mut entry = Self::new(id.clone(), url.clone(), file_name);
        entry.total_bytes = total_bytes;
        if let Some(total) = total_bytes {
            if total > 0 {
                let ranges = plan_segments(total, segments.max(1));
                for (i, (start, end)) in ranges.iter().copied().enumerate() {
                    entry.chunks.push(Chunk::new(
                        format!("{id}#{i}"),
                        url.clone(),
                        start,
                        Some(end - start + 1),
                    ));
                }
            }
        }
        entry
    }

    /// Overall progress in `0.0..=1.0`, if the total size is known.
    pub fn progress(&self) -> Option<f64> {
        match self.total_bytes {
            Some(0) | None => None,
            Some(total) => Some(self.downloaded_bytes.min(total) as f64 / total as f64),
        }
    }

    /// Recompute `downloaded_bytes` from the chunks.
    pub fn sync_from_chunks(&mut self) {
        self.downloaded_bytes = self.chunks.iter().map(|c| c.downloaded).sum();
    }
}

/// Guess a file name from a URL's path, e.g.
/// `https://host/dir/file.zip?x=1` -> `file.zip`.
/// Falls back to `"download.bin"` when nothing usable is found.
pub fn guess_file_name(url: &str) -> String {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return "download.bin".to_string(),
    };
    let last = parsed
        .path_segments()
        .and_then(|mut s| s.rfind(|p| !p.is_empty()))
        .unwrap_or("");
    // Strip any query-like leftovers and percent-decoding issues conservatively.
    let name = last.split(['?', '#']).next().unwrap_or("").trim();
    if name.is_empty() {
        "download.bin".to_string()
    } else {
        name.to_string()
    }
}

/// Unique-enough id without extra dependencies (millis + pid).
pub fn new_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        crate::speed_limiter::now_ms(),
        std::process::id()
    )
}

/// Make a server-provided file name safe to use on Windows and Unix
/// (cf. XDM's file-name helpers): `< > : " / \ | ? *` and control
/// characters become `_`, trailing dots/spaces (illegal on Windows) are
/// stripped, and an empty result falls back to `"download.bin"`.
pub fn sanitize_file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        "download.bin".to_string()
    } else {
        trimmed
    }
}

/// Destination for a file: `<dir>/<Category>/<name>` when organizing is on
/// and the extension matches a category, else `<dir>/<name>`
/// (cf. XDM categories folders). The name is sanitized first.
pub fn resolve_dest(
    download_dir: &std::path::Path,
    file_name: &str,
    categories: &[Category],
    organize: bool,
) -> std::path::PathBuf {
    let safe = sanitize_file_name(file_name);
    if organize {
        if let Some(category) = Category::for_file_name(categories, &safe) {
            return download_dir.join(&category.name).join(&safe);
        }
    }
    download_dir.join(&safe)
}

/// File category used for sorting into subfolders (cf. XDM `Category`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Category {
    /// Display name, e.g. `"Video"`.
    pub name: String,
    /// File extensions (lowercase, without dot).
    pub extensions: Vec<String>,
}

impl Category {
    /// Default set: Documents, Video, Audio, Compressed, Programs, Images.
    pub fn default_categories() -> Vec<Self> {
        let mk = |name: &str, exts: &[&str]| Self {
            name: name.to_string(),
            extensions: exts.iter().map(|s| s.to_string()).collect(),
        };
        vec![
            mk(
                "Documents",
                &[
                    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "odt",
                ],
            ),
            mk(
                "Video",
                &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v"],
            ),
            mk("Audio", &["mp3", "wav", "flac", "ogg", "m4a", "opus"]),
            mk(
                "Compressed",
                &["zip", "rar", "7z", "tar", "gz", "bz2", "xz"],
            ),
            mk(
                "Programs",
                &["exe", "msi", "dmg", "pkg", "deb", "rpm", "apk"],
            ),
            mk(
                "Images",
                &["jpg", "jpeg", "png", "gif", "bmp", "svg", "webp"],
            ),
        ]
    }

    /// Find the category matching a file name's extension.
    pub fn for_file_name<'a>(categories: &'a [Self], file_name: &str) -> Option<&'a Self> {
        let ext = file_name.rsplit('.').next()?.to_lowercase();
        categories.iter().find(|c| c.extensions.contains(&ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_none_without_total() {
        let e = DownloadEntry::new("1".into(), "https://x/y".into(), "y".into());
        assert_eq!(e.progress(), None);
    }

    #[test]
    fn progress_half() {
        let mut e = DownloadEntry::new("1".into(), "https://x/y".into(), "y".into());
        e.total_bytes = Some(100);
        e.downloaded_bytes = 50;
        assert!((e.progress().unwrap() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn guess_name_from_url() {
        assert_eq!(guess_file_name("https://host/dir/file.zip?x=1"), "file.zip");
        assert_eq!(guess_file_name("https://host/"), "download.bin");
        assert_eq!(guess_file_name("not a url"), "download.bin");
    }

    #[test]
    fn category_lookup() {
        let cats = Category::default_categories();
        assert_eq!(
            Category::for_file_name(&cats, "movie.MP4").unwrap().name,
            "Video"
        );
        assert!(Category::for_file_name(&cats, "noext").is_none());
    }

    #[test]
    fn sanitize_replaces_illegal_chars() {
        assert_eq!(
            sanitize_file_name("a<b>:c\"d/e\\f|g?h*i"),
            "a_b__c_d_e_f_g_h_i"
        );
        assert_eq!(sanitize_file_name("trailing...   "), "trailing");
        assert_eq!(sanitize_file_name("..."), "download.bin");
        assert_eq!(sanitize_file_name("  ok-name.zip  "), "ok-name.zip");
    }

    #[test]
    fn with_plan_covers_total() {
        let e = DownloadEntry::with_plan("x".into(), "https://h/f".into(), "f".into(), Some(10), 3);
        assert_eq!(e.chunks.len(), 3);
        let covered: u64 = e.chunks.iter().map(|c| c.size.unwrap()).sum();
        assert_eq!(covered, 10);
        assert_eq!(e.chunks[0].id, "x#0");
    }

    #[test]
    fn resolve_dest_organizes_by_category() {
        let cats = Category::default_categories();
        let dir = std::path::Path::new("/dl");
        assert_eq!(
            resolve_dest(dir, "movie.mkv", &cats, true),
            std::path::PathBuf::from("/dl/Video/movie.mkv")
        );
        assert_eq!(
            resolve_dest(dir, "movie.mkv", &cats, false),
            std::path::PathBuf::from("/dl/movie.mkv")
        );
        assert_eq!(
            resolve_dest(dir, "weird?.sh", &cats, true),
            std::path::PathBuf::from("/dl/weird_.sh")
        );
    }
}
