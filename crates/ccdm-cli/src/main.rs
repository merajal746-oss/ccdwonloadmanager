//! `ccdm-cli`: headless download manager (queue + one-shot downloading).
//!
//! ```sh
//! ccdm-cli probe <URL>
//! ccdm-cli download <URL> [--output out.bin] [--connections 8]
//! ccdm-cli add <URL> [--name file.zip]
//! ccdm-cli list
//! ccdm-cli start [--id ID] [--connections 8]
//! ```
//!
//! The queue is persisted as JSON (see `ccdm_core::Store`), so `add`/`list`
//!/`start` survive restarts — the mini equivalent of XDM's queue manager.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use anyhow::Context;
use clap::{Parser, Subcommand};

use ccdm_core::convert::ConvertTarget;
use ccdm_core::i18n;
use ccdm_core::model::{resolve_dest, sanitize_file_name};
use ccdm_core::{
    http, media, AppConfig, Category, DownloadEntry, DownloadStatus, SharedLimiter, SpeedLimiter,
    Store,
};

#[derive(Debug, Parser)]
#[command(
    name = "ccdm-cli",
    version,
    about = "ccdwonloadmanager headless downloader"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Show what the server advertises (size, name, range support).
    Probe {
        /// URL to inspect.
        url: String,
    },
    /// Download a file right now (segmented, resumable).
    Download {
        /// URL to download.
        url: String,
        /// Output file (default: probed name inside the download dir).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Max connections/segments (default: config value).
        #[arg(short, long)]
        connections: Option<usize>,
    },
    /// Probe a URL and remember it in the persisted queue.
    Add {
        /// URL to remember.
        url: String,
        /// File name override (sanitized automatically).
        #[arg(short, long)]
        name: Option<String>,
        /// Connections for this entry (default: config value).
        #[arg(short, long)]
        connections: Option<usize>,
    },
    /// Show the persisted queue.
    List,
    /// Run queued downloads (with resume + transient-error retries).
    Start {
        /// Only run this entry id (see `list`).
        #[arg(short, long)]
        id: Option<String>,
        /// Max connections/segments (default: config value).
        #[arg(short, long)]
        connections: Option<usize>,
        /// Ignore the configured schedule window.
        #[arg(long)]
        force: bool,
        /// Wait until the schedule window opens instead of exiting.
        #[arg(long)]
        wait: bool,
        /// Power off the machine when the queue drains cleanly.
        #[arg(long)]
        shutdown: bool,
    },
    /// Convert a media file with system ffmpeg.
    Convert {
        /// File to convert.
        file: PathBuf,
        /// Target format (mp3|mp4).
        #[arg(short, long, default_value = "mp3")]
        to: String,
    },
    /// Check GitHub releases for a newer version.
    UpdateCheck {
        /// Override the configured update_repo (owner/name).
        #[arg(long)]
        repo: Option<String>,
    },
    /// Show or set the UI language.
    Lang {
        /// Language code to use (needs lang/<code>.json unless en).
        #[arg(short, long)]
        set: Option<String>,
    },
    /// Resolve a video page (YouTube & co.) via yt-dlp and download it.
    Video {
        /// Watch-page URL.
        url: String,
        /// Output file (default: video title inside the download dir).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Quality: best|1080p|720p|480p|audio (or a custom yt-dlp -f spec).
        #[arg(short, long, default_value = "best")]
        quality: String,
    },
    /// Download yt-dlp (+ffmpeg on Windows) so video pages just work.
    Setup,
}

/// Stable-enough unique id without extra dependencies.
fn next_id(n: usize) -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("dl-{ms}-{n}")
}

/// Progress printer shared by `download`/`start`; also records totals so
/// callers can persist them afterwards.
fn make_progress() -> (
    Arc<AtomicU64>,
    Arc<AtomicU64>,
    impl Fn(u64, Option<u64>) + Send + Sync + Clone + 'static,
) {
    let last = Arc::new(AtomicU64::new(u64::MAX));
    let got = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    let printer = {
        let last = Arc::clone(&last);
        let got = Arc::clone(&got);
        let total = Arc::clone(&total);
        move |d: u64, t: Option<u64>| {
            got.store(d, Ordering::Relaxed);
            if let Some(x) = t {
                total.store(x, Ordering::Relaxed);
            }
            match t {
                Some(x) if x > 0 => {
                    let pct = d.min(x) * 100 / x;
                    if pct != last.swap(pct, Ordering::Relaxed) {
                        eprintln!("  {pct:3}%  {d}/{x} bytes");
                    }
                }
                _ => eprintln!("  {d} bytes"),
            }
        }
    };
    (got, total, printer)
}

