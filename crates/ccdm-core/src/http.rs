//! HTTP probing and (segmented) downloading.
//!
//! The engine room: `probe` learns the size/name/range support of a URL
//! (like XDM's probing/download-type detection), `download_with_resume`
//! does a single resumable connection, and `download_segmented` fans out
//! over up to N range connections and assembles the parts — the Rust
//! equivalent of XDM's progressive/adaptive downloaders.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::model::guess_file_name;
use crate::segmented::plan_segments;
use crate::speed_limiter::SharedLimiter;
use crate::{CancelFlag, CcdmError, Result};

/// Build the shared HTTP client with default settings.
pub fn build_client() -> Result<reqwest::Client> {
    build_client_with(&crate::AppConfig::default())
}

/// Build the HTTP client honoring `AppConfig` (proxy, ...).
pub fn build_client_with(config: &crate::AppConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent("ccdm/0.1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(3600));
    if let Some(proxy) = config
        .proxy_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        builder =
            builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| CcdmError::Other(e.to_string()))?);
    }
    builder.build().map_err(CcdmError::from)
}

/// What `probe` learned about a URL.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// URL after redirects.
    pub final_url: String,
    /// Best-guess file name.
    pub file_name: String,
    /// Total size, when the server reveals it.
    pub total_bytes: Option<u64>,
    /// Whether `Range` requests work (needed for segmenting/resume).
    pub supports_ranges: bool,
    /// MIME type, when advertised.
    pub content_type: Option<String>,
}

/// Learn size, name and range support of `url`.
///
/// Tries `HEAD` first, then falls back to a 1-byte ranged `GET`
/// (`Range: bytes=0-0`) because many servers/CDNs answer HEAD poorly.
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<ProbeResult> {
    url::Url::parse(url).map_err(CcdmError::from)?;

    // 1) HEAD attempt.
    if let Ok(resp) = client.head(url).send().await {
        if resp.status().is_success() {
            let final_url = resp.url().to_string();
            let headers = resp.headers().clone();
            let total = content_length_of(&headers);
            let ranges = accepts_ranges(&headers);
            let name =
                disposition_filename(&headers).unwrap_or_else(|| guess_file_name(&final_url));
            let ctype = headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or("").trim().to_string());
            // A successful HEAD that already promises ranges is enough.
            if total.is_some() || ranges {
                return Ok(ProbeResult {
                    final_url,
                    file_name: name,
                    total_bytes: total,
                    supports_ranges: ranges,
                    content_type: ctype.filter(|s| !s.is_empty()),
                });
            }
        }
    }

    // 2) 1-byte ranged GET fallback.
    let resp = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(CcdmError::from)?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(CcdmError::Http(format!(
            "probe failed with HTTP {}",
            resp.status()
        )));
    }
    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let total = content_range_total(&headers).or_else(|| content_length_of(&headers));
    let name = disposition_filename(&headers).unwrap_or_else(|| guess_file_name(&final_url));
    let ctype = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or("").trim().to_string());
    Ok(ProbeResult {
        final_url,
        file_name: name,
        total_bytes: total,
        supports_ranges: partial || accepts_ranges(&headers),
        content_type: ctype.filter(|s| !s.is_empty()),
    })
}

