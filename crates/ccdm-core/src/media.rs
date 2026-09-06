//! HLS (m3u8) and DASH (mpd) support (cf. XDM's adaptive downloaders).
//!
//! Clean-room implementation with dependency-free parsers:
//! line-based for HLS playlists, a minimal tag scanner for the common
//! MPD subset (SegmentTemplate with `@duration`, SegmentList, BaseURL
//! chaining). Encrypted HLS and live/dynamic MPDs report
//! [`CcdmError::Unsupported`](crate::CcdmError::Unsupported) instead of
//! failing obscurely. [`download_auto`] sniffs playlist URLs and otherwise
//! behaves exactly like segmented progressive downloading.

use futures::StreamExt;

use crate::{CancelFlag, CcdmError, Result, SharedLimiter};

/// Streaming media flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Hls,
    Dash,
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hls => "HLS",
            Self::Dash => "DASH",
        })
    }
}

/// Guess from URL extension and/or MIME type (no network involved).
pub fn detect_media(url: &str, content_type: Option<&str>) -> Option<MediaKind> {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        return Some(MediaKind::Hls);
    }
    if path.ends_with(".mpd") {
        return Some(MediaKind::Dash);
    }
    match content_type.unwrap_or("").to_lowercase() {
        ct if ct.contains("mpegurl") => Some(MediaKind::Hls),
        ct if ct.contains("dash+xml") => Some(MediaKind::Dash),
        _ => None,
    }
}

/// Classify already-fetched playlist text.
pub fn classify(text: &str) -> Option<MediaKind> {
    if text.trim_start().starts_with("#EXTM3U") {
        return Some(MediaKind::Hls);
    }
    if text.contains("<MPD") || text.contains("<mpd") {
        return Some(MediaKind::Dash);
    }
    None
}

/// Resolve `reference` (relative or absolute) against `base`.
pub fn resolve_url(base: &str, reference: &str) -> Result<String> {
    if let Ok(absolute) = url::Url::parse(reference) {
        if absolute.has_host() {
            return Ok(absolute.to_string());
        }
    }
    Ok(url::Url::parse(base)
        .map_err(CcdmError::from)?
        .join(reference)
        .map_err(CcdmError::from)?
        .to_string())
}

/// One rendition from an HLS master playlist.
#[derive(Debug, Clone)]
pub struct HlsVariant {
    pub bandwidth: u64,
    pub resolution: Option<(u32, u32)>,
    pub uri: String,
}

/// One entry of an HLS media playlist.
#[derive(Debug, Clone)]
pub struct HlsSegment {
    pub uri: String,
    pub duration: f32,
}

/// Parsed HLS media playlist.
#[derive(Debug, Clone, Default)]
pub struct HlsMedia {
    pub segments: Vec<HlsSegment>,
    /// `#EXT-X-MAP` init fragment for fMP4 playlists.
    pub map_uri: Option<String>,
    /// `#EXT-X-KEY` with anything but `METHOD=NONE` was seen.
    pub encrypted: bool,
}

/// Parse an HLS master playlist; empty when `text` is already a media
/// playlist (no `EXT-X-STREAM-INF` tags).
pub fn parse_master(base: &str, text: &str) -> Result<Vec<HlsVariant>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let mut variants = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(rest) = lines[i].strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = parse_attrs(rest);
            let bandwidth = attr(&attrs, "BANDWIDTH")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let resolution = attr(&attrs, "RESOLUTION").and_then(parse_resolution);
            i += 1;
            while i < lines.len() && lines[i].starts_with('#') {
                i += 1;
            }
            if i < lines.len() {
                variants.push(HlsVariant {
                    bandwidth,
                    resolution,
                    uri: resolve_url(base, lines[i])?,
                });
            }
        }
        i += 1;
    }
    Ok(variants)
}

