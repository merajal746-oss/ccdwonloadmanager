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
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use clap::{Parser, Subcommands};

use ccdm_core::model::{resolve_dest, sanitize_file_name};
use ccdm_core::{
    AppConfig, Category, DownloadEntry, DownloadStatus, SharedLimiter, SpeedLimiter, Store, http,
    media,
};

#[derive(Debug, Parser)]
#[command(name = "ccdm-cli", version, about = "ccdwonloadmanager headless downloader")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommands)]
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
    },
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
                eprintln!("  attempt {attempt} failed ({e}); retrying in {wait}s...");
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
    let config = match AppConfig::config_path() {
        Some(path) => AppConfig::load(&path).unwrap_or_default(),
        None => AppConfig::default(),
    };
    let client = http::build_client_with(&config).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let limiter = SpeedLimiter::shared(config.speed_limit_kbps, config.enable_speed_limit);

    match cli.command {
        Commands::Probe { url } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("final url : {}", info.final_url);
            println!("file name : {}", sanitize_file_name(&info.file_name));
            match info.total_bytes {
                Some(n) => println!("size      : {n} bytes"),
                None => println!("size      : unknown"),
            }
            println!("ranges    : {}", info.supports_ranges);
            println!(
                "mime      : {}",
                info.content_type.as_deref().unwrap_or("-")
            );
            let media_hint = match media::detect_media(&url, info.content_type.as_deref()) {
                Some(kind) => kind.to_string(),
                None => "-".to_string(),
            };
            println!("media     : {media_hint}");
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
                "downloading {} -> {} ({} segments)",
                info.final_url,
                dest.display(),
                segments
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

            eprintln!(
                "done: {} ({} bytes)",
                dest.display(),
                got.load(Ordering::Relaxed)
            );
        }
        Commands::Add { url, name } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let file_name = match name {
                Some(n) => sanitize_file_name(&n),
                None => sanitize_file_name(&info.file_name),
            };
            let mut store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let id = next_id(store.queue().len());
            let entry = DownloadEntry::with_plan(
                id.clone(),
                info.final_url.clone(),
                file_name.clone(),
                info.total_bytes,
                config.max_connections,
            );
            store.queue_mut().add(entry);
            store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("added {id}  {file_name}  {}", info.final_url);
        }
        Commands::List => {
            let store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if store.queue().is_empty() {
                println!("queue is empty");
            }
            for e in store.queue().iter_ordered() {
                let pct = e
                    .progress()
                    .map(|p| format!("{:5.1}%", p * 100.0))
                    .unwrap_or_else(|| "   ---".to_string());
                println!("{}  {:?}  {}  {}  {}", e.id, e.status, pct, e.file_name, e.url);
            }
            println!("stored in {}", store.path().display());
        }
        Commands::Start { id, connections } => {
            let mut store = Store::load().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if let Some(one) = &id {
                if store.queue().get(one).is_none() {
                    anyhow::bail!("unknown id: {one}");
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
                println!("nothing queued");
                return Ok(());
            }
            let segments = connections.unwrap_or(config.max_connections).clamp(1, 32);
            let (mut done, mut failed) = (0u32, 0u32);
            for one in ids {
                let (url, file_name) = match store.queue_mut().get_mut(&one) {
                    Some(e) if e.status == DownloadStatus::Queued => {
                        e.status = DownloadStatus::Downloading;
                        (e.url.clone(), e.file_name.clone())
                    }
                    _ => continue,
                };
                store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let dest = resolve_dest(
                    &config.download_dir,
                    &file_name,
                    &Category::default_categories(),
                    config.organize_by_category,
                );
                eprintln!("starting {one} -> {} ({} segments)", dest.display(), segments);
                let (res, got, tot) =
                    run_with_retry(&client, &url, &dest, segments, limiter.clone()).await;
                if let Some(e) = store.queue_mut().get_mut(&one) {
                    e.downloaded_bytes = got;
                    if let Some(t) = tot {
                        e.total_bytes = Some(t);
                    }
                    match &res {
                        Ok(()) => {
                            e.status = DownloadStatus::Finished;
                            done += 1;
                            eprintln!("done: {}", dest.display());
                        }
                        Err(err) => {
                            e.status = DownloadStatus::Failed;
                            failed += 1;
                            eprintln!("failed {}: {err:#}", dest.display());
                        }
                    }
                }
                store.save().map_err(|e| anyhow::anyhow!(e.to_string()))?;
            }
            println!("finished: {done} ok, {failed} failed");
        }
    }
    Ok(())
}