/// Single-connection download with resume.
///
/// If `dest` already exists, its length is used as the resume offset
/// (via `Range`). When the server answers `200` instead of `206` the
/// partial file is discarded and the download restarts from zero.
/// `progress(downloaded, total)` is called after every chunk; `limiter`,
/// when present, enforces the global speed cap (cf. XDM SpeedLimiter);
/// `cancel`, when present, aborts at the next chunk boundary (pause).
pub async fn download_with_resume<F>(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    limiter: Option<SharedLimiter>,
    cancel: Option<CancelFlag>,
    progress: F,
) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    let resume_from = tokio::fs::metadata(dest)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let mut req = client.get(url);
    if resume_from > 0 {
        req = req.header(RANGE, format!("bytes={resume_from}-"));
    }
    let resp = req.send().await.map_err(CcdmError::from)?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(CcdmError::Http(format!(
            "download failed with HTTP {}",
            resp.status()
        )));
    }
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if resume_from > 0 && !partial {
        // Server ignored Range: restart from scratch.
        tokio::fs::remove_file(dest)
            .await
            .map_err(CcdmError::from)?;
    }
    let base = if partial { resume_from } else { 0 };
    let total = resp.content_length().map(|r| base + r);

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CcdmError::from)?;
        }
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(!partial)
        .append(partial)
        .write(true)
        .open(dest)
        .await
        .map_err(CcdmError::from)?;

    let mut downloaded = base;
    progress(downloaded, total);
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(CcdmError::from)?;
        if let Some(flag) = &cancel {
            flag.check()?;
        }
        file.write_all(&bytes).await.map_err(CcdmError::from)?;
        downloaded += bytes.len() as u64;
        progress(downloaded, total);
        if let Some(lim) = &limiter {
            lim.lock().await.throttle(downloaded).await;
        }
    }
    file.flush().await.map_err(CcdmError::from)?;
    Ok(())
}

/// Multi-connection segmented download with per-part resume.
///
/// Splits the file via [`plan_segments`], downloads each range into
/// `<dest>.part<i>` files (resuming partial parts), then concatenates
/// them into `dest` and deletes the parts. Falls back to
/// [`download_with_resume`] when the size is unknown, ranges are
/// unsupported, or `segments <= 1`.
pub async fn download_segmented<F>(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    segments: usize,
    limiter: Option<SharedLimiter>,
    cancel: Option<CancelFlag>,
    progress: F,
) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync + 'static,
{
    let info = probe(client, url).await?;
    let total = match info.total_bytes {
        Some(t) if t > 0 && info.supports_ranges && segments > 1 => t,
        _ => {
            return download_with_resume(client, &info.final_url, dest, limiter, cancel, progress)
                .await;
        }
    };

    let ranges = plan_segments(total, segments);
    let progress = Arc::new(progress);
    let downloaded = Arc::new(AtomicU64::new(0));

    // Account for already-complete parts (cross-run resume).
    let mut pending: Vec<(usize, u64, u64)> = Vec::new();
    for (i, (start, end)) in ranges.iter().copied().enumerate() {
        let part = part_path(dest, i);
        let have = tokio::fs::metadata(&part)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let want = end - start + 1;
        if have > want {
            // Stale part (e.g. the file on the server shrank): restart it,
            // otherwise assembling would silently corrupt the download.
            let _ = tokio::fs::remove_file(&part).await;
            pending.push((i, start, end));
        } else if have == want {
            downloaded.fetch_add(want, Ordering::Relaxed);
        } else {
            downloaded.fetch_add(have, Ordering::Relaxed);
            pending.push((i, start + have, end));
        }
    }
    progress(downloaded.load(Ordering::Relaxed), Some(total));

    let mut handles = Vec::with_capacity(pending.len());
    for (i, start, end) in pending {
        let client = client.clone();
        let url = info.final_url.clone();
        let part = part_path(dest, i);
        let progress = Arc::clone(&progress);
        let downloaded = Arc::clone(&downloaded);
        let limiter = limiter.clone();
        let cancel = cancel.clone();
        handles.push(tokio::spawn(async move {
            fetch_range(
                &client,
                &url,
                &part,
                start,
                end,
                total,
                limiter,
                cancel,
                &*progress,
                &downloaded,
            )
            .await
        }));
    }
    for h in handles {
        h.await
            .map_err(|e| CcdmError::Other(format!("segment task panicked: {e}")))??;
    }

    if let Some(flag) = &cancel {
        flag.check()?;
    }
    assemble_parts(dest, ranges.len()).await?;
    progress(total, Some(total));
    Ok(())
}