/// Parse an HLS media playlist (segments, init map, encryption flag).
pub fn parse_media(base: &str, text: &str) -> Result<HlsMedia> {
    let mut media = HlsMedia::default();
    let mut pending_duration: Option<f32> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            pending_duration = rest.split(',').next().and_then(|s| s.trim().parse().ok());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = attr(&parse_attrs(rest), "URI") {
                media.map_uri = Some(resolve_url(base, &uri)?);
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let method = attr(&parse_attrs(rest), "METHOD").unwrap_or_default();
            if method != "NONE" {
                media.encrypted = true;
            }
        } else if line.starts_with('#') {
            continue;
        } else if let Some(duration) = pending_duration.take() {
            media.segments.push(HlsSegment {
                uri: resolve_url(base, line)?,
                duration,
            });
        } else {
            // Lenient: bare URI without EXTINF still downloads.
            media.segments.push(HlsSegment {
                uri: resolve_url(base, line)?,
                duration: 0.0,
            });
        }
    }
    Ok(media)
}

/// One downloadable DASH rendition.
#[derive(Debug, Clone)]
pub struct DashStream {
    pub id: String,
    pub bandwidth: u64,
    pub mime: Option<String>,
    pub init: Option<String>,
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct SegTpl {
    media: Option<String>,
    initialization: Option<String>,
    timescale: u64,
    duration: Option<f64>,
    start_number: u64,
}

#[derive(Debug, Clone, Default)]
struct RepCtx {
    id: String,
    bandwidth: u64,
    mime: Option<String>,
    base: Option<String>,
    tpl: Option<SegTpl>,
    seg_urls: Vec<String>,
    init: Option<String>,
}

#[derive(Debug)]
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    closing: bool,
    self_closing: bool,
}

/// Split `key="v" other='w' bare=3` into pairs (double/single/bare values).
fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len()
            && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/' || bytes[i] == b',')
        {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let key_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        let key = s[key_start..i].to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                out.push((key, s[value_start..i].to_string()));
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' {
                    i += 1;
                }
                out.push((key, s[value_start..i].to_string()));
            }
        } else if !key.is_empty() {
            out.push((key, String::new()));
        }
    }
    out
}

fn attr(attrs: &[(String, String)], key: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// Read `<...>` at `start`; skips comments/prologs (returns `None` tag).
fn read_tag(text: &str, start: usize) -> Option<(Option<Tag>, usize)> {
    let bytes = text.as_bytes();
    if text[start..].starts_with("<!--") {
        let end = text[start..].find("-->")? + start + 3;
        return Some((None, end));
    }
    let mut i = start + 1;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            break;
        }
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let inner = text[start + 1..i].trim();
    let end = i + 1;
    if inner.starts_with('?') || inner.starts_with('!') {
        return Some((None, end));
    }
    if let Some(rest) = inner.strip_prefix('/') {
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        return Some((
            Some(Tag {
                name,
                attrs: Vec::new(),
                closing: true,
                self_closing: false,
            }),
            end,
        ));
    }
    let self_closing = inner.ends_with('/');
    let inner = if self_closing {
        inner[..inner.len() - 1].trim_end()
    } else {
        inner
    };
    let mut parts = inner.splitn(2, |c: char| c.is_ascii_whitespace());
    let name = parts.next().unwrap_or("").to_string();
    let attrs = parse_attrs(parts.next().unwrap_or(""));
    Some((
        Some(Tag {
            name,
            attrs,
            closing: false,
            self_closing,
        }),
        end,
    ))
}

