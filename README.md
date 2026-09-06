# ccdwonloadmanager (ccdM)

Rust download manager in the spirit of **Xtreme Download Manager (XDM)**,
**Neat Download Manager** and **AB Download Manager** — written in Rust,
built entirely by **GitHub Actions** so you need **no Rust toolchain locally**
(only `git`).

> v0.6 = HLS (m3u8) + DASH (mpd) downloads: dependency-free playlist
> parsers, best-rendition picking, `.ts`/`.mp4` output, auto-dispatch for
> playlist URLs in CLI and GUI, media hint in `probe`.

## What works today

**GUI (`ccdm-gpui`)**
- Live queue: per-row progress bars, %/bytes, status, URL
- Row buttons: Start, Pause (cooperative cancel), Resume, Retry, Remove
- **Add from clipboard**: probes the URL in the background, dedupes
- Toolbar: speed-cap cycle (unlimited → 10 MiB/s), connections cycle (1→32),
  organize toggle — all persisted to `config.json`
- Finished rows: **Folder** (reveals file location), **Again** (re-downloads)
- Queue auto-persists on every state change; reloads on startup

**CLI (`ccdm-cli`)**
- `probe` (size, name, range support, mime), one-shot `download`
- `add` / `list` / `start [--id]` over the same persisted queue as the GUI
- Resumable segmented downloads, transient-error retries with backoff

**Engine (`ccdm-core`)**
- Segmented multi-connection HTTP with per-part resume + stale-part repair
- HLS + DASH: playlist parsing, best rendition, `.ts`/`.mp4` assembly
- Global speed cap enforced across all segments, proxy support
- Filename guessing + sanitizing, categories + `resolve_dest()` folders
- JSON config + versioned JSON queue store

## Layout

```text
crates/ccdm-core   engine: model, config, queue, store, cancel, segments, speed limit, HTTP, media (HLS/DASH)
crates/ccdm-cli    headless manager: probe/download/add/list/start
crates/ccdm-gpui   live GUI (GPUI 0.2.2): rows, progress bars, buttons, settings
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
| `CancelFlag` / pause | `cancel::CancelFlag`, checked per chunk, never auto-retried |
| `DownloadEntries`, `DownloadQueue`, `Category` | `model::{DownloadEntry, Category}`, `queue::DownloadQueue` |
| `Config`, `ProxyInfo` | `config::AppConfig` (JSON; incl. `proxy_url`, `organize_by_category`) |
| `DataAccess`/`AppDB`, `QueueManager` | `store::Store` (versioned JSON queue; SQLite + named/scheduled queues later) |
| File-name helpers, categories folders | `model::{guess_file_name, sanitize_file_name, resolve_dest}` |
| Transient-vs-fatal failures | `CcdmError::is_transient` + 3-attempt backoff in CLI `start` |
| Progressive/adaptive HTTP downloaders | `http::{probe, download_with_resume, download_segmented}` |
| HLS/DASH, `MediaParser` | `media::{parse_master, parse_media, parse_mpd, download_media, download_auto}` (encrypted HLS + live MPD refused with `Unsupported`) |
| WPF/GTK UI + queue window | `ccdm-gpui`: live rows, worker threads, 4 Hz poll loop, clipboard add |

Still to port: FFmpeg wrapper, browser
native-messaging host + extensions, clipboard monitor, scheduler
(`DownloadSchedule`), updater, translations, themes.

License: **GPL-2.0-only** (compatible with XDM's GPL-2.0). See `LICENSE`.

## No-local-Rust workflow

1. Edit files here (or on github.com directly).
2. Commit + push with `git` (the only tools installed: `git`, `gh`).
3. GitHub Actions builds + tests on Windows / Linux / macOS.
4. Download the `.exe` from the run's **Artifacts** section — no `cargo`
   ever runs on your machine.

## First push (repo doesn't exist yet)

Create an **empty** repo on github.com (no README/license template, to avoid
conflicts), e.g. named `ccdwonloadmanager`, then (PowerShell, this folder):

```powershell
& "C:\Program Files\Git\bin\git.exe" remote add origin https://github.com/<YOU>/ccdwonloadmanager.git
& "C:\Program Files\Git\bin\git.exe" push -u origin main
```

(Replace `<YOU>` with your GitHub username. Local history is already
committed on `main`: v0.1 → v0.5.)

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
Edit `config.json` to set `speed_limit_kbps` / `enable_speed_limit`,
`max_connections`, `proxy_url`, or `organize_by_category` by hand.

## GUI usage (after downloading the artifact)

Run `ccdm-gpui`. Copy a download link anywhere, hit **Add from clipboard**
(probes the URL in the background), then **Start**. **Pause** cancels at the
next chunk boundary; **Resume** continues from the `.part` files. The queue
is the same file the CLI uses, so `ccdm-cli list` sees GUI downloads too.
Finished rows offer **Folder** (reveals the file location) and **Again**
(deletes outputs and downloads afresh). The **Organize** toggle sorts new
downloads into `Video/`, `Documents/`, … subfolders. **Speed** and
**Connections** apply to newly started downloads.

## Roadmap

- [x] Live engine ↔ GPUI wiring (progress bars, pause/resume buttons)
- [x] In-GUI speed-limit control + per-download connection setting
- [x] Categories folders, finished-file actions (open folder, re-download)
- [x] HLS (m3u8) + DASH (mpd) downloaders
- [ ] Browser integration (native-messaging host + extension)
- [ ] Clipboard monitor, queue scheduler, shutdown-on-finish
- [ ] Video probe/convert via system ffmpeg, updater, translations
