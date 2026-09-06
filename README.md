# ccdwonloadmanager (ccdM)

Rust download manager in the spirit of **Xtreme Download Manager (XDM)**,
**Neat Download Manager** and **AB Download Manager** — written in Rust,
built entirely by **GitHub Actions** so you need **no Rust toolchain locally**
(only `git`).

> v0.2 = persisted queue + `add`/`list`/`start` CLI, global speed cap wired
> into downloads, transient-error retries, filename sanitizing, proxy support.
> The full XDM feature set (HLS/DASH, browser integration, video converter,
> scheduler, …) is a roadmap, ported module by module (see below).

## Layout

```text
crates/ccdm-core   engine: model, config, queue, store, segments, speed limit, HTTP
crates/ccdm-cli    headless manager: probe/download/add/list/start
crates/ccdm-gpui   desktop GUI shell (GPUI 0.2.2)
.github/workflows  CI: fmt + clippy + test, then release builds per OS
```

## XDM → Rust port map

XDM upstream (`https://github.com/subhra74/xdm`, GPL-2.0, C# .NET as of
2025: `XDM.Core`, `XDM.Wpf.UI`, `XDM.Gtk.UI`, 346 `.cs` files) is big, so we
port its *concepts* clean-room (no copied code):

| XDM.Core concept | This repo |
|---|---|
| `Downloader/Chunk`, `ChunkState` | `ccdm-core::model::{Chunk, ChunkState}` |
| `Downloader/Progressive/SegmentState`, `Piece` | `model::SegmentState`, `segmented::plan_segments` |
| `Downloader/SpeedLimiter` | `speed_limiter::{SpeedLimiter, SharedLimiter}` — global cap enforced across all segments |
| `DownloadEntries`, `DownloadQueue`, `Category` | `model::{DownloadEntry, Category}`, `queue::DownloadQueue` |
| `Config`, `ProxyInfo` | `config::AppConfig` (JSON; incl. `proxy_url`) |
| `DataAccess`/`AppDB`, `QueueManager` | `store::Store` (versioned JSON queue; SQLite + named/scheduled queues later) |
| File-name helpers | `model::{guess_file_name, sanitize_file_name}` |
| Transient-vs-fatal failures | `CcdmError::is_transient` + 3-attempt backoff in CLI `start` |
| Progressive/adaptive HTTP downloaders | `http::{probe, download_with_resume, download_segmented}` |
| WPF/GTK UI | `ccdm-gpui` (GPUI shell; live wiring is next) |

Still to port: HLS/DASH parsers, `MediaParser`, FFmpeg wrapper, browser
native-messaging host + extensions, clipboard monitor, scheduler
(`DownloadSchedule`), updater, translations, themes.

License: **GPL-2.0-only** (compatible with XDM's GPL-2.0). See `LICENSE`.

## No-local-Rust workflow

1. Edit files here (or on github.com directly).
2. Commit + push with `git` (the only tool you installed).
3. GitHub Actions builds + tests on Windows / Linux / macOS.
4. Download the `.exe` from the run's **Artifacts** section — no `cargo`
   ever runs on your machine.

## First push (repo doesn't exist yet)

Create an **empty** repo on github.com (no README/license template, to avoid
conflicts), e.g. named `ccdwonloadmanager`, then (PowerShell, this folder):

```powershell
& "C:\Program Files\Git\bin\git.exe" init -b main
& "C:\Program Files\Git\bin\git.exe" add -A
& "C:\Program Files\Git\bin\git.exe" commit -m "v0.1: Rust engine + CLI + GPUI shell + CI"
& "C:\Program Files\Git\bin\git.exe" remote add origin https://github.com/<YOU>/ccdwonloadmanager.git
& "C:\Program Files\Git\bin\git.exe" push -u origin main
```

(Replace `<YOU>` with your GitHub username; the commands below do the first
three steps for you.)

Then open the repo → **Actions** tab → latest run → download
`ccdm-windows-x86_64`.

## CLI usage (after downloading the artifact)

```sh
ccdm-cli probe <URL>
ccdm-cli download <URL> [--output out.bin] [--connections 8]
ccdm-cli add <URL> [--name file.zip]
ccdm-cli list
ccdm-cli start [--id ID] [--connections 8]
```

Config lives in `<config-dir>/ccdm/config.json`, the queue in
`<config-dir>/ccdm/queue.json` (Windows: `%APPDATA%\ccdm\...`).

## Roadmap

- [ ] Live engine ↔ GPUI wiring (progress bars, pause/resume buttons)
- [ ] Add-URL dialog, categories folders, speed-limit control in GUI
- [ ] HLS (m3u8) + DASH (mpd) downloaders
- [ ] Browser integration (native-messaging host + extension)
- [ ] Clipboard monitor, queue scheduler, shutdown-on-finish
- [ ] Video probe/convert via system ffmpeg, updater, translations
