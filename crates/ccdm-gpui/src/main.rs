//! `ccdm-gpui`: GPUI desktop shell (v0.1).
//!
//! Follows the official gpui.rs "Hello, World" structure (`Application`,
//! root view implementing `Render`). v0.1 shows the app title, engine
//! stats from `ccdm-core` (categories, queued sample entries) and the
//! roadmap. Wiring real engine events into views is the next milestone.

use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

use ccdm_core::{Category, DownloadEntry, DownloadStatus};

/// Root view: title + engine snapshot + roadmap.
struct DownloadManager {
    title: SharedString,
    entries: Vec<DownloadEntry>,
    categories: Vec<Category>,
}

impl Render for DownloadManager {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let finished = self
            .entries
            .iter()
            .filter(|e| e.status == DownloadStatus::Finished)
            .count();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(0x1e1e2e))
            .size_full()
            .p_6()
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .text_xl()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(format!("{} — v0.1 (GPUI)", self.title)),
            )
            .child(div().text_sm().child(format!(
                "{} download(s), {} finished, {} categories (engine: ccdm-core)",
                self.entries.len(),
                finished,
                self.categories.len()
            )))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(self.entries.iter().map(|e| {
                        let label = match e.progress() {
                            Some(p) => format!("{} — {:.0}%", e.file_name, p * 100.0),
                            None => format!("{} — {:?}", e.file_name, e.status),
                        };
                        div()
                            .px_3()
                            .py_2()
                            .bg(rgb(0x313244))
                            .text_sm()
                            .child(label)
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xa6adc8))
                    .child("Next: wire live engine events, add-URL dialog, pause/resume, speed limit."),
            )
    }
}

fn sample_entries() -> Vec<DownloadEntry> {
    let mut a = DownloadEntry::new(
        "sample-1".into(),
        "https://example.com/ubuntu.iso".into(),
        "ubuntu.iso".into(),
    );
    a.total_bytes = Some(4_000_000_000);
    a.downloaded_bytes = 1_000_000_000;
    let b = DownloadEntry::new(
        "sample-2".into(),
        "https://example.com/paper.pdf".into(),
        "paper.pdf".into(),
    );
    vec![a, b]
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(640.), px(480.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| DownloadManager {
                    title: "ccdwonloadmanager".into(),
                    entries: sample_entries(),
                    categories: Category::default_categories(),
                })
            },
        )
        .unwrap();
    });
}
