# EzThumb (Rust)

Generate video thumbnail contact sheets, individual frame snapshots, and animated GIF previews from the command line or desktop GUI.

## Introduction

**EzThumb** is a cross-platform Rust rewrite of [xuminic/ezthumb](https://github.com/xuminic/ezthumb) that builds as a single standalone binary with both a command-line interface and a native `eframe/egui` desktop GUI. It invokes external `ffmpeg` and `ffprobe` binaries for broad container and codec support without bundling legacy C or FFmpeg DLLs.

Running `ezthumb` with no file arguments (or with `-G` / `--gui`) opens the desktop GUI with drag-and-drop batch processing, Dark and Light themes, 1.5x scaled typography, and an inline contact-sheet preview. Passing video files or directories runs headless batch processing directly in the terminal.

## Features

- **FFmpeg and FFprobe backend:** Supports modern video containers and codecs (H.264, H.265/HEVC, VP9, AV1, MP4, MKV, AVI, MOV, WebM, TS) through external `ffmpeg` and `ffprobe` on `PATH`.
- **Flexible output modes:** Creates tiled contact sheets (`-g CxR`, default `4x4`), individual numbered frames (`-g 0xN`), static PNG/JPEG/GIF images, transparent PNG/GIF canvases (`--transparent`), and animated GIFs (`-m gif@800`).
- **Media metadata and timestamps:** Renders a configurable media info header (filename, size, duration, bitrate, video codec/resolution/FPS, audio stream) and per-frame timestamps with TrueType fonts (`ab_glyph`) plus a built-in bitmap fallback.
- **Fast and accurate seeking:** Supports fast keyframe input seek by default, frame-accurate seek (`--accurate`), fixed interval capture (`-t`), start/end time bounds (`--time-from`, `--time-end`), and first/last frame toggles (`--opt-ffr`, `--opt-lfr`).
- **Batch and recursive processing:** Scans folders recursively (`-R`, `--depth`, `--filter`), honors `EZTHUMB` environment defaults, and supports `copy`, `overwrite`, and `skip` conflict policies (`--override`).
- **Desktop GUI (`eframe/egui`):** Includes a Dark/Light theme toggle, 1.5x scaled typography, native file/folder dialogs, drag-and-drop queuing, background worker progress with cancellation, and live preview rendering.

## Requirements

Install **FFmpeg** (`ffmpeg` and `ffprobe`) on your `PATH`, or place `ffmpeg.exe` and `ffprobe.exe` next to `ezthumb.exe`.

## How to Use

### Desktop GUI

1. Launch `ezthumb` with no arguments (or `ezthumb --gui`).
2. Add videos with **Add Files**, **Add Folder**, or drag-and-drop.
3. Open **Settings** to adjust grid dimensions, canvas width, output format (`JPEG`, `PNG`, `GIF`, `Animated GIF`), timestamps, colors, suffix, and output folder.
4. Click **Generate Thumbnails** to process the queue and inspect the generated preview.

### Command Line Examples

Generate a default `4x4` JPEG contact sheet (`1280 px` wide):

```powershell
ezthumb sample.mp4
```

Generate a `3x4` transparent PNG sheet at `1600 px` width without the header block:

```powershell
ezthumb -g 3x4 -w 1600 -m png --transparent --opt-info off sample.mp4
```

Extract `10` separate numbered PNG screenshots from `10%` to `90%` of the video:

```powershell
ezthumb -g 0x10 -m png --time-from 10% --time-end 90% sample.mp4
```

Generate an animated GIF preview (`800 ms` per frame):

```powershell
ezthumb -g 3x4 -s 320x180 -m gif@800 sample.mp4
```

Print short or detailed media metadata without generating images:

```powershell
ezthumb -i sample.mp4
ezthumb -I sample.mp4
```

## Build and Test

Install Rust (`stable-x86_64-pc-windows-msvc` on Windows, or stable Rust on Linux/macOS), then run:

```powershell
cargo fmt --all -- --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The standalone executable builds to `target\release\ezthumb.exe` (or `target/release/ezthumb` on Unix).

## Attribution and Credits

- Original concept, C implementation, and CLI specification by **Andy Xuming** at [xuminic/ezthumb](https://github.com/xuminic/ezthumb).
- Media probing and frame decoding powered by [FFmpeg](https://ffmpeg.org/).
- Original project inspired by **movie thumbnailer (`mtn`)**.

## License

GNU General Public License v3.0 (`GPL-3.0-only`). Refer to `COPYING`.
