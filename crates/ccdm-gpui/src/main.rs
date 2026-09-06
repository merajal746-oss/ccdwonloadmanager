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
    App, Application, Bounds, Context, Div, MouseButton, SharedString, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, rgb, size,
};

use ccdm_core::model::{resolve_dest, sanitize_file_name};
use ccdm_core::speed_limiter::now_ms;
use ccdm_core::{
    AppConfig, CancelFlag, Category, DownloadEntry, DownloadStatus, Schedule, SharedLimiter,
    SpeedLimiter, Store, http, i18n, media,
};

/// Which page the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Queue,
    Settings,
}

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
    fn label(self, lang: &str) -> String {
        i18n::t(
            lang,
            match self {
                Self::Queued => "st.queued",
                Self::Downloading => "st.downloading",
                Self::Paused => "st.paused",
                Self::Finished => "st.finished",
                Self::Failed => "st.failed",
            },
        )
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
    SetupDone { ytdlp_path: String, message: String },
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
fn speed_value(lang: &str, config: &AppConfig) -> String {
    if !config.enable_speed_limit || config.speed_limit_kbps == 0 {
        i18n::t(lang, "tb.unlimited")
    } else if config.speed_limit_kbps >= 1024 {
        format!("{:.1} MiB/s", config.speed_limit_kbps as f32 / 1024.0)
    } else {
        format!("{} KiB/s", config.speed_limit_kbps)
    }
}

fn speed_label(lang: &str, config: &AppConfig) -> String {
    let value = speed_value(lang, config);
    i18n::format(lang, "tb.speed", &[("v", &value)])
}

fn connections_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(
        lang,
        "tb.connections",
        &[("n", &config.max_connections.to_string())],
    )
}

fn on_off(lang: &str, on: bool) -> String {
    i18n::t(lang, if on { "cm.on" } else { "cm.off" })
}

fn organize_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(lang, "tb.organize", &[("v", &on_off(lang, config.organize_by_category))])
}

fn monitor_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(lang, "tb.monitor", &[("v", &on_off(lang, config.clipboard_monitor))])
}

fn sched_label(lang: &str, config: &AppConfig) -> String {
    match &config.schedule {
        Some(schedule) => i18n::format(lang, "tb.sched", &[("v", &schedule.describe())]),
        None => i18n::t(lang, "tb.sched_off"),
    }
}

fn shutdown_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(lang, "tb.shutdown", &[("v", &on_off(lang, config.shutdown_after_queue))])
}

fn quality_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(lang, "tb.quality", &[("v", &config.video_quality)])
}

fn quality_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(lang, "tb.quality", &[("v", &config.video_quality)])
}

fn theme_label(lang: &str, config: &AppConfig) -> String {
    i18n::format(
        lang,
        "tb.theme",
        &[("v", &i18n::t(lang, if config.dark_mode { "cm.dark" } else { "cm.light" }))],
    )
}

/// Full-window palette (cf. XDM skins). Toggle with the Theme button.
#[derive(Debug, Clone, Copy)]
struct Theme {
    bg: u32,
    card: u32,
    track: u32,
    accent: u32,
    text: u32,
    dim: u32,
    warn: u32,
    faint: u32,
    ok: u32,
    danger: u32,
    primary: u32,
    muted: u32,
    hover: u32,
}

const THEME_DARK: Theme = Theme {
    bg: 0x1e1e2e,
    card: 0x313244,
    track: 0x11111b,
    accent: 0x89b4fa,
    text: 0xffffff,
    dim: 0xa6adc8,
    warn: 0xf9e2af,
    faint: 0x6c7086,
    ok: 0x40a02b,
    danger: 0xf38ba8,
    primary: 0x89b4fa,
    muted: 0x585b70,
    hover: 0x45475a,
};

