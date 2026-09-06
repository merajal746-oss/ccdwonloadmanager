//! `ccdm-gpui` v0.3: live download list wired to `ccdm-core`.
//!
//! * each row is driven by a worker thread (its own current-thread tokio
//!   runtime) that shares progress through atomics — the UI never blocks;
//! * a `cx.spawn` poll loop (~4 Hz) drains worker events, refreshes the
//!   rows and persists the queue, then `cx.notify()`s;
//! * buttons: add-from-clipboard, per-row start / pause / resume / retry /
//!   remove. Pause is cooperative via [`CancelFlag`](ccdm_core::CancelFlag),
//!   resume continues from the `.part` files on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::time::Duration;

use gpui::{
    App, Application, Bounds, Context, MouseButton, SharedString, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

use ccdm_core::model::{resolve_dest, sanitize_file_name};
use ccdm_core::speed_limiter::now_ms;
use ccdm_core::{
    AppConfig, CancelFlag, Category, DownloadEntry, DownloadStatus, Schedule, SharedLimiter,
    SpeedLimiter, Store, http, media,
};

/// Engine-side state of one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Queued,
    Downloading,
    Paused,
    Finished,
    Failed,
}

impl RowStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }
}

/// Handles shared between the UI thread and the row's worker thread.
#[derive(Debug, Clone)]
struct Worker {
    id: String,
    url: String,
    file_name: String,
    status: Arc<Mutex<RowStatus>>,
    detail: Arc<Mutex<String>>,
    downloaded: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    cancel: CancelFlag,
    alive: Arc<AtomicBool>,
}

impl Worker {
    fn new(id: String, url: String, file_name: String) -> Self {
        Self {
            id,
            url,
            file_name,
            status: Arc::new(Mutex::new(RowStatus::Queued)),
            detail: Arc::new(Mutex::new(String::new())),
            downloaded: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            cancel: CancelFlag::new(),
            alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Owned snapshot for rendering (never blocks the UI thread long:
    /// only brief mutex takes and atomic loads).
    fn snapshot(&self) -> RowView {
        RowView {
            id: self.id.clone(),
            url: self.url.clone(),
            file_name: self.file_name.clone(),
            status: *self.status.lock().unwrap(),
            detail: self.detail.lock().unwrap().clone(),
            downloaded: self.downloaded.load(Ordering::Relaxed),
            total: match self.total.load(Ordering::Relaxed) {
                0 => None,
                t => Some(t),
            },
            running: self.alive.load(Ordering::SeqCst),
        }
    }
}

/// What `render` sees.
struct RowView {
    id: String,
    url: String,
    file_name: String,
    status: RowStatus,
    detail: String,
    downloaded: u64,
    total: Option<u64>,
    running: bool,
}

/// Messages from worker threads to the UI thread.
enum UiEvent {
    AddRow(Worker),
    Notice(String),
}

/// Fingerprint used to persist only on real state changes.
fn worker_fingerprint(w: &Worker) -> String {
    let status = *w.status.lock().unwrap();
    match status {
        RowStatus::Downloading => format!("{status:?}"),
        _ => format!("{status:?}:{}", w.downloaded.load(Ordering::Relaxed)),
    }
}

fn new_id(n: usize) -> String {
    format!("gui-{}-{n}", now_ms())
}

/// Speed-cap steps cycled by the toolbar button (0 = unlimited).
const SPEED_STEPS: &[u32] = &[0, 256, 512, 1024, 2048, 5120, 10240];
/// Per-download connection steps cycled by the toolbar button.
const CONN_STEPS: &[usize] = &[1, 2, 4, 8, 16, 32];

fn speed_label(config: &AppConfig) -> String {    if !config.enable_speed_limit || config.speed_limit_kbps == 0 {
        "Speed: unlimited".to_string()
    } else if config.speed_limit_kbps >= 1024 {
        format!(
            "Speed: {:.1} MiB/s",
            config.speed_limit_kbps as f32 / 1024.0
        )
    } else {
        format!("Speed: {} KiB/s", config.speed_limit_kbps)
    }
}

fn organize_label(config: &AppConfig) -> String {
    format!("Organize: {}", if config.organize_by_category { "on" } else { "off" })
}

fn monitor_label(config: &AppConfig) -> String {
    format!("Monitor: {}", if config.clipboard_monitor { "on" } else { "off" })
}

fn sched_label(config: &AppConfig) -> String {
    match &config.schedule {
        Some(schedule) => format!("Sched: {}", schedule.describe()),
        None => "Sched: off".to_string(),
    }
}

fn shutdown_label(config: &AppConfig) -> String {
    format!("Shutdown: {}", if config.shutdown_after_queue { "on" } else { "off" })
}

/// Clickable label. `on_click` receives the mouse-down event like the
/// official `input.rs` example does.
fn button(
    label: String,
    bg: u32,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .bg(rgb(bg))
        .rounded_lg()
        .hover(|s| s.bg(rgb(0x45475a)))
        .text_sm()
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label)
}

/// Blocking worker: probe is already done, download to `dest` with resume.
fn run_download(
    worker: Worker,
    client: reqwest::Client,
    config: AppConfig,
    limiter: Option<SharedLimiter>,
    segments: usize,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            *worker.status.lock().unwrap() = RowStatus::Failed;
            *worker.detail.lock().unwrap() = format!("runtime error: {e}");
            worker.alive.store(false, Ordering::SeqCst);
            return;
        }
    };
    let dest = resolve_dest(
        &config.download_dir,
        &worker.file_name,
        &Category::default_categories(),
        config.organize_by_category,
    );
    let downloaded = worker.downloaded.clone();
    let total = worker.total.clone();
    let res = runtime.block_on(media::download_auto(
        &client,
        &worker.url,
        &dest,
        segments,
        limiter,
        Some(worker.cancel.clone()),
        move |d, t| {
            downloaded.store(d, Ordering::Relaxed);
            if let Some(x) = t {
                total.store(x, Ordering::Relaxed);
            }
        },
    ));
    match res {
        Ok(()) => {
            let t = worker.total.load(Ordering::Relaxed);
            if t > 0 {
                worker.downloaded.store(t, Ordering::Relaxed);
            }
            *worker.status.lock().unwrap() = RowStatus::Finished;
        }
        Err(e) => {
            if worker.cancel.is_cancelled() {
                *worker.status.lock().unwrap() = RowStatus::Paused;
            } else {
                *worker.status.lock().unwrap() = RowStatus::Failed;
                *worker.detail.lock().unwrap() = e.to_string();
            }
        }
    }
    worker.alive.store(false, Ordering::SeqCst);
}