/// `PT1H2M3.5S` (and `P1DT2H`) to seconds.
fn parse_iso8601(value: &str) -> Option<f64> {
    let rest = value
        .trim()
        .strip_prefix('P')
        .or_else(|| value.trim().strip_prefix('p'))?;
    let split = rest.find(['T', 't']);
    let (date, time) = match split {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    let days = parse_num_suffix(date, 'D').unwrap_or(0.0);
    let hours = parse_num_suffix(time, 'H').unwrap_or(0.0);
    let minutes = parse_num_suffix(time, 'M').unwrap_or(0.0);
    let seconds = parse_num_suffix(time, 'S').unwrap_or(0.0);
    if date.is_empty() && time.is_empty() {
        return None;
    }
    Some(days * 86400.0 + hours * 3600.0 + minutes * 60.0 + seconds)
}

/// Number directly before `suffix` (`"2M30S"` + `'M'` → 2).
fn parse_num_suffix(s: &str, suffix: char) -> Option<f64> {
    let i = s.find(suffix)?;
    let bytes = s.as_bytes();
    let mut k = i;
    while k > 0 && (bytes[k - 1].is_ascii_digit() || bytes[k - 1] == b'.') {
        k -= 1;
    }
    if k == i {
        return None;
    }
    s[k..i].parse::<f64>().ok()
}

fn parse_resolution(s: String) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Parse an MPD manifest; supports SegmentTemplate with `@duration`,
/// SegmentList, and BaseURL chaining. Live/dynamic manifests are refused.
pub fn parse_mpd(mpd_url: &str, text: &str) -> Result<Vec<DashStream>> {
    let mut stack: Vec<String> = Vec::new();
    let mut mpd_base: Option<String> = None;
    let mut mpd_dur: Option<f64> = None;
    let mut period_base: Option<String> = None;
    let mut period_dur: Option<f64> = None;
    let mut as_mime: Option<String> = None;
    let mut as_base: Option<String> = None;
    let mut as_tpl: Option<SegTpl> = None;
    let mut as_seg_urls: Vec<String> = Vec::new();
    let mut as_init: Option<String> = None;
    let mut rep: Option<RepCtx> = None;
    let mut in_seglist = false;
    let mut base_capture = false;
    let mut streams = Vec::new();

    let mut pos = 0;
    while let Some(relative) = text[pos..].find('<') {
        let node = text[pos..pos + relative].trim();
        if base_capture && !node.is_empty() {
            let value = node.to_string();
            if stack.iter().any(|e| e == "Representation") {
                if let Some(ctx) = rep.as_mut() {
                    ctx.base = Some(value);
                }
            } else if stack.iter().any(|e| e == "AdaptationSet") {
                as_base = Some(value);
            } else if stack.iter().any(|e| e == "Period") {
                period_base = Some(value);
            } else {
                mpd_base = Some(value);
            }
            base_capture = false;
        }
        let (tag, next) = read_tag(text, pos + relative)
            .ok_or_else(|| CcdmError::Other("malformed MPD manifest".to_string()))?;
        pos = next;
        let Some(tag) = tag else { continue };
        if tag.closing {
            match tag.name.as_str() {
                "Representation" => {
                    if let Some(ctx) = rep.take() {
                        if let Some(stream) = finish_rep(
                            mpd_url,
                            mpd_base.as_deref(),
                            period_base.as_deref(),
                            as_mime.as_deref(),
                            as_base.as_deref(),
                            as_tpl.as_ref(),
                            &as_seg_urls,
                            as_init.as_deref(),
                            period_dur.or(mpd_dur),
                            ctx,
                        )? {
                            streams.push(stream);
                        }
                    }
                }
                "AdaptationSet" => {
                    as_mime = None;
                    as_base = None;
                    as_tpl = None;
                    as_seg_urls.clear();
                    as_init = None;
                    in_seglist = false;
                }
                "Period" => {
                    period_base = None;
                    period_dur = None;
                }
                "SegmentList" => in_seglist = false,
                "BaseURL" => base_capture = false,
                _ => {}
            }
            if stack.last().map(String::as_str) == Some(tag.name.as_str()) {
                stack.pop();
            }
            continue;
        }
        match tag.name.as_str() {
            "MPD" => {
                if attr(&tag.attrs, "type").as_deref() == Some("dynamic") {
                    return Err(CcdmError::Unsupported(
                        "live/dynamic DASH (MPD@type=dynamic)".to_string(),
                    ));
                }
                if let Some(total) =
                    attr(&tag.attrs, "mediaPresentationDuration").and_then(|s| parse_iso8601(&s))
                {
                    mpd_dur = Some(total);
                }
                stack.push(tag.name);
            }
            "Period" => {
                period_dur = attr(&tag.attrs, "duration").and_then(|s| parse_iso8601(&s));
                period_base = None;
                stack.push(tag.name);
            }
            "AdaptationSet" => {
                as_mime = attr(&tag.attrs, "mimeType");
                as_base = None;
                as_tpl = None;
                as_seg_urls.clear();
                as_init = None;
                in_seglist = false;
                stack.push(tag.name);
            }
            "Representation" => {
                let ctx = RepCtx {
                    id: attr(&tag.attrs, "id").unwrap_or_default(),
                    bandwidth: attr(&tag.attrs, "bandwidth")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    mime: attr(&tag.attrs, "mimeType"),
                    ..RepCtx::default()
                };
                if tag.self_closing {
                    if let Some(stream) = finish_rep(
                        mpd_url,
                        mpd_base.as_deref(),
                        period_base.as_deref(),
                        as_mime.as_deref(),
                        as_base.as_deref(),
                        as_tpl.as_ref(),
                        &as_seg_urls,
                        as_init.as_deref(),
                        period_dur.or(mpd_dur),
                        ctx,
                    )? {
                        streams.push(stream);
                    }
                } else {
                    rep = Some(ctx);
                    stack.push(tag.name);
                }
            }
            "SegmentTemplate" => {
                let tpl = SegTpl {
                    media: attr(&tag.attrs, "media"),
                    initialization: attr(&tag.attrs, "initialization"),
                    timescale: attr(&tag.attrs, "timescale")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1),
                    duration: attr(&tag.attrs, "duration").and_then(|s| s.parse().ok()),
                    start_number: attr(&tag.attrs, "startNumber")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1),
                };
                if stack.iter().any(|e| e == "Representation") {
                    if let Some(ctx) = rep.as_mut() {
                        ctx.tpl = Some(tpl);
                    }
                } else {
                    as_tpl = Some(tpl);
                }
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
            "SegmentList" => {
                in_seglist = true;
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
            "SegmentURL" => {
                if in_seglist {
                    if let Some(url) = attr(&tag.attrs, "media") {
                        if let Some(ctx) = rep.as_mut() {
                            ctx.seg_urls.push(url);
                        } else {
                            as_seg_urls.push(url);
                        }
                    }
                }
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
            "Initialization" => {
                if let Some(url) = attr(&tag.attrs, "sourceURL") {
                    if let Some(ctx) = rep.as_mut() {
                        ctx.init = Some(url);
                    } else if in_seglist {
                        as_init = Some(url);
                    }
                }
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
            "BaseURL" => {
                base_capture = !tag.self_closing;
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
            _ => {
                if !tag.self_closing {
                    stack.push(tag.name);
                }
            }
        }
    }
    // Salvage a representation the manifest never closed.
    if let Some(ctx) = rep.take() {
        if let Some(stream) = finish_rep(
            mpd_url,
            mpd_base.as_deref(),
            period_base.as_deref(),
            as_mime.as_deref(),
            as_base.as_deref(),
            as_tpl.as_ref(),
            &as_seg_urls,
            as_init.as_deref(),
            period_dur.or(mpd_dur),
            ctx,
        )? {
            streams.push(stream);
        }
    }
    Ok(streams)
}

#[allow(clippy::too_many_arguments)]
fn finish_rep(
    mpd_url: &str,
    mpd_base: Option<&str>,
    period_base: Option<&str>,
    as_mime: Option<&str>,
    as_base: Option<&str>,
    as_tpl: Option<&SegTpl>,
    as_seg_urls: &[String],
    as_init: Option<&str>,
    total_dur: Option<f64>,
    rep: RepCtx,
) -> Result<Option<DashStream>> {
    if rep.id.is_empty()
        && rep.bandwidth == 0
        && rep.base.is_none()
        && rep.tpl.is_none()
        && rep.seg_urls.is_empty()
    {
        return Ok(None);
    }
    let mut base = mpd_url.to_string();
    for part in [mpd_base, period_base, as_base, rep.base.as_deref()]
        .into_iter()
        .flatten()
    {
        base = resolve_url(&base, part)?;
    }
    let mime = rep.mime.or_else(|| as_mime.map(str::to_string));
    let seg_urls = if rep.seg_urls.is_empty() {
        as_seg_urls
    } else {
        &rep.seg_urls
    };
    if !seg_urls.is_empty() {
        let segments = seg_urls
            .iter()
            .map(|u| resolve_url(&base, u))
            .collect::<Result<Vec<_>>>()?;
        let init = rep
            .init
            .as_deref()
            .or(as_init)
            .map(|u| resolve_url(&base, u))
            .transpose()?;
        return Ok(Some(DashStream {
            id: rep.id,
            bandwidth: rep.bandwidth,
            mime,
            init,
            segments,
        }));
    }
    if let Some(tpl) = rep.tpl.as_ref().or(as_tpl) {
        let media = tpl
            .media
            .as_deref()
            .ok_or_else(|| CcdmError::Unsupported("SegmentTemplate without @media".to_string()))?;
        let seg_dur = tpl
            .duration
            .filter(|d| *d > 0.0)
            .map(|d| d / tpl.timescale.max(1) as f64)
            .ok_or_else(|| {
                CcdmError::Unsupported("SegmentTemplate without @duration".to_string())
            })?;
        let total = total_dur.ok_or_else(|| {
            CcdmError::Unsupported(
                "cannot determine DASH segment count (no Period duration)".to_string(),
            )
        })?;
        let count = (total / seg_dur).ceil() as u64;
        if count == 0 || count > 100_000 {
            return Err(CcdmError::Unsupported(
                "implausible DASH segment count".to_string(),
            ));
        }
        let fill = |template: &str, n: u64| {
            template
                .replace("$Number$", &n.to_string())
                .replace("$RepresentationID$", &rep.id)
        };
        let init = tpl
            .initialization
            .as_deref()
            .map(|t| resolve_url(&base, &fill(t, tpl.start_number)))
            .transpose()?;
        let mut segments = Vec::with_capacity(count as usize);
        for i in 0..count {
            segments.push(resolve_url(&base, &fill(media, tpl.start_number + i))?);
        }
        return Ok(Some(DashStream {
            id: rep.id,
            bandwidth: rep.bandwidth,
            mime,
            init,
            segments,
        }));
    }
    let init = rep
        .init
        .as_deref()
        .or(as_init)
        .map(|u| resolve_url(&base, u))
        .transpose()?;
    Ok(Some(DashStream {
        id: rep.id,
        bandwidth: rep.bandwidth,
        mime,
        init,
        segments: vec![base],
    }))
}

/// Prefer the richest video rendition, else the richest overall.
fn pick_dash(streams: &[DashStream]) -> Option<&DashStream> {
    streams
        .iter()
        .filter(|s| {
            s.mime
                .as_deref()
                .map(|m| m.starts_with("video/"))
                .unwrap_or(false)
        })
        .max_by_key(|s| s.bandwidth)
        .or_else(|| streams.iter().max_by_key(|s| s.bandwidth))
}

/// Richest audio-only rendition, if the manifest labels one.
fn pick_audio(streams: &[DashStream]) -> Option<&DashStream> {
    streams
        .iter()
        .filter(|s| {
            s.mime
                .as_deref()
                .map(|m| m.starts_with("audio/"))
                .unwrap_or(false)
        })
        .max_by_key(|s| s.bandwidth)
}

/// Fetch a (small) text playlist, refusing oversized bodies.
async fn fetch_text(client: &reqwest::Client, url: &str, max_bytes: u64) -> Result<String> {
    let resp = client.get(url).send().await.map_err(CcdmError::from)?;
    if !resp.status().is_success() {
        return Err(CcdmError::Http(format!(
            "playlist fetch failed with HTTP {}",
            resp.status()
        )));
    }
    let mut buf = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(CcdmError::from)?;
        buf.extend_from_slice(&bytes);
        if buf.len() as u64 > max_bytes {
            return Err(CcdmError::Other("playlist too large".to_string()));
        }
    }
    String::from_utf8(buf).map_err(|e| CcdmError::Other(format!("playlist is not UTF-8: {e}")))
}

/// Append one media segment, honoring cancel/speed/progress.
async fn append_url<F>(
    client: &reqwest::Client,
    url: &str,
    file: &mut tokio::fs::File,
    limiter: &Option<SharedLimiter>,
    cancel: &Option<CancelFlag>,
    downloaded: &mut u64,
    progress: &F,
) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    use tokio::io::AsyncWriteExt;
    let resp = client.get(url).send().await.map_err(CcdmError::from)?;
    if !resp.status().is_success() {
        return Err(CcdmError::Http(format!(
            "segment fetch failed with HTTP {}",
            resp.status()
        )));
    }
    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(CcdmError::from)?;
        if let Some(flag) = cancel {
            flag.check()?;
        }
        file.write_all(&bytes).await.map_err(CcdmError::from)?;
        *downloaded += bytes.len() as u64;
        progress(*downloaded, None);
        if let Some(limiter) = limiter {
            limiter.lock().await.throttle(*downloaded).await;
        }
    }
    Ok(())
}

/// Download an HLS/DASH URL into `dest` (extension fixed to `.ts`/`.mp4`
/// when the URL itself ends in a playlist suffix).
pub async fn download_media<F>(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    limiter: Option<SharedLimiter>,
    cancel: Option<CancelFlag>,
    progress: F,
) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    use tokio::io::AsyncWriteExt;
    let text = fetch_text(client, url, 8 * 1024 * 1024).await?;
    let kind =
        classify(&text).ok_or_else(|| CcdmError::Other(format!("not a media playlist: {url}")))?;
    let mut dest = dest.to_path_buf();
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ["m3u8", "m3u", "mpd"].contains(&ext.as_str()) {
        dest.set_extension(match kind {
            MediaKind::Hls => "ts",
            MediaKind::Dash => "mp4",
        });
    }
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(CcdmError::from)?;
        }
    }
    let mut file = tokio::fs::File::create(&dest)
        .await
        .map_err(CcdmError::from)?;
    let mut downloaded = 0u64;
    progress(0, None);
    match kind {
        MediaKind::Hls => {
            let variants = parse_master(url, &text)?;
            let (media_text, base) = if variants.is_empty() {
                (text, url.to_string())
            } else {
                let best = variants
                    .iter()
                    .max_by_key(|v| v.bandwidth)
                    .ok_or_else(|| CcdmError::Other("empty HLS master".to_string()))?;
                (
                    fetch_text(client, &best.uri, 8 * 1024 * 1024).await?,
                    best.uri.clone(),
                )
            };
            let media = parse_media(&base, &media_text)?;
            if media.encrypted {
                return Err(CcdmError::Unsupported(
                    "encrypted HLS (EXT-X-KEY) is not supported".to_string(),
                ));
            }
            if media.segments.is_empty() {
                return Err(CcdmError::Other("empty HLS playlist".to_string()));
            }
            if let Some(map) = &media.map_uri {
                append_url(
                    client,
                    map,
                    &mut file,
                    &limiter,
                    &cancel,
                    &mut downloaded,
                    &progress,
                )
                .await?;
            }
            for segment in &media.segments {
                append_url(
                    client,
                    &segment.uri,
                    &mut file,
                    &limiter,
                    &cancel,
                    &mut downloaded,
                    &progress,
                )
                .await?;
            }
        }
        MediaKind::Dash => {
            let streams = parse_mpd(url, &text)?;
            let best = pick_dash(&streams)
                .ok_or_else(|| CcdmError::Other("no usable DASH representations".to_string()))?;
            if best.segments.is_empty() {
                return Err(CcdmError::Other("empty DASH stream".to_string()));
            }
            let mux_audio = pick_audio(&streams)
                .filter(|a| a.id != best.id && !a.segments.is_empty())
                .filter(|_| crate::convert::ffmpeg_binary().is_some());
            if let Some(audio) = mux_audio {
                let audio_tmp = dest.with_extension("audio.tmp");
                let mut audio_file = tokio::fs::File::create(&audio_tmp)
                    .await
                    .map_err(CcdmError::from)?;
                if let Some(init) = &audio.init {
                    append_url(
                        client,
                        init,
                        &mut audio_file,
                        &limiter,
                        &cancel,
                        &mut downloaded,
                        &progress,
                    )
                    .await?;
                }
                for segment in &audio.segments {
                    append_url(
                        client,
                        segment,
                        &mut audio_file,
                        &limiter,
                        &cancel,
                        &mut downloaded,
                        &progress,
                    )
                    .await?;
                }
                audio_file.flush().await.map_err(CcdmError::from)?;
                drop(file);
                let muxed = dest.with_extension("muxed.mp4");
                crate::video::mux_av(&dest, &audio_tmp, &muxed)?;
                let _ = tokio::fs::remove_file(&audio_tmp).await;
                tokio::fs::rename(&muxed, &dest)
                    .await
                    .map_err(CcdmError::from)?;
                progress(downloaded, None);
                return Ok(());
            }
            if let Some(init) = &best.init {
                append_url(
                    client,
                    init,
                    &mut file,
                    &limiter,
                    &cancel,
                    &mut downloaded,
                    &progress,
                )
                .await?;
            }
            for segment in &best.segments {
                append_url(
                    client,
                    segment,
                    &mut file,
                    &limiter,
                    &cancel,
                    &mut downloaded,
                    &progress,
                )
                .await?;
            }
        }
    }
    file.flush().await.map_err(CcdmError::from)?;
    progress(downloaded, None);
    Ok(())
}

