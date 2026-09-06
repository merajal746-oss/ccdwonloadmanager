//! `ccdm-cli`: headless downloader proving the engine works with no GUI.
//!
//! ```sh
//! ccdm-cli probe <URL>
//! ccdm-cli download <URL> [--output out.bin] [--connections 8]
//! ```

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use clap::{Parser, Subcommands};

use ccdm_core::{AppConfig, http};

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
    /// Download a file (segmented, resumable).
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
    let config = AppConfig::default().normalized();
    let client = http::build_client().map_err(|e| anyhow::anyhow!(e.to_string()))?;

    match cli.command {
        Commands::Probe { url } => {
            let info = http::probe(&client, &url)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("final url : {}", info.final_url);
            println!("file name : {}", info.file_name);
            match info.total_bytes {
                Some(n) => println!("size      : {n} bytes"),
                None => println!("size      : unknown"),
            }
            println!("ranges    : {}", info.supports_ranges);
            println!(
                "mime      : {}",
                info.content_type.as_deref().unwrap_or("-")
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
                None => config.download_dir.join(&info.file_name),
            };
            let segments = connections.unwrap_or(config.max_connections).clamp(1, 32);
            eprintln!(
                "downloading {} -> {} ({} segments)",
                info.final_url,
                dest.display(),
                segments
            );

            let last_pct = Arc::new(AtomicU64::new(u64::MAX));
            let lp = Arc::clone(&last_pct);
            http::download_segmented(&client, &info.final_url, &dest, segments, move |got, total| {
                match total {
                    Some(t) if t > 0 => {
                        let pct = got.min(t) * 100 / t;
                        if pct != lp.swap(pct, Ordering::Relaxed) {
                            eprintln!("  {pct:3}%  {got}/{t} bytes");
                        }
                    }
                    _ => eprintln!("  {got} bytes"),
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
            .with_context(|| format!("download of {} failed", info.final_url))?;

            eprintln!("done: {}", dest.display());
        }
    }
    Ok(())
}