/// Root view: owns the rows, the engine pieces and the queue store.
struct DownloadManager {
    title: SharedString,
    workers: Vec<Worker>,
    rows: Vec<RowView>,
    notice: String,
    categories: Vec<Category>,
    config_summary: String,
    config: AppConfig,
    client: reqwest::Client,
    limiter: Option<SharedLimiter>,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    store: Store,
    saved: HashMap<String, String>,
    store_mtime: Option<std::time::SystemTime>,
    last_clipboard: String,
}

impl DownloadManager {
    fn open(
        cx: &mut Context<Self>,
        config: AppConfig,
        client: reqwest::Client,
        limiter: Option<SharedLimiter>,
        tx: Sender<UiEvent>,
        rx: Receiver<UiEvent>,
    ) -> Self {
        // Refresh loop: pull worker state into the view ~4x/second.
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let alive = view.update(cx, |this, cx| {
                    this.drain_events();
                    this.sync_from_store();
                    this.poll_clipboard(cx);
                    this.refresh_rows();
                    this.persist_if_changed();
                    this.poll_shutdown();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let store = match Store::load() {
            Ok(store) => store,
            Err(e) => {
                eprintln!("queue load failed ({e}); starting fresh");
                Store::new(
                    Store::default_path().unwrap_or_else(|| PathBuf::from("queue.json")),
                )
            }
        };
        let mut workers = Vec::new();
        for entry in store.queue().iter_ordered() {
            let worker = Worker::new(
                entry.id.clone(),
                entry.url.clone(),
                entry.file_name.clone(),
            );
            *worker.status.lock().unwrap() = match entry.status {
                DownloadStatus::Finished => RowStatus::Finished,
                DownloadStatus::Paused => RowStatus::Paused,
                DownloadStatus::Failed => RowStatus::Failed,
                DownloadStatus::Queued | DownloadStatus::Downloading => RowStatus::Queued,
            };
            worker
                .downloaded
                .store(entry.downloaded_bytes, Ordering::Relaxed);
            if let Some(t) = entry.total_bytes {
                worker.total.store(t, Ordering::Relaxed);
            }
            workers.push(worker);
        }
        let saved = workers
            .iter()
            .map(|w| (w.id.clone(), worker_fingerprint(w)))
            .collect();
        let cap = if config.enable_speed_limit && config.speed_limit_kbps > 0 {
            format!("cap {} KiB/s", config.speed_limit_kbps)
        } else {
            "no speed cap".to_string()
        };
        Self {
            title: "ccdwonloadmanager".into(),
            workers,
            rows: Vec::new(),
            notice: "Copy a download link, then hit “Add from clipboard”.".to_string(),
            categories: Category::default_categories(),
            config_summary: format!(
                "dir: {} | {} conn | {cap}",
                config.download_dir.display(),
                config.max_connections,
            ),
            config,
            client,
            limiter,
            tx,
            rx,
            store,
            saved,
            store_mtime: None,
            last_clipboard: String::new(),
        }
    }

    fn find(&self, id: &str) -> Option<&Worker> {
        self.workers.iter().find(|w| w.id == id)
    }

    fn start_row(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(schedule) = &self.config.schedule {
            if !schedule.allows_now() {
                self.notice = format!(
                    "outside scheduled window ({}); toggle Sched to run now",
                    schedule.describe()
                );
                cx.notify();
                return;
            }
        }
        let Some(worker) = self.find(&id).cloned() else {
            return;
        };
        if worker.alive.load(Ordering::SeqCst) {
            return;
        }
        worker.cancel.clear();
        *worker.status.lock().unwrap() = RowStatus::Downloading;
        *worker.detail.lock().unwrap() = String::new();
        worker.alive.store(true, Ordering::SeqCst);
        let client = self.client.clone();
        let config = self.config.clone();
        let limiter = self.limiter.clone();
        let segments = self.config.max_connections.clamp(1, 32);
        std::thread::spawn(move || run_download(worker, client, config, limiter, segments));
        self.notice = String::new();
        self.persist();
        cx.notify();
    }

    /// Cycle the global speed cap; persists to `config.json` and rebuilds
    /// the limiter for subsequently started downloads.
    fn cycle_speed(&mut self, cx: &mut Context<Self>) {
        let current = if self.config.enable_speed_limit {
            self.config.speed_limit_kbps
        } else {
            0
        };
        let pos = SPEED_STEPS.iter().position(|&s| s == current).unwrap_or(0);
        let next = SPEED_STEPS[(pos + 1) % SPEED_STEPS.len()];
        self.config.speed_limit_kbps = next;
        self.config.enable_speed_limit = next > 0;
        self.limiter = SpeedLimiter::shared(
            self.config.speed_limit_kbps,
            self.config.enable_speed_limit,
        );
        self.save_config();
        cx.notify();
    }

    /// Cycle per-download connections; applies to newly started downloads.
    fn cycle_conns(&mut self, cx: &mut Context<Self>) {
        let pos = CONN_STEPS
            .iter()
            .position(|&c| c == self.config.max_connections)
            .unwrap_or(3);
        self.config.max_connections = CONN_STEPS[(pos + 1) % CONN_STEPS.len()];
        self.save_config();
        cx.notify();
    }

    fn save_config(&mut self) {
        match AppConfig::config_path() {
            Some(path) => {
                if let Err(e) = self.config.save(&path) {
                    self.notice = format!("config save failed: {e}");
                }
            }
            None => self.notice = "no config dir on this platform".to_string(),
        }
    }

    /// Flip organize-into-category-folders; persists immediately.
    fn toggle_organize(&mut self, cx: &mut Context<Self>) {
        self.config.organize_by_category = !self.config.organize_by_category;
        self.save_config();
        cx.notify();
    }

    fn toggle_monitor(&mut self, cx: &mut Context<Self>) {
        self.config.clipboard_monitor = !self.config.clipboard_monitor;
        self.save_config();
        cx.notify();
    }

    fn toggle_sched(&mut self, cx: &mut Context<Self>) {
        self.config.schedule = match self.config.schedule {
            Some(_) => None,
            None => Some(Schedule::nightly()),
        };
        self.save_config();
        cx.notify();
    }

    fn toggle_shutdown(&mut self, cx: &mut Context<Self>) {
        self.config.shutdown_after_queue = !self.config.shutdown_after_queue;
        self.save_config();
        cx.notify();
    }

    /// Clipboard monitor: auto-queue freshly copied links (cf. XDM).
    fn poll_clipboard(&mut self, cx: &mut Context<Self>) {
        if !self.config.clipboard_monitor {
            return;
        }
        let url = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(url) = url else { return };
        if url == self.last_clipboard {
            return;
        }
        self.last_clipboard = url.clone();
        if !url.to_lowercase().starts_with("http") {
            return;
        }
        self.add_url(url, cx);
    }

    /// Power off once a non-empty queue drains cleanly (cf. XDM).
    fn poll_shutdown(&mut self) {
        if !self.config.shutdown_after_queue || self.workers.is_empty() {
            return;
        }
        let all_done = self.workers.iter().all(|w| {
            matches!(
                *w.status.lock().unwrap(),
                RowStatus::Finished | RowStatus::Failed
            )
        });
        let any_finished = self
            .workers
            .iter()
            .any(|w| matches!(*w.status.lock().unwrap(), RowStatus::Finished));
        if all_done && any_finished {
            self.config.shutdown_after_queue = false;
            self.save_config();
            self.notice = "queue complete, shutting down in 60s…".to_string();
            if let Err(e) = ccdm_core::power::shutdown_host(60) {
                self.notice = format!("shutdown failed: {e}");
            }
        }
    }

    /// Open the containing folder of a row's file in the file manager.
    fn reveal_row(&mut self, id: String, cx: &mut Context<Self>) {
        let dest = self.find(&id).map(|worker| {
            resolve_dest(
                &self.config.download_dir,
                &worker.file_name,
                &self.categories,
                self.config.organize_by_category,
            )
        });
        match dest {
            Some(path) => {
                let target = path.parent().map(|parent| parent.to_path_buf()).unwrap_or(path);
                if let Err(e) = open::that(&target) {
                    self.notice = format!("cannot open folder: {e}");
                }
            }
            None => self.notice = "row not found".to_string(),
        }
        cx.notify();
    }

    /// Delete outputs and download again from scratch.
    fn redownload_row(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(dest) = self.find(&id).map(|worker| {
            resolve_dest(
                &self.config.download_dir,
                &worker.file_name,
                &self.categories,
                self.config.organize_by_category,
            )
        }) {
            let _ = std::fs::remove_file(&dest);
            for part in 0..32 {
                let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
                    "{}.part{part}",
                    dest.display()
                )));
            }
            if let Some(worker) = self.workers.iter().find(|w| w.id == id) {
                worker.downloaded.store(0, Ordering::Relaxed);
            }
        }
        self.start_row(id, cx);
    }

    fn pause_row(&mut self, id: String, cx: &mut Context<Self>) {        if let Some(name) = self.find(&id).map(|worker| {
            worker.cancel.cancel();
            worker.file_name.clone()
        }) {
            self.notice = format!("pausing {name} …");
        }
        cx.notify();
    }

    fn remove_row(&mut self, id: String, cx: &mut Context<Self>) {
        let busy = self
            .find(&id)
            .map(|w| w.alive.load(Ordering::SeqCst))
            .unwrap_or(false);
        if busy {
            self.notice = "pause it before removing".to_string();
        } else {
            self.workers.retain(|w| w.id != id);
            self.store.queue_mut().remove(&id);
            self.saved.remove(&id);
            if let Err(e) = self.store.save() {
                self.notice = format!("queue save failed: {e}");
            } else {
                self.notice = String::new();
            }
        }
        cx.notify();
    }

    fn add_from_clipboard(&mut self, cx: &mut Context<Self>) {
        let url = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(url) = url else {
            self.notice = "clipboard has no text — copy a link first".to_string();
            cx.notify();
            return;
        };
        self.add_url(url, cx);
    }

    /// Validate + probe `url` in a worker thread (shared by the clipboard
    /// button and the clipboard monitor).
    fn add_url(&mut self, url: String, cx: &mut Context<Self>) {
        if self.workers.iter().any(|w| w.url == url) {
            self.notice = "that URL is already in the list".to_string();
            cx.notify();
            return;
        }
        // Probing needs async IO: worker thread, result back via channel.
        let tx = self.tx.clone();
        let client = self.client.clone();
        let count = self.workers.len();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Err(e) => {
                    let _ = tx.send(UiEvent::Notice(format!("runtime error: {e}")));
                }
                Ok(runtime) => match runtime.block_on(http::probe(&client, &url)) {
                    Ok(info) => {
                        let worker = Worker::new(
                            new_id(count),
                            info.final_url,
                            sanitize_file_name(&info.file_name),
                        );
                        if let Some(t) = info.total_bytes {
                            worker.total.store(t, Ordering::Relaxed);
                        }
                        let _ = tx.send(UiEvent::AddRow(worker));
                    }
                    Err(e) => {
                        let _ = tx.send(UiEvent::Notice(format!("probe failed: {e}")));
                    }
                },
            }
        });
        self.notice = format!("probing {url} …");
        cx.notify();
    }

    /// Import queue entries added externally (browser host, CLI) since the
    /// last poll, detected via the store file's mtime.
    fn sync_from_store(&mut self) {
        let mtime = std::fs::metadata(self.store.path())
            .and_then(|meta| meta.modified())
            .ok();
        if mtime == self.store_mtime {
            return;
        }
        self.store_mtime = mtime;
        let fresh = match Store::load_from(self.store.path()) {
            Ok(store) => store,
            Err(_) => return,
        };
        for entry in fresh.queue().iter_ordered() {
            if self.workers.iter().any(|w| w.id == entry.id) {
                continue;
            }
            let worker = Worker::new(
                entry.id.clone(),
                entry.url.clone(),
                entry.file_name.clone(),
            );
            *worker.status.lock().unwrap() = match entry.status {
                DownloadStatus::Finished => RowStatus::Finished,
                DownloadStatus::Paused => RowStatus::Paused,
                DownloadStatus::Failed => RowStatus::Failed,
                DownloadStatus::Queued | DownloadStatus::Downloading => RowStatus::Queued,
            };
            worker
                .downloaded
                .store(entry.downloaded_bytes, Ordering::Relaxed);
            if let Some(total) = entry.total_bytes {
                worker.total.store(total, Ordering::Relaxed);
            }
            self.notice = format!("browser added {}", worker.file_name);
            self.workers.push(worker);
        }
    }

    fn drain_events(&mut self) {        while let Ok(event) = self.rx.try_recv() {
            match event {
                UiEvent::AddRow(worker) => {
                    self.notice = format!("added {} — hit Start", worker.file_name);
                    self.workers.push(worker);
                }
                UiEvent::Notice(message) => {
                    self.notice = message;
                }
            }
        }
    }

    fn refresh_rows(&mut self) {
        self.rows = self.workers.iter().map(Worker::snapshot).collect();
    }

    fn persist(&mut self) {
        let entries: Vec<DownloadEntry> = self
            .workers
            .iter()
            .map(|worker| {
                let mut entry = DownloadEntry::new(
                    worker.id.clone(),
                    worker.url.clone(),
                    worker.file_name.clone(),
                );
                entry.status = match *worker.status.lock().unwrap() {
                    RowStatus::Queued => DownloadStatus::Queued,
                    RowStatus::Downloading => DownloadStatus::Downloading,
                    RowStatus::Paused => DownloadStatus::Paused,
                    RowStatus::Finished => DownloadStatus::Finished,
                    RowStatus::Failed => DownloadStatus::Failed,
                };
                entry.downloaded_bytes = worker.downloaded.load(Ordering::Relaxed);
                entry.total_bytes = match worker.total.load(Ordering::Relaxed) {
                    0 => None,
                    t => Some(t),
                };
                entry
            })
            .collect();
        for entry in entries {
            self.store.queue_mut().add(entry);
        }
        if let Err(e) = self.store.save() {
            self.notice = format!("queue save failed: {e}");
        }
    }

    fn persist_if_changed(&mut self) {
        let updates: Vec<(String, String)> = self
            .workers
            .iter()
            .map(|worker| (worker.id.clone(), worker_fingerprint(worker)))
            .collect();
        let mut changed = false;
        for (id, fingerprint) in updates {
            if self.saved.get(&id) != Some(&fingerprint) {
                self.saved.insert(id, fingerprint);
                changed = true;
            }
        }
        if changed {
            self.persist();
        }
    }
}