/// Download one `[start, end]` range (inclusive) into `part`.
#[allow(clippy::too_many_arguments)]
async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    start: u64,
    end: u64,
    total: u64,
    limiter: Option<SharedLimiter>,
    cancel: Option<CancelFlag>,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    downloaded: &AtomicU64,
) -> Result<()> {
    if start > end {
        return Ok(());
    }
    if let Some(parent) = part.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CcdmError::from)?;
        }
    }
    let resp = client
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(CcdmError::from)?;
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(CcdmError::RangeNotSupported);
    }
    let append = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
        > 0;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(!append)
        .append(append)
        .write(true)
        .open(part)
        .await
        .map_err(CcdmError::from)?;

    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(CcdmError::from)?;
        if let Some(flag) = &cancel {
            flag.check()?;
        }
        file.write_all(&bytes).await.map_err(CcdmError::from)?;
        let now = downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
        progress(now, Some(total));
        if let Some(lim) = &limiter {
            lim.lock().await.throttle(now).await;
        }
    }
    file.flush().await.map_err(CcdmError::from)?;
    Ok(())
}

/// Concatenate `<dest>.part*` files into `dest`, then delete the parts.
async fn assemble_parts(dest: &Path, count: usize) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CcdmError::from)?;
        }
    }
    let mut out = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(dest)
        .await
        .map_err(CcdmError::from)?;
    for i in 0..count {
        let part = part_path(dest, i);
        let mut input = tokio::fs::File::open(&part)
            .await
            .map_err(CcdmError::from)?;
        tokio::io::copy(&mut input, &mut out)
            .await
            .map_err(CcdmError::from)?;
    }
    out.flush().await.map_err(CcdmError::from)?;
    drop(out);
    for i in 0..count {
        let part = part_path(dest, i);
        tokio::fs::remove_file(&part)
            .await
            .map_err(CcdmError::from)?;
    }
    Ok(())
}

/// `<dest>.part<i>` sidecar path.
fn part_path(dest: &Path, i: usize) -> PathBuf {
    PathBuf::from(format!("{}.part{i}", dest.display()))
}

fn content_length_of(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn accepts_ranges(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(ACCEPT_RANGES)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase().contains("bytes"))
        .unwrap_or(false)
}

/// Parse `Content-Range: bytes 0-0/12345` -> `12345`.
fn content_range_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let v = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    v.split('/').nth(1)?.trim().parse::<u64>().ok()
}

/// Parse `Content-Disposition: attachment; filename="x.zip"`
/// (also handles RFC 5987 `filename*=UTF-8''x.zip`).
fn disposition_filename(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let v = headers.get(CONTENT_DISPOSITION)?.to_str().ok()?;
    // RFC 5987 encoded form first.
    if let Some(pos) = v.find("filename*=") {
        let rest = v[pos + "filename*=".len()..].trim();
        let name = rest.split('\'').nth(2).unwrap_or(rest);
        let name = name.trim().trim_matches('"').trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let pos = v.find("filename=")?;
    let mut rest = v[pos + "filename=".len()..].trim().to_string();
    if rest.starts_with('"') {
        rest = rest.trim_matches('"').to_string();
    } else {
        rest = rest.split(';').next().unwrap_or("").trim().to_string();
    }
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_path_appends_index() {
        let p = part_path(Path::new("/tmp/a.bin"), 2);
        assert_eq!(p, PathBuf::from("/tmp/a.bin.part2"));
    }

    #[test]
    fn parses_content_range_total() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(CONTENT_RANGE, "bytes 0-0/12345".parse().unwrap());
        assert_eq!(content_range_total(&h), Some(12345));
    }

    #[test]
    fn parses_disposition_names() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            CONTENT_DISPOSITION,
            "attachment; filename=\"x.zip\"".parse().unwrap(),
        );
        assert_eq!(disposition_filename(&h).as_deref(), Some("x.zip"));

        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert(
            CONTENT_DISPOSITION,
            "attachment; filename*=UTF-8''v%C3%ADdeo.mp4"
                .parse()
                .unwrap(),
        );
        assert_eq!(disposition_filename(&h2).as_deref(), Some("v%C3%ADdeo.mp4"));
    }
}