const THEME_LIGHT: Theme = Theme {
    bg: 0xeff1f5,
    card: 0xffffff,
    track: 0xccd0da,
    accent: 0x1e66f5,
    text: 0x4c4f69,
    dim: 0x6c6f85,
    warn: 0xdf8e1d,
    faint: 0x9ca0b0,
    ok: 0x40a02b,
    danger: 0xd20f39,
    primary: 0x1e66f5,
    muted: 0xacb0be,
    hover: 0xdce0e8,
};

/// Clickable label. `on_click` receives the mouse-down event like the
/// official `input.rs` example does.
fn button(
    theme: Theme,
    label: String,
    bg: u32,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .px_3()
        .py_1()
        .bg(rgb(bg))
        .rounded_lg()
        .hover(move |s| s.bg(rgb(theme.hover)))
        .text_sm()
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label)
}

/// Settings-screen button dispatching to a view method by fn pointer.
fn settings_button(
    cx: &mut Context<DownloadManager>,
    theme: Theme,
    label: String,
    bg: u32,
    action: fn(&mut DownloadManager, &mut Context<DownloadManager>),
) -> impl IntoElement {
    button(
        theme,
        label,
        bg,
        cx.listener(move |this, _event, _window, cx| {
            action(this, cx);
        }),
    )
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
    theme: Theme,
    config: AppConfig,
    client: reqwest::Client,
    limiter: Option<SharedLimiter>,
    tx: Sender<UiEvent>,
    rx: Receiver<UiEvent>,
    store: Store,
    saved: HashMap<String, String>,
    store_mtime: Option<std::time::SystemTime>,
    last_clipboard: String,
    screen: Screen,
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
        let theme = if config.dark_mode { THEME_DARK } else { THEME_LIGHT };
        if let Some(repo) = config.update_repo.clone() {
            let update_tx = tx.clone();
            let update_lang = config.language.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(runtime) = runtime {
                    if let Ok(client) = ccdm_core::http::build_client() {
                        if let Ok(info) =
                            runtime.block_on(ccdm_core::update::latest_release(&repo, &client))
                        {
                            if ccdm_core::update::is_newer(env!("CARGO_PKG_VERSION"), &info.tag) {
                                let _ = update_tx.send(UiEvent::Notice(i18n::format(
                                    &update_lang,
                                    "n.update",
                                    &[("tag", &info.tag), ("url", &info.url)],
                                )));
                            }
                        }
                    }
                }
            });
        }
        Self {
            title: i18n::t(&config.language, "app.title").into(),
            workers,
            rows: Vec::new(),
            notice: i18n::t(&config.language, "app.clip_hint"),
            categories: Category::default_categories(),
            config_summary: format!(
                "dir: {} | {} conn | {cap}",
                config.download_dir.display(),
                config.max_connections,
            ),
            theme,
            config,
            client,
            limiter,
            tx,
            rx,
            store,
            saved,
            store_mtime: None,
            last_clipboard: String::new(),
            screen: Screen::Queue,
        }
    }

    fn find(&self, id: &str) -> Option<&Worker> {
        self.workers.iter().find(|w| w.id == id)
    }

    fn start_row(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(schedule) = &self.config.schedule {
            if !schedule.allows_now() {
                self.notice = i18n::format(
                    &self.config.language,
                    "n.outside",
                    &[("w", &schedule.describe())],
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
        self.refresh_config_summary();
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
        self.refresh_config_summary();
        self.save_config();
        cx.notify();
    }

    fn save_config(&mut self) {
        match AppConfig::config_path() {
            Some(path) => {
                if let Err(e) = self.config.save(&path) {
                    self.notice =
                        i18n::format(&self.config.language, "n.save_c", &[("e", &e.to_string())]);
                }
            }
            None => self.notice = i18n::t(&self.config.language, "n.no_cfgdir"),
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

    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.config.dark_mode = !self.config.dark_mode;
        self.theme = if self.config.dark_mode {
            THEME_DARK
        } else {
            THEME_LIGHT
        };
        self.save_config();
        cx.notify();
    }

    fn toggle_screen(&mut self, cx: &mut Context<Self>) {
        self.screen = match self.screen {
            Screen::Queue => Screen::Settings,
            Screen::Settings => Screen::Queue,
        };
        cx.notify();
    }

    fn cycle_lang(&mut self, cx: &mut Context<Self>) {
        let langs = ccdm_core::i18n::available_langs();
        let pos = langs
            .iter()
            .position(|l| *l == self.config.language)
            .unwrap_or(0);
        self.config.language = langs[(pos + 1) % langs.len()].clone();
        self.save_config();
        cx.notify();
    }

    /// Pick the download folder with a native dialog.
    fn browse_download_dir(&mut self, cx: &mut Context<Self>) {
        let start = self.config.download_dir.clone();
        if let Some(dir) = rfd::FileDialog::new().set_directory(start).pick_folder() {
            self.config.download_dir = dir;
            self.refresh_config_summary();
            self.save_config();
        }
        cx.notify();
    }

    /// Reveal the download folder in the file manager.
    fn open_download_dir(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = open::that(&self.config.download_dir) {
            self.notice =
                i18n::format(&self.config.language, "n.reveal", &[("e", &e.to_string())]);
        }
        cx.notify();
    }

    /// Open config.json in the default editor (creates it first if needed).
    fn open_config_file(&mut self, cx: &mut Context<Self>) {
        let lang = self.config.language.clone();
        match AppConfig::config_path() {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if !path.exists() {
                    let _ = self.config.save(&path);
                }
                if let Err(e) = open::that(&path) {
                    self.notice = i18n::format(&lang, "n.reveal", &[("e", &e.to_string())]);
                }
            }
            None => self.notice = i18n::t(&lang, "n.no_cfgdir"),
        }
        cx.notify();
    }

    /// Download yt-dlp (+ffmpeg on Windows) in a worker thread.
    fn setup_video_tools(&mut self, cx: &mut Context<Self>) {
        let tx = self.tx.clone();
        let lang = self.config.language.clone();
        self.notice = i18n::t(&lang, "n.setup");
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let result = match runtime {
                Err(e) => Err(format!("runtime error: {e}")),
                Ok(runtime) => runtime.block_on(async {
                    let client = ccdm_core::http::build_client()
                        .map_err(|e| format!("http engine failed: {e}"))?;
                    let path = ccdm_core::video::setup_ytdlp(&client, |_, _| {})
                        .await
                        .map_err(|e| e.to_string())?;
                    if let Err(e) =
                        ccdm_core::video::setup_ffmpeg(&client, |_, _| {}).await
                    {
                        eprintln!("ffmpeg setup skipped: {e}");
                    }
                    Ok::<_, String>(path.display().to_string())
                }),
            };
            match result {
                Ok(path) => {
                    let _ = tx.send(UiEvent::SetupDone {
                        ytdlp_path: path.clone(),
                        message: i18n::format(&lang, "n.setup_ok", &[("v", &path)]),
                    });
                }
                Err(e) => {
                    let _ = tx.send(UiEvent::Notice(i18n::format(
                        &lang,
                        "n.setup_fail",
                        &[("e", &e)],
                    )));
                }
            }
        });
        cx.notify();
    }

    fn refresh_config_summary(&mut self) {
        let cap = if self.config.enable_speed_limit && self.config.speed_limit_kbps > 0 {
            format!("cap {} KiB/s", self.config.speed_limit_kbps)
        } else {
            "no speed cap".to_string()
        };
        self.config_summary = format!(
            "dir: {} | {} conn | {cap}",
            self.config.download_dir.display(),
            self.config.max_connections,
        );
    }

    fn cycle_quality(&mut self, cx: &mut Context<Self>) {
        let pos = ccdm_core::video::QUALITIES
            .iter()
            .position(|&q| q == self.config.video_quality)
            .unwrap_or(0);
        self.config.video_quality =
            ccdm_core::video::QUALITIES[(pos + 1) % ccdm_core::video::QUALITIES.len()].to_string();
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
            self.notice = i18n::t(&self.config.language, "n.shutting");
            if let Err(e) = ccdm_core::power::shutdown_host(60) {
                self.notice =
                    i18n::format(&self.config.language, "n.shut_fail", &[("e", &e.to_string())]);
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
                    self.notice =
                        i18n::format(&self.config.language, "n.reveal", &[("e", &e.to_string())]);
                }
            }
            None => self.notice = i18n::t(&self.config.language, "n.notfound"),
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

    /// Convert a finished row's file to MP3 in a worker thread.
    fn convert_row(&mut self, id: String, cx: &mut Context<Self>) {
        let lang = self.config.language.clone();
        let input = self.find(&id).map(|worker| {
            resolve_dest(
                &self.config.download_dir,
                &worker.file_name,
                &self.categories,
                self.config.organize_by_category,
            )
        });
        let Some(input) = input else {
            self.notice = i18n::t(&lang, "n.notfound");
            cx.notify();
            return;
        };
        if !ccdm_core::convert::ffmpeg_available() {
            self.notice = i18n::t(&lang, "n.no_ffmpeg");
            cx.notify();
            return;
        }
        let update_tx = self.tx.clone();
        std::thread::spawn(move || {
            let result =
                ccdm_core::convert::convert(&input, ccdm_core::convert::ConvertTarget::Mp3);
            let message = match result {
                Ok(path) => {
                    i18n::format(&lang, "n.converted", &[("out", &path.display().to_string())])
                }
                Err(e) => i18n::format(&lang, "n.convert_fail", &[("e", &e.to_string())]),
            };
            let _ = update_tx.send(UiEvent::Notice(message));
        });
        self.notice = i18n::format(
            &lang,
            "n.converting",
            &[("file", &input.display().to_string())],
        );
        cx.notify();
    }

    fn pause_row(&mut self, id: String, cx: &mut Context<Self>) {        if let Some(name) = self.find(&id).map(|worker| {
            worker.cancel.cancel();
            worker.file_name.clone()
        }) {
            self.notice = i18n::format(&self.config.language, "n.pausing", &[("name", &name)]);
        }
        cx.notify();
    }

    fn remove_row(&mut self, id: String, cx: &mut Context<Self>) {
        let busy = self
            .find(&id)
            .map(|w| w.alive.load(Ordering::SeqCst))
            .unwrap_or(false);
        if busy {
            self.notice = i18n::t(&self.config.language, "n.busy");
        } else {
            self.workers.retain(|w| w.id != id);
            self.store.queue_mut().remove(&id);
            self.saved.remove(&id);
            if let Err(e) = self.store.save() {
                self.notice =
                    i18n::format(&self.config.language, "n.save_q", &[("e", &e.to_string())]);
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
            self.notice = i18n::t(&self.config.language, "n.no_text");
            cx.notify();
            return;
        };
        self.add_url(url, cx);
    }

    /// Validate + probe `url` in a worker thread (shared by the clipboard
    /// button and the clipboard monitor).
    fn add_url(&mut self, url: String, cx: &mut Context<Self>) {
        if self.workers.iter().any(|w| w.url == url) {
            self.notice = i18n::t(&self.config.language, "n.dup");
            cx.notify();
            return;
        }
        if ccdm_core::video::is_video_page(&url) {
            self.add_video_page(url, cx);
            return;
        }
        // Probing needs async IO: worker thread, result back via channel.
        let tx = self.tx.clone();
        let client = self.client.clone();
        let count = self.workers.len();
        let lang = self.config.language.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match runtime {
                Err(e) => {
                    let _ = tx.send(UiEvent::Notice(i18n::format(
                        &lang,
                        "n.rt_err",
                        &[("e", &e.to_string())],
                    )));
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
                        let _ = tx.send(UiEvent::Notice(i18n::format(
                            &lang,
                            "n.probe_fail",
                            &[("e", &e.to_string())],
                        )));
                    }
                },
            }
        });
        self.notice = i18n::format(&self.config.language, "n.probing", &[("url", &url)]);
        cx.notify();
    }

    /// Resolve a watch page through yt-dlp in a worker thread, then queue
    /// the direct media URL like any other download.
    fn add_video_page(&mut self, url: String, cx: &mut Context<Self>) {
        let lang = self.config.language.clone();
        let tx = self.tx.clone();
        let count = self.workers.len();
        let ytdlp_path = self.config.ytdlp_path.clone();
        let quality = self.config.video_quality.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<(Worker, bool), ccdm_core::CcdmError> {
                let binary = ccdm_core::video::find_ytdlp(ytdlp_path.as_deref()).ok_or_else(|| {
                    ccdm_core::CcdmError::Other(
                        "yt-dlp not found — set ytdlp_path in config.json for video pages"
                            .to_string(),
                    )
                })?;
                let media = ccdm_core::video::resolve(
                    &binary,
                    &url,
                    ccdm_core::video::single_file_spec(&quality),
                )?;
                let name = format!(
                    "{}.{}",
                    sanitize_file_name(&media.title),
                    media.playback_ext()
                );
                let Some(play) = media.playback_url() else {
                    return Err(ccdm_core::CcdmError::Other(
                        "yt-dlp returned no downloadable streams".to_string(),
                    ));
                };
                Ok((
                    Worker::new(new_id(count), play.to_string(), name),
                    media.needs_mux(),
                ))
            })();
            match result {
                Ok((worker, video_only)) => {
                    let name = worker.file_name.clone();
                    let _ = tx.send(UiEvent::AddRow(worker));
                    if video_only {
                        let _ = tx.send(UiEvent::Notice(i18n::format(
                            &lang,
                            "n.video_only",
                            &[("file", &name)],
                        )));
                    }
                }
                Err(e) => {
                    let _ = tx.send(UiEvent::Notice(i18n::format(
                        &lang,
                        "n.probe_fail",
                        &[("e", &e.to_string())],
                    )));
                }
            }
        });
        self.notice = i18n::format(&self.config.language, "n.probing", &[("url", &url)]);
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
            self.notice = i18n::format(
                &self.config.language,
                "n.browser",
                &[("file", &worker.file_name)],
            );
            self.workers.push(worker);
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                UiEvent::AddRow(worker) => {
                    self.notice = i18n::format(
                        &self.config.language,
                        "n.added",
                        &[("file", &worker.file_name)],
                    );
                    self.workers.push(worker);
                }
                UiEvent::Notice(message) => {
                    self.notice = message;
                }
                UiEvent::SetupDone { ytdlp_path, message } => {
                    self.config.ytdlp_path = Some(ytdlp_path);
                    self.save_config();
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
            self.notice =
                i18n::format(&self.config.language, "n.save_q", &[("e", &e.to_string())]);
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
        match self.screen {
            Screen::Queue => self.render_queue(cx),
            Screen::Settings => self.render_settings(cx),
        }
    }
}