impl Render for DownloadManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.rows.len();
        let active = self.rows.iter().filter(|row| row.running).count();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(0x1e1e2e))
            .size_full()
            .p_4()
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_xl().child(format!("{} — live queue", self.title)))
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0xa6adc8))
                            .child(format!(
                                "{total} row(s), {active} active, {} categories • {}",
                                self.categories.len(),
                                self.config_summary,
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        "Add from clipboard".to_string(),
                        0x40a02b,
                        cx.listener(|this, _event, _window, cx| this.add_from_clipboard(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(0xf9e2af))
                            .child(self.notice.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        speed_label(&self.config),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.cycle_speed(cx)),
                    ))
                    .child(button(
                        format!("Connections: {}", self.config.max_connections),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.cycle_conns(cx)),
                    ))
                    .child(button(
                        organize_label(&self.config),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.toggle_organize(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(0x6c7086))
                            .child("apply to newly started downloads"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        monitor_label(&self.config),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.toggle_monitor(cx)),
                    ))
                    .child(button(
                        sched_label(&self.config),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.toggle_sched(cx)),
                    ))
                    .child(button(
                        shutdown_label(&self.config),
                        0x585b70,
                        cx.listener(|this, _event, _window, cx| this.toggle_shutdown(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(0x6c7086))
                            .child("monitor adds copied links • sched gates starts"),
                    ),
            )
            .child(if self.rows.is_empty() {
                div()
                    .text_sm()
                    .text_color(rgb(0x6c7086))
                    .child("Queue is empty — copy a download link, then hit “Add from clipboard”.")
            } else {
                div().flex().flex_col().gap_2().children(self.rows.iter().map(
                    |row| {
                        let frac = match row.total {
                            Some(t) if t > 0 => (row.downloaded.min(t) as f32) / (t as f32),
                            _ => 0.0,
                        }
                        .clamp(0.0, 1.0);
                        let progress_text = match row.total {
                            Some(t) if t > 0 => format!("{:.0}% of {t} bytes", frac * 100.0),
                            _ => format!("{} bytes", row.downloaded),
                        };
                        let status_text = match row.status {
                            RowStatus::Failed => format!("failed — {}", row.detail),
                            status => status.label().to_string(),
                        };
                        let mut actions = Vec::new();
                        match (row.status, row.running) {
                            (RowStatus::Downloading, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    "Pause".to_string(),
                                    0xf38ba8,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.pause_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            (RowStatus::Finished, _) => {
                                let folder_id = row.id.clone();
                                actions.push(button(
                                    "Folder".to_string(),
                                    0x89b4fa,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.reveal_row(folder_id.clone(), cx);
                                    }),
                                ));
                                let again_id = row.id.clone();
                                actions.push(button(
                                    "Again".to_string(),
                                    0x585b70,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.redownload_row(again_id.clone(), cx);
                                    }),
                                ));
                            }
                            (RowStatus::Failed, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    "Retry".to_string(),
                                    0x89b4fa,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            (RowStatus::Paused, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    "Resume".to_string(),
                                    0x89b4fa,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            _ => {
                                let id = row.id.clone();
                                actions.push(button(
                                    "Start".to_string(),
                                    0x89b4fa,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                        }
                        let remove_id = row.id.clone();
                        actions.push(button(
                            "Remove".to_string(),
                            0x585b70,
                            cx.listener(move |this, _event, _window, cx| {
                                this.remove_row(remove_id.clone(), cx);
                            }),
                        ));
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_2()
                            .bg(rgb(0x313244))
                            .rounded_lg()
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_sm()
                                            .truncate()
                                            .child(row.file_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(0xa6adc8))
                                            .child(status_text),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .w(px(280.0))
                                            .h(px(10.0))
                                            .bg(rgb(0x11111b))
                                            .rounded_lg()
                                            .child(
                                                div()
                                                    .w(px(280.0 * frac))
                                                    .h(px(10.0))
                                                    .bg(rgb(0x89b4fa))
                                                    .rounded_lg(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(rgb(0xa6adc8))
                                            .child(progress_text),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x6c7086))
                                    .truncate()
                                    .child(row.url.clone()),
                            )
                            .child(div().flex().gap_2().children(actions))
                    },
                ))
            })
    }
}

fn main() {
    // Same source as the CLI so both tools share settings.
    let config = match AppConfig::config_path() {
        Some(path) => AppConfig::load(&path).unwrap_or_default(),
        None => AppConfig::default(),
    };
    let client = http::build_client_with(&config).unwrap_or_else(|e| {
        eprintln!("proxy config invalid ({e}); continuing without proxy");
        http::build_client().expect("default http client")
    });
    let limiter = SpeedLimiter::shared(config.speed_limit_kbps, config.enable_speed_limit);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(720.), px(520.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                let (tx, rx) = mpsc::channel();
                cx.new(|cx| {
                    DownloadManager::open(
                        cx,
                        config.clone(),
                        client.clone(),
                        limiter.clone(),
                        tx,
                        rx,
                    )
                })
            },
        )
        .unwrap();
    });
}
