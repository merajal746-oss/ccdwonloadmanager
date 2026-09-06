//! Persisted download queue (cf. XDM `DataAccess`/SQLite `AppDB`).
//!
//! v0.2 stores the queue as versioned JSON
//! (`<config-dir>/ccdm/queue.json`); a SQLite backend with named,
//! scheduled queues (XDM `DownloadQueue` + `DownloadSchedule`) is roadmap.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CcdmError, DownloadEntry, DownloadQueue, Result};

/// On-disk shape; `version` lets us migrate later.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedQueue {
    version: u32,
    entries: Vec<DownloadEntry>,
}

/// A [`DownloadQueue`] bound to a file.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    queue: DownloadQueue,
}

impl Store {
    /// Empty in-memory queue bound to `path` (no IO performed).
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            queue: DownloadQueue::new(),
        }
    }

    /// OS-specific queue file path, if a config dir is known.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("ccdm").join("queue.json"))
    }

    /// Load from the default path (missing file = empty queue).
    pub fn load() -> Result<Self> {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self {
                path: PathBuf::from("queue.json"),
                queue: DownloadQueue::new(),
            }),
        }
    }

    /// Load from `path` (missing file = empty queue).
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                queue: DownloadQueue::new(),
            });
        }
        let text = std::fs::read_to_string(path).map_err(CcdmError::from)?;
        let persisted: PersistedQueue = serde_json::from_str(&text).map_err(CcdmError::from)?;
        let mut queue = DownloadQueue::new();
        for entry in persisted.entries {
            queue.add(entry);
        }
        Ok(Self {
            path: path.to_path_buf(),
            queue,
        })
    }

    /// Save the queue (insertion order preserved) to its file.
    pub fn save(&self) -> Result<()> {
        let entries: Vec<DownloadEntry> = self.queue.iter_ordered().cloned().collect();
        let text = serde_json::to_string_pretty(&PersistedQueue {
            version: 1,
            entries,
        })
        .map_err(CcdmError::from)?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(CcdmError::from)?;
            }
        }
        std::fs::write(&self.path, text).map_err(CcdmError::from)?;
        Ok(())
    }

    /// File backing this store.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read access to the queue.
    pub fn queue(&self) -> &DownloadQueue {
        &self.queue
    }

    /// Modify the queue (remember to [`save`](Self::save) afterwards).
    pub fn queue_mut(&mut self) -> &mut DownloadQueue {
        &mut self.queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DownloadStatus;

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let mut store = Store::load_from(&path).unwrap();
        assert!(store.queue().is_empty());

        let mut entry = DownloadEntry::with_plan(
            "a".into(),
            "https://x/a.bin".into(),
            "a.bin".into(),
            Some(10),
            2,
        );
        entry.status = DownloadStatus::Paused;
        store.queue_mut().add(entry);
        store.save().unwrap();

        let back = Store::load_from(&path).unwrap();
        let got = back.queue().get("a").unwrap();
        assert_eq!(got.status, DownloadStatus::Paused);
        assert_eq!(got.chunks.len(), 2);
        assert_eq!(back.queue().iter_ordered().count(), 1);
    }
}