/// Segmented progressive download, unless the URL looks like a media
/// playlist — then HLS/DASH takes over. Same signature as
/// [`crate::http::download_segmented`], so callers switch trivially.
pub async fn download_auto<F>(
    client: &reqwest::Client,
    url: &str,
    dest: &std::path::Path,
    segments: usize,
    limiter: Option<SharedLimiter>,
    cancel: Option<CancelFlag>,
    progress: F,
) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync + 'static,
{
    if detect_media(url, None).is_some() {
        return download_media(client, url, dest, limiter, cancel, progress).await;
    }
    crate::http::download_segmented(client, url, dest, segments, limiter, cancel, progress).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\nhi/index.m3u8\n";

    #[test]
    fn master_parses_variants() {
        let variants = parse_master("https://cdn.example.com/vod/master.m3u8", MASTER).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].bandwidth, 800000);
        assert_eq!(variants[0].resolution, Some((640, 360)));
        assert_eq!(
            variants[0].uri,
            "https://cdn.example.com/vod/low/index.m3u8"
        );
        assert_eq!(variants[1].uri, "https://cdn.example.com/vod/hi/index.m3u8");
    }

    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:9.009,\nseg0.m4s\n#EXTINF:9.009,\nseg1.m4s\n#EXT-X-ENDLIST\n";

    #[test]
    fn media_parses_segments_and_map() {
        let media = parse_media("https://h.com/a/pl.m3u8", MEDIA).unwrap();
        assert_eq!(media.segments.len(), 2);
        assert!((media.segments[0].duration - 9.009).abs() < 0.001);
        assert_eq!(media.segments[1].uri, "https://h.com/a/seg1.m4s");
        assert_eq!(media.map_uri.as_deref(), Some("https://h.com/a/init.mp4"));
        assert!(!media.encrypted);
    }

    #[test]
    fn media_detects_encryption() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:8.0,\ns0.ts\n";
        assert!(parse_media("https://h.com/x.m3u8", text).unwrap().encrypted);
    }

    #[test]
    fn classify_markers() {
        assert_eq!(classify("#EXTM3U\nfoo"), Some(MediaKind::Hls));
        assert_eq!(classify("<?xml?><MPD foo"), Some(MediaKind::Dash));
        assert_eq!(classify("<html>nope"), None);
    }

    #[test]
    fn detect_hints() {
        assert_eq!(
            detect_media("https://h/v.m3u8?tok=1", None),
            Some(MediaKind::Hls)
        );
        assert_eq!(detect_media("https://h/v.mpd", None), Some(MediaKind::Dash));
        assert_eq!(detect_media("https://h/v.mp4", Some("video/mp4")), None);
        assert_eq!(
            detect_media("https://h/v", Some("application/vnd.apple.mpegurl")),
            Some(MediaKind::Hls)
        );
        assert_eq!(
            detect_media("https://h/v", Some("application/dash+xml")),
            Some(MediaKind::Dash)
        );
    }

    const MPD_TEMPLATE: &str = r#"<?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT20S"><Period duration="PT20S"><AdaptationSet mimeType="video/mp4"><SegmentTemplate timescale="1" duration="10" initialization="init-$RepresentationID$.mp4" media="seg-$RepresentationID$-$Number$.m4s" startNumber="1"/><Representation id="v1" bandwidth="1000000"/></AdaptationSet></Period></MPD>"#;

    #[test]
    fn mpd_template_expands() {
        let streams = parse_mpd("https://cdn.example.com/dash/manifest.mpd", MPD_TEMPLATE).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].segments,
            vec![
                "https://cdn.example.com/dash/seg-v1-1.m4s",
                "https://cdn.example.com/dash/seg-v1-2.m4s",
            ]
        );
        assert_eq!(
            streams[0].init.as_deref(),
            Some("https://cdn.example.com/dash/init-v1.mp4")
        );
        assert_eq!(streams[0].bandwidth, 1000000);
    }

    const MPD_LIST: &str = r#"<MPD type="static"><Period><AdaptationSet mimeType="video/mp4"><BaseURL>video/</BaseURL><SegmentList><Initialization sourceURL="init.mp4"/><SegmentURL media="s1.m4s"/><SegmentURL media="s2.m4s"/></SegmentList><Representation id="v" bandwidth="500"/></AdaptationSet></Period></MPD>"#;

    #[test]
    fn mpd_list_resolves() {
        let streams = parse_mpd("https://h.com/m/manifest.mpd", MPD_LIST).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].segments,
            vec![
                "https://h.com/m/video/s1.m4s",
                "https://h.com/m/video/s2.m4s",
            ]
        );
        assert_eq!(
            streams[0].init.as_deref(),
            Some("https://h.com/m/video/init.mp4")
        );
    }

    #[test]
    fn mpd_audio_picked_separately() {
        let text = r#"<MPD type="static"><Period duration="PT20S"><AdaptationSet mimeType="video/mp4"><SegmentTemplate timescale="1" duration="10" media="v-$Number$.m4s"/><Representation id="v" bandwidth="800"/></AdaptationSet><AdaptationSet mimeType="audio/mp4"><SegmentTemplate timescale="1" duration="10" media="a-$Number$.m4s"/><Representation id="a" bandwidth="128"/></AdaptationSet></Period></MPD>"#;
        let streams = parse_mpd("https://h.com/m/manifest.mpd", text).unwrap();
        assert_eq!(streams.len(), 2);
        let video = pick_dash(&streams).unwrap();
        assert_eq!(video.id, "v");
        assert_eq!(video.segments.len(), 2);
        let audio = pick_audio(&streams).unwrap();
        assert_eq!(audio.id, "a");
        assert_eq!(
            audio.segments,
            vec![
                "https://h.com/m/a-1.m4s",
                "https://h.com/m/a-2.m4s",
            ]
        );
    }

    #[test]
    fn mpd_dynamic_rejected() {
        let text = r#"<MPD type="dynamic" minimumUpdatePeriod="PT5S"><Period><AdaptationSet><Representation id="a" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        assert!(matches!(
            parse_mpd("https://h.com/x.mpd", text),
            Err(CcdmError::Unsupported(_))
        ));
    }

    #[test]
    fn iso8601_durations() {
        assert_eq!(parse_iso8601("PT20S"), Some(20.0));
        assert_eq!(parse_iso8601("PT1H2M3S"), Some(3723.0));
        assert!((parse_iso8601("PT9.5S").unwrap() - 9.5).abs() < 1e-9);
        assert_eq!(parse_iso8601("P1DT2H"), Some(93600.0));
        assert_eq!(parse_iso8601("nope"), None);
    }

    #[test]
    fn resolve_absolute_and_relative() {
        assert_eq!(
            resolve_url("https://h.com/a/b.m3u8", "seg.ts").unwrap(),
            "https://h.com/a/seg.ts"
        );
        assert_eq!(
            resolve_url("https://h.com/a/b.m3u8", "https://cdn.com/x.ts").unwrap(),
            "https://cdn.com/x.ts"
        );
    }
}