/// One settings row: translated name, current value, action buttons.
fn setting_row(
    theme: Theme,
    label: String,
    value: String,
    actions: Vec<impl IntoElement>,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .bg(rgb(theme.card))
        .rounded_lg()
        .child(div().text_xs().text_color(rgb(theme.dim)).child(label))
        .child(
            div()
                .flex()
                .gap_2()
                .child(div().flex_1().text_sm().truncate().child(value))
                .child(div().flex().gap_2().children(actions)),
        )
}

impl DownloadManager {
    fn render_queue(&mut self, cx: &mut Context<Self>) -> Div {
        let lang = self.config.language.clone();
        let total = self.rows.len();
        let active = self.rows.iter().filter(|row| row.running).count();
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(theme.bg))
            .size_full()
            .p_4()
            .text_color(rgb(theme.text))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xl()
                            .child(i18n::format(
                                &lang,
                                "app.live",
                                &[("t", &self.title.to_string())],
                            )),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(theme.dim))
                            .child(i18n::format(
                                &lang,
                                "app.stats",
                                &[
                                    ("total", &total.to_string()),
                                    ("active", &active.to_string()),
                                    ("cats", &self.categories.len().to_string()),
                                    ("cfg", &self.config_summary),
                                ],
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        theme,
                        i18n::t(&lang, "tb.add"),
                        theme.ok,
                        cx.listener(|this, _event, _window, cx| this.add_from_clipboard(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(rgb(theme.warn))
                            .child(self.notice.clone()),
                    )
                    .child(button(
                        theme,
                        i18n::t(&lang, "tb.settings"),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_screen(cx)),
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        theme,
                        speed_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.cycle_speed(cx)),
                    ))
                    .child(button(
                        theme,
                        connections_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.cycle_conns(cx)),
                    ))
                    .child(button(
                        theme,
                        organize_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_organize(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(theme.faint))
                            .child(i18n::t(&lang, "app.hint_settings")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(button(
                        theme,
                        monitor_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_monitor(cx)),
                    ))
                    .child(button(
                        theme,
                        sched_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_sched(cx)),
                    ))
                    .child(button(
                        theme,
                        shutdown_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_shutdown(cx)),
                    ))
                    .child(button(
                        theme,
                        theme_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.toggle_theme(cx)),
                    ))
                    .child(button(
                        theme,
                        quality_label(&lang, &self.config),
                        theme.muted,
                        cx.listener(|this, _event, _window, cx| this.cycle_quality(cx)),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(rgb(theme.faint))
                            .child(i18n::t(&lang, "app.hint_auto")),
                    ),
            )
            .child(if self.rows.is_empty() {
                div()
                    .text_sm()
                    .text_color(rgb(theme.faint))
                    .child(i18n::t(&lang, "app.empty"))
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
                            RowStatus::Failed => {
                                format!("{} — {}", row.status.label(&lang), row.detail)
                            }
                            status => status.label(&lang),
                        };
                        let mut actions = Vec::new();
                        match (row.status, row.running) {
                            (RowStatus::Downloading, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.pause"),
                                    theme.danger,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.pause_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            (RowStatus::Finished, _) => {
                                let folder_id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.folder"),
                                    theme.primary,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.reveal_row(folder_id.clone(), cx);
                                    }),
                                ));
                                let again_id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.again"),
                                    theme.muted,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.redownload_row(again_id.clone(), cx);
                                    }),
                                ));
                                if ccdm_core::convert::convertible_to_mp3(&row.file_name) {
                                    let mp3_id = row.id.clone();
                                    actions.push(button(
                                        theme,
                                        i18n::t(&lang, "row.mp3"),
                                        theme.ok,
                                        cx.listener(move |this, _event, _window, cx| {
                                            this.convert_row(mp3_id.clone(), cx);
                                        }),
                                    ));
                                }
                            }
                            (RowStatus::Failed, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.retry"),
                                    theme.primary,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            (RowStatus::Paused, _) => {
                                let id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.resume"),
                                    theme.primary,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                            _ => {
                                let id = row.id.clone();
                                actions.push(button(
                                    theme,
                                    i18n::t(&lang, "row.start"),
                                    theme.primary,
                                    cx.listener(move |this, _event, _window, cx| {
                                        this.start_row(id.clone(), cx);
                                    }),
                                ));
                            }
                        }
                        let remove_id = row.id.clone();
                        actions.push(button(
                            theme,
                            i18n::t(&lang, "row.remove"),
                            theme.muted,
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
                            .bg(rgb(theme.card))
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
                                            .text_color(rgb(theme.dim))
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
                                            .bg(rgb(theme.track))
                                            .rounded_lg()
                                            .child(
                                                div()
                                                    .w(px(280.0 * frac))
                                                    .h(px(10.0))
                                                    .bg(rgb(theme.accent))
                                                    .rounded_lg(),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(rgb(theme.dim))
                                            .child(progress_text),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme.faint))
                                    .truncate()
                                    .child(row.url.clone()),
                            )
                            .child(div().flex().gap_2().children(actions))
                    },
                ))
            })
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> Div {
        let lang = self.config.language.clone();
        let theme = self.theme;
        let tools_value = format!(
            "yt-dlp {} • ffmpeg {}",
            ccdm_core::video::find_ytdlp(self.config.ytdlp_path.as_deref())
                .unwrap_or_else(|| "—".to_string()),
            ccdm_core::convert::ffmpeg_binary().unwrap_or_else(|| "—".to_string())
        );
        let config_path = AppConfig::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".to_string());
        div()
            .flex()
            .flex_col()
            .gap_2()
            .bg(rgb(theme.bg))
            .size_full()
            .p_4()
            .text_color(rgb(theme.text))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(settings_button(
                        cx,
                        theme,
                        i18n::t(&lang, "tb.back"),
                        theme.muted,
                        Self::toggle_screen,
                    ))
                    .child(div().flex_1().text_xl().child(i18n::t(&lang, "set.title"))),
            )
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.folder"),
                self.config.download_dir.display().to_string(),
                vec![
                    settings_button(
                        cx,
                        theme,
                        i18n::t(&lang, "tb.browse"),
                        theme.primary,
                        Self::browse_download_dir,
                    ),
                    settings_button(
                        cx,
                        theme,
                        i18n::t(&lang, "tb.open"),
                        theme.muted,
                        Self::open_download_dir,
                    ),
                ],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.speed"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    speed_label(&lang, &self.config),
                    theme.muted,
                    Self::cycle_speed,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.connections"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    connections_label(&lang, &self.config),
                    theme.muted,
                    Self::cycle_conns,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.quality"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    quality_label(&lang, &self.config),
                    theme.muted,
                    Self::cycle_quality,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.language"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    self.config.language.clone(),
                    theme.muted,
                    Self::cycle_lang,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.organize"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    on_off(&lang, self.config.organize_by_category),
                    theme.muted,
                    Self::toggle_organize,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.monitor"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    on_off(&lang, self.config.clipboard_monitor),
                    theme.muted,
                    Self::toggle_monitor,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.shutdown"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    on_off(&lang, self.config.shutdown_after_queue),
                    theme.muted,
                    Self::toggle_shutdown,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.theme"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    on_off(&lang, self.config.dark_mode),
                    theme.muted,
                    Self::toggle_theme,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.sched"),
                String::new(),
                vec![settings_button(
                    cx,
                    theme,
                    sched_label(&lang, &self.config),
                    theme.muted,
                    Self::toggle_sched,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.video"),
                tools_value,
                vec![settings_button(
                    cx,
                    theme,
                    i18n::t(&lang, "tb.setup_video"),
                    theme.primary,
                    Self::setup_video_tools,
                )],
            ))
            .child(setting_row(
                theme,
                i18n::t(&lang, "set.advanced"),
                config_path,
                vec![settings_button(
                    cx,
                    theme,
                    i18n::t(&lang, "tb.edit_file"),
                    theme.muted,
                    Self::open_config_file,
                )],
            ))
    }
}

fn main() {
    // Same source as the CLI so both tools share settings.
    let config = match AppConfig::config_path() {
        Some(path) => AppConfig::load(&path).unwrap_or_default(),
        None => AppConfig::default(),
    };
    ccdm_core::i18n::load_available();
    let client = http::build_client_with(&config).unwrap_or_else(|e| {
        eprintln!("proxy config invalid ({e}); continuing without proxy");
        http::build_client().expect("default http client")
    });
    let limiter = SpeedLimiter::shared(config.speed_limit_kbps, config.enable_speed_limit);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(780.), px(640.)), cx);
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
