//! In-memory download queue (cf. XDM `DownloadQueue`).
//!
//! Owns every [`DownloadEntry`](crate::model::DownloadEntry), keeps
//! insertion order, and answers "what should run next?".

use std::collections::{HashMap, VecDeque};

use crate::model::{DownloadEntry, DownloadStatus};

/// Ordered set of downloads with O(1) lookup by id.
#[derive(Debug, Default)]
pub struct DownloadQueue {
    order: VecDeque<String>,
    entries: HashMap<String, DownloadEntry>,
}

impl DownloadQueue {
    /// Empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert (or replace) an entry, preserving first-insertion order.
    pub fn add(&mut self, entry: DownloadEntry) {
        if !self.entries.contains_key(&entry.id) {
            self.order.push_back(entry.id.clone());
        }
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Look up an entry.
    pub fn get(&self, id: &str) -> Option<&DownloadEntry> {
        self.entries.get(id)
    }

    /// Look up an entry mutably.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut DownloadEntry> {
        self.entries.get_mut(id)
    }

    /// Remove an entry; returns true when something was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        if self.entries.remove(id).is_some() {
            self.order.retain(|x| x != id);
            true
        } else {
            false
        }
    }

    /// Mark a download paused (only from Queued/Downloading/Failed).
    pub fn pause(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e)
                if matches!(
                    e.status,
                    DownloadStatus::Queued | DownloadStatus::Downloading | DownloadStatus::Failed
                ) =>
            {
                e.status = DownloadStatus::Paused;
                true
            }
            _ => false,
        }
    }

    /// Move a paused/failed download back to Queued.
    pub fn resume(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e)
                if matches!(e.status, DownloadStatus::Paused | DownloadStatus::Failed) =>
            {
                e.status = DownloadStatus::Queued;
                true
            }
            _ => false,
        }
    }

    /// Id of the oldest still-Queued download, if any.
    pub fn next_queued(&self) -> Option<&str> {
        self.order.iter().find_map(|id| {
            self.entries
                .get(id)
                .filter(|e| e.status == DownloadStatus::Queued)
                .map(|_| id.as_str())
        })
    }

    /// All entries in insertion order.
    pub fn iter_ordered(&self) -> impl Iterator<Item = &DownloadEntry> {
        self.order.iter().filter_map(|id| self.entries.get(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> DownloadEntry {
        DownloadEntry::new(id.into(), format!("https://x/{id}"), format!("{id}.bin"))
    }

    #[test]
    fn add_get_remove() {
        let mut q = DownloadQueue::new();
        q.add(entry("a"));
        assert_eq!(q.len(), 1);
        assert!(q.get("a").is_some());
        assert!(q.remove("a"));
        assert!(q.is_empty());
    }

    #[test]
    fn pause_resume_cycle() {
        let mut q = DownloadQueue::new();
        q.add(entry("a"));
        assert!(q.pause("a"));
        assert_eq!(q.get("a").unwrap().status, DownloadStatus::Paused);
        assert!(q.resume("a"));
        assert_eq!(q.get("a").unwrap().status, DownloadStatus::Queued);
    }

    #[test]
    fn next_queued_skips_paused() {
        let mut q = DownloadQueue::new();
        q.add(entry("a"));
        q.add(entry("b"));
        q.pause("a");
        assert_eq!(q.next_queued(), Some("b"));
    }
}
