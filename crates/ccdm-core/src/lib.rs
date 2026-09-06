//! `ccdm-core`: download engine for ccdwonloadmanager.
//!
//! Clean-room Rust port of the *concepts* in XDM.Core (GPL-2.0,
//! <https://github.com/subhra74/xdm>):
//! segmented/multi-connection downloading, chunks, speed limiting,
//! download queue, categories and config. No XDM source is copied here.

pub mod cancel;
pub mod config;
pub mod error;
pub mod http;
pub mod model;
pub mod queue;
pub mod segmented;
pub mod speed_limiter;
pub mod store;

pub use cancel::CancelFlag;
pub use config::AppConfig;
pub use error::{CcdmError, Result};
pub use model::{Category, Chunk, ChunkState, DownloadEntry, DownloadStatus, SegmentState};
pub use queue::DownloadQueue;
pub use segmented::plan_segments;
pub use speed_limiter::{SharedLimiter, SpeedLimiter};
pub use store::Store;