/// Segmented download with up to 3 attempts for transient failures
/// (cf. XDM's transient-vs-fatal split). Returns the outcome plus the
/// last observed `(downloaded, total)` for persistence.
async fn run_with_retry(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    segments: usize,
    limiter: Option<SharedLimiter>,
    lang: &str,
) -> (anyhow::Result<()>, u64, Option<u64>) {
    let (got, total, progress) = make_progress();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let res = media::download_auto(
            client,
            url,
            dest,
            segments,
            limiter.clone(),
            None,
            progress.clone(),
        )
        .await;
        match res {
            Ok(()) => {
                let t = total.load(Ordering::Relaxed);
                return (
                    Ok(()),
                    got.load(Ordering::Relaxed),
                    if t > 0 { Some(t) } else { None },
                );
            }
            Err(e) if e.is_transient() && attempt < 3 => {
                let wait = 2u64.pow(attempt);
                eprintln!(
                    "{}",
                    i18n::format(
                        lang,
                        "c.retry",
                        &[
                            ("n", &attempt.to_string()),
                            ("e", &e.to_string()),
                            ("w", &wait.to_string()),
                        ]
                    )
                );
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
            Err(e) => {
                return (
                    Err(anyhow::anyhow!(e.to_string())),
                    got.load(Ordering::Relaxed),
                    None,
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let mut config = match AppConfig::config_path() {
        Some(path) => AppConfig::load(&path).unwrap_or_default(),
        None => AppConfig::default(),
    };
    ccdm_core::i18n::load_available();
    let lang = config.language.clone();
    let client = http::build_client_with(&config).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let limiter = SpeedLimiter::shared(config.speed_limit_kbps, config.enable_speed_limit);

    match cli.command {
        Commands::Probe { url } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!(
                "{}",
                i18n::format(&lang, "c.probe_url", &[("v", &info.final_url)])
            );
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.probe_name",
                    &[("v", &sanitize_file_name(&info.file_name))]
                )
            );
            let unknown = i18n::t(&lang, "c.probe_unknown");
            match info.total_bytes {
                Some(n) => println!(
                    "{}",
                    i18n::format(&lang, "c.probe_size", &[("v", &format!("{n} bytes"))])
                ),
                None => println!(
                    "{}",
                    i18n::format(&lang, "c.probe_size", &[("v", &unknown)])
                ),
            }
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.probe_ranges",
                    &[("v", &info.supports_ranges.to_string())]
                )
            );
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.probe_mime",
                    &[("v", info.content_type.as_deref().unwrap_or("-"))]
                )
            );
            let media_hint = match media::detect_media(&url, info.content_type.as_deref()) {
                Some(kind) => kind.to_string(),
                None => "-".to_string(),
            };
            println!(
                "{}",
                i18n::format(&lang, "c.probe_media", &[("v", &media_hint)])
            );
        }
        Commands::Download {
            url,
            output,
            connections,
        } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let dest = match output {
                Some(p) => p,
                None => resolve_dest(
                    &config.download_dir,
                    &info.file_name,
                    &Category::default_categories(),
                    config.organize_by_category,
                ),
            };
            let segments = connections.unwrap_or(config.max_connections).clamp(1, 32);
            eprintln!(
                "{}",
                i18n::format(
                    &lang,
                    "c.downloading",
                    &[
                        ("url", &info.final_url),
                        ("dest", &dest.display().to_string()),
                        ("n", &segments.to_string()),
                    ]
                )
            );

            let (got, _total, progress) = make_progress();
            media::download_auto(
                &client,
                &info.final_url,
                &dest,
                segments,
                limiter,
                None,
                progress,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .with_context(|| format!("download of {} failed", info.final_url))?;

            let got_bytes = got.load(Ordering::Relaxed).to_string();
            eprintln!(
                "{}",
                i18n::format(
                    &lang,
                    "c.done",
                    &[("dest", &dest.display().to_string()), ("n", &got_bytes)]
                )
            );
        }
        Commands::Add {
            url,
            name,
            connections,
        } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let file_name = match name {
                Some(n) => sanitize_file_name(&n),
                None => sanitize_file_name(&info.file_name),
            };
            let mut store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let id = next_id(store.queue().len());
            let mut entry = DownloadEntry::with_plan(
                id.clone(),
                info.final_url.clone(),
                file_name.clone(),
                info.total_bytes,
                config.max_connections,
            );
            entry.segments = connections;
            store.queue_mut().add(entry);
            store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.added",
                    &[("id", &id), ("file", &file_name), ("url", &info.final_url)]
                )
            );
        }
        Commands::List => {
            let store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if store.queue().is_empty() {
                println!("{}", i18n::t(&lang, "c.empty"));
            }
            for e in store.queue().iter_ordered() {
                let pct = e
                    .progress()
                    .map(|p| format!("{:5.1}%", p * 100.0))
                    .unwrap_or_else(|| "   ---".to_string());
                println!(
                    "{}  {:?}  {}  {}  {}",
                    e.id, e.status, pct, e.file_name, e.url
                );
            }
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.stored",
                    &[("p", &store.path().display().to_string())]
                )
            );
        }
        Commands::Start {
            id,
            connections,
            force,
            wait,
            shutdown,
        } => {
            if let Some(schedule) = &config.schedule {
                if !force && !schedule.allows_now() {
                    let window = schedule.describe();
                    if wait {
                        println!("{}", i18n::format(&lang, "c.waiting", &[("w", &window)]));
                        while !schedule.allows_now() {
                            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                        }
                    } else {
                        println!("{}", i18n::format(&lang, "c.outside", &[("w", &window)]));
                        return Ok(());
                    }
                }
            }
            let mut store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if let Some(one) = &id {
                if store.queue().get(one).is_none() {
                    anyhow::bail!("{}", i18n::format(&lang, "c.unknown", &[("id", one)]));
                }
                if let Some(e) = store.queue_mut().get_mut(one) {
                    if matches!(e.status, DownloadStatus::Paused | DownloadStatus::Failed) {
                        e.status = DownloadStatus::Queued;
                    }
                }
            }
            let ids: Vec<String> = match &id {
                Some(one) => vec![one.clone()],
                None => store
                    .queue()
                    .iter_ordered()
                    .filter(|e| e.status == DownloadStatus::Queued)
                    .map(|e| e.id.clone())
                    .collect(),
            };
            if ids.is_empty() {
                println!("{}", i18n::t(&lang, "c.none"));
                return Ok(());
            }
            let (mut done, mut failed) = (0u32, 0u32);
            for one in ids {
                let (url, file_name, entry_segments) = match store.queue_mut().get_mut(&one) {
                    Some(e) if e.status == DownloadStatus::Queued => {
                        e.status = DownloadStatus::Downloading;
                        (e.url.clone(), e.file_name.clone(), e.segments)
                    }
                    _ => continue,
                };
                let segments = connections
                    .or(entry_segments)
                    .unwrap_or(config.max_connections)
                    .clamp(1, 32);
                store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let dest = resolve_dest(
                    &config.download_dir,
                    &file_name,
                    &Category::default_categories(),
                    config.organize_by_category,
                );
                eprintln!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.starting",
                        &[
                            ("id", &one),
                            ("dest", &dest.display().to_string()),
                            ("n", &segments.to_string()),
                        ]
                    )
                );
                let (res, got, tot) =
                    run_with_retry(&client, &url, &dest, segments, limiter.clone(), &lang).await;
                if let Some(e) = store.queue_mut().get_mut(&one) {
                    e.downloaded_bytes = got;
                    if let Some(t) = tot {
                        e.total_bytes = Some(t);
                    }
                    match &res {
                        Ok(()) => {
                            e.status = DownloadStatus::Finished;
                            done += 1;
                            eprintln!(
                                "{}",
                                i18n::format(
                                    &lang,
                                    "c.row_done",
                                    &[("dest", &dest.display().to_string())]
                                )
                            );
                        }
                        Err(err) => {
                            e.status = DownloadStatus::Failed;
                            failed += 1;
                            eprintln!(
                                "{}",
                                i18n::format(
                                    &lang,
                                    "c.row_fail",
                                    &[
                                        ("dest", &dest.display().to_string()),
                                        ("e", &format!("{err:#}")),
                                    ]
                                )
                            );
                        }
                    }
                }
                store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.summary",
                    &[("ok", &done.to_string()), ("failed", &failed.to_string())]
                )
            );
            if shutdown && failed == 0 && done > 0 {
                println!("{}", i18n::t(&lang, "c.shutting"));
                if let Err(e) = ccdm_core::power::shutdown_host(60) {
                    eprintln!(
                        "{}",
                        i18n::format(&lang, "c.shut_fail", &[("e", &e.to_string())])
                    );
                }
            }
        }
        Commands::Convert { file, to } => {
            let target = ConvertTarget::parse(&to)
                .ok_or_else(|| anyhow::anyhow!("unknown format: {to} (mp3|mp4)"))?;
            eprintln!(
                "{}",
                i18n::format(
                    &lang,
                    "c.converting",
                    &[
                        ("src", &file.display().to_string()),
                        ("fmt", target.extension())
                    ]
                )
            );
            let output = ccdm_core::convert::convert(&file, target)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.converted",
                    &[("out", &output.display().to_string())]
                )
            );
        }
        Commands::UpdateCheck { repo } => {
            let repo = repo
                .or(config.update_repo.clone())
                .ok_or_else(|| anyhow::anyhow!(i18n::t(&lang, "c.upd_none")))?;
            let info = ccdm_core::update::latest_release(&repo, &client)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let current = env!("CARGO_PKG_VERSION");
            if ccdm_core::update::is_newer(current, &info.tag) {
                println!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.upd_new",
                        &[("tag", &info.tag), ("url", &info.url)]
                    )
                );
            } else {
                println!("{}", i18n::format(&lang, "c.upd_cur", &[("v", current)]));
            }
        }
        Commands::Lang { set } => {
            if let Some(code) = set {
                config.language = code.clone();
                match AppConfig::config_path() {
                    Some(path) => {
                        config
                            .save(&path)
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                        println!("{}", i18n::format(&lang, "c.lang_set", &[("l", &code)]));
                    }
                    None => anyhow::bail!("no config dir on this platform"),
                }
            } else {
                println!(
                    "{}",
                    i18n::format(&lang, "c.lang_cur", &[("l", &config.language)])
                );
            }
        }
        Commands::Video {
            url,
            output,
            quality,
        } => {
            let ytdlp = ccdm_core::video::find_ytdlp(config.ytdlp_path.as_deref())
                .ok_or_else(|| anyhow::anyhow!(i18n::t(&lang, "c.yt_noytdlp")))?;
            eprintln!(
                "{}",
                i18n::format(&lang, "c.yt_resolving", &[("url", &url)])
            );
            let spec = ccdm_core::video::full_spec(&quality);
            let media = ccdm_core::video::resolve(&ytdlp, &url, &spec)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            eprintln!(
                "{}",
                i18n::format(&lang, "c.yt_found", &[("title", &media.title)])
            );
            let stem = sanitize_file_name(&media.title);
            let ext = if media.direct_url.is_some() || media.video_url.is_none() {
                media.ext.clone()
            } else {
                "mp4".to_string()
            };
            let dest = match output {
                Some(p) => p,
                None => {
                    let name = format!("{stem}.{ext}");
                    resolve_dest(
                        &config.download_dir,
                        &name,
                        &Category::default_categories(),
                        config.organize_by_category,
                    )
                }
            };
            let segments = config.max_connections.clamp(1, 32);
            if let Some(direct) = &media.direct_url {
                let (res, got, _) =
                    run_with_retry(&client, direct, &dest, segments, limiter.clone(), &lang).await;
                res?;
                eprintln!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.done",
                        &[
                            ("dest", &dest.display().to_string()),
                            ("n", &got.to_string())
                        ]
                    )
                );
            } else if let (Some(video), Some(audio)) = (&media.video_url, &media.audio_url) {
                let vtmp = dest.with_extension("video.tmp");
                let atmp = dest.with_extension("audio.tmp");
                let (video_res, _, _) =
                    run_with_retry(&client, video, &vtmp, segments, limiter.clone(), &lang).await;
                video_res?;
                let (audio_res, _, _) =
                    run_with_retry(&client, audio, &atmp, segments, limiter.clone(), &lang).await;
                audio_res?;
                eprintln!("{}", i18n::t(&lang, "c.yt_muxing"));
                ccdm_core::video::mux_av(&vtmp, &atmp, &dest)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let _ = std::fs::remove_file(&vtmp);
                let _ = std::fs::remove_file(&atmp);
                eprintln!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.row_done",
                        &[("dest", &dest.display().to_string())]
                    )
                );
            } else if let Some(single) = media.video_url.as_deref().or(media.audio_url.as_deref()) {
                let (res, got, _) =
                    run_with_retry(&client, single, &dest, segments, limiter.clone(), &lang).await;
                res?;
                eprintln!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.done",
                        &[
                            ("dest", &dest.display().to_string()),
                            ("n", &got.to_string())
                        ]
                    )
                );
            } else {
                anyhow::bail!("{}", i18n::t(&lang, "c.yt_nostreams"));
            }
        }
        Commands::Setup => {
            use ccdm_core::video;
            let (_got, _total, progress) = make_progress();
            eprintln!(
                "{}",
                i18n::format(&lang, "c.setup_dl", &[("what", "yt-dlp")])
            );
            let path = video::setup_ytdlp(&client, progress)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            config.ytdlp_path = Some(path.display().to_string());
            let (_got, _total, progress) = make_progress();
            match video::setup_ffmpeg(&client, progress).await {
                Ok(ffmpeg) => eprintln!(
                    "{}",
                    i18n::format(
                        &lang,
                        "c.setup_ok",
                        &[("path", &ffmpeg.display().to_string())]
                    )
                ),
                Err(e) => eprintln!("ffmpeg: {e}"),
            }
            match AppConfig::config_path() {
                Some(path) => {
                    config
                        .save(&path)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                }
                None => eprintln!("no config dir; yt-dlp left unregistered"),
            }
            println!(
                "{}",
                i18n::format(
                    &lang,
                    "c.setup_ok",
                    &[("path", &config.ytdlp_path.clone().unwrap_or_default())]
                )
            );
        }
    }
    Ok(())
}
