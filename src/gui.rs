use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Rect, RichText, Sense, Stroke,
    Style, TextStyle, Ui, Visuals,
};

use crate::cli::collect_video_files;
use crate::config::{
    ConflictPolicy, DurationScanMode, EzConfig, FontSource, GridSetting, OutputFormat,
    ProcessMethod, RgbColor, SizeSpec, TextPosition, ThemeMode,
};
use crate::probe::{find_binary, probe_video_with_mode};
use crate::render::{discover_system_fonts, process_video, SystemFontEntry};

/// Global typography multiplier applied across all egui text styles.
pub const FONT_SCALE: f32 = 1.5_f32;

const BASE_HEADER_FONT_SIZE: f32 = 17.0_f32;
const LABEL_COL_WIDTH: f32 = 245.0_f32;
const COMBO_WIDTH: f32 = 365.0_f32;

/// Scales every default egui text style by FONT_SCALE.
pub fn scaled_text_styles() -> BTreeMap<TextStyle, FontId> {
    Style::default()
        .text_styles
        .into_iter()
        .map(|(style, mut font_id)| {
            font_id.size *= FONT_SCALE;
            (style, font_id)
        })
        .collect()
}

/// Formats milliseconds as compact H:MM:SS for the Queue Length column.
pub fn format_duration_compact(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

/// Builds the compact Queue footer summary from active grid, zoom, and scan settings.
pub fn format_queue_footer(config: &EzConfig) -> String {
    let grid_part = match config.grid_setting {
        GridSetting::Auto => "Grid: Auto".to_string(),
        GridSetting::ColumnAndRow => {
            if config.grid.is_individual() {
                format!("Discrete: {} frames", config.grid.rows)
            } else {
                format!("Grid: {}x{}", config.grid.cols, config.grid.rows)
            }
        }
        GridSetting::ColumnAndStep => {
            let step_s = config
                .timestep_ms
                .filter(|s| *s > 0)
                .map(|ms| (ms / 1000).max(1))
                .unwrap_or(60);
            format!("Grid: {} cols (Every {}s)", config.grid.cols.max(1), step_s)
        }
        GridSetting::DiscreteByNumber => format!("Discrete: {} frames", config.grid.rows.max(1)),
        GridSetting::DiscreteByStep => {
            let step_s = config
                .timestep_ms
                .filter(|s| *s > 0)
                .map(|ms| (ms / 1000).max(1))
                .unwrap_or(60);
            format!("Discrete: Every {step_s}s")
        }
        GridSetting::DiscreteKeyFrames => "Discrete: Key Frames".to_string(),
    };

    let zoom_part = match config.size {
        SizeSpec::Auto => "Zoom: Auto".to_string(),
        SizeSpec::ZoomPercent(pct) => format!("Zoom to {pct}%"),
        SizeSpec::CanvasWidth(w) => format!("Canvas {w}px"),
        SizeSpec::TilePixels { width, height } => format!("Size {width}x{height}"),
    };

    let scan_part = match config.duration_scan {
        DurationScanMode::Auto => "Duration: Auto",
        DurationScanMode::FastScan => "Fast Scan",
        DurationScanMode::FullScan => "Full Scan",
        DurationScanMode::FileHead => "File Head",
    };

    format!("{grid_part}  |  {zoom_part}  |  {scan_part}")
}

/// Launches the eframe/egui desktop window.
pub fn run_gui(initial_files: Vec<PathBuf>, config: EzConfig) -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("EzThumb")
        .with_inner_size([1040.0, 690.0])
        .with_min_inner_size([920.0, 580.0]);

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "EzThumb",
        options,
        Box::new(move |cc| {
            let app = EzThumbApp::new(initial_files, config);
            apply_style(&cc.egui_ctx, app.config.theme);
            Ok(Box::new(app))
        }),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    Queue,
    Settings,
    About,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomMode {
    Auto,
    ZoomByRatio,
    ZoomByCanvas,
    ZoomBySize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Neutral,
    Success,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeState {
    Pending,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RowProgress {
    Queued,
    Running(u8),
    Done,
    Skipped,
    Error(String),
}

impl RowProgress {
    fn label(&self) -> String {
        match self {
            Self::Queued => "0%".to_string(),
            Self::Running(pct) => format!("{pct}%"),
            Self::Done => "100%".to_string(),
            Self::Skipped => "Skipped".to_string(),
            Self::Error(_) => "Error".to_string(),
        }
    }
}

struct QueueEntry {
    path: PathBuf,
    size_bytes: Option<u64>,
    duration_ms: Option<u64>,
    resolution: Option<(u32, u32)>,
    probe_state: ProbeState,
    progress: RowProgress,
    output_path: Option<PathBuf>,
}

enum ProbeEvent {
    Loaded {
        path: PathBuf,
        size_bytes: u64,
        duration_ms: u64,
        width: u32,
        height: u32,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

enum WorkerEvent {
    RowProgress {
        path: PathBuf,
        file_index: usize,
        total_files: usize,
        shot_done: usize,
        shot_total: usize,
        file_name: String,
    },
    RowComplete {
        path: PathBuf,
        skipped: bool,
        output_path: Option<PathBuf>,
    },
    RowError {
        path: PathBuf,
        error: String,
    },
    Finished {
        succeeded: usize,
        skipped: usize,
        failed: usize,
        cancelled: bool,
    },
}

struct SettingsDraft {
    config: EzConfig,
    timestep_secs: u64,
    zoom_mode: ZoomMode,
    zoom_percent: u32,
    canvas_width: u32,
    tile_width: u32,
    tile_height: u32,
    use_custom_outdir: bool,
    outdir_input: String,
    bg_hex_input: String,
    info_hex_input: String,
    time_hex_input: String,
}

impl SettingsDraft {
    fn from_config(config: &EzConfig) -> Self {
        let timestep_secs = config
            .timestep_ms
            .filter(|s| *s > 0)
            .map(|ms| (ms / 1000).max(1))
            .unwrap_or(60);

        let (zoom_mode, zoom_percent, canvas_width, tile_width, tile_height) = match config.size {
            SizeSpec::Auto => (ZoomMode::Auto, 50, 1280, 320, 180),
            SizeSpec::ZoomPercent(pct) => (ZoomMode::ZoomByRatio, pct, 1280, 320, 180),
            SizeSpec::CanvasWidth(w) => (ZoomMode::ZoomByCanvas, 50, w, 320, 180),
            SizeSpec::TilePixels { width, height } => {
                (ZoomMode::ZoomBySize, 50, 1280, width, height)
            }
        };

        let use_custom_outdir = config.outdir.is_some();
        let outdir_input = config
            .outdir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        Self {
            bg_hex_input: config.canvas_color.to_hex(),
            info_hex_input: config.info_color.to_hex(),
            time_hex_input: config.timestamp_color.to_hex(),
            config: config.clone(),
            timestep_secs,
            zoom_mode,
            zoom_percent,
            canvas_width,
            tile_width,
            tile_height,
            use_custom_outdir,
            outdir_input,
        }
    }

    /// Synchronizes UI draft helper fields into a candidate EzConfig.
    fn to_config(&self) -> Result<EzConfig, String> {
        let mut out = self.config.clone();

        match out.grid_setting {
            GridSetting::Auto => {
                out.timestep_ms = None;
            }
            GridSetting::ColumnAndRow => {
                out.grid.cols = out.grid.cols.clamp(1, 32);
                out.grid.rows = out.grid.rows.clamp(1, 200);
                out.timestep_ms = None;
            }
            GridSetting::ColumnAndStep => {
                out.grid.cols = out.grid.cols.clamp(1, 32);
                out.grid.rows = out.grid.rows.clamp(1, 200);
                out.timestep_ms = Some(self.timestep_secs.clamp(1, 3600) * 1000);
            }
            GridSetting::DiscreteByNumber => {
                out.grid.cols = 0;
                out.grid.rows = out.grid.rows.clamp(1, 200);
                out.timestep_ms = None;
            }
            GridSetting::DiscreteByStep => {
                out.grid.cols = 0;
                out.timestep_ms = Some(self.timestep_secs.clamp(1, 3600) * 1000);
            }
            GridSetting::DiscreteKeyFrames => {
                out.grid.cols = 0;
                out.timestep_ms = None;
            }
        }

        out.size = match self.zoom_mode {
            ZoomMode::Auto => SizeSpec::Auto,
            ZoomMode::ZoomByRatio => SizeSpec::ZoomPercent(self.zoom_percent.clamp(10, 400)),
            ZoomMode::ZoomByCanvas => SizeSpec::CanvasWidth(self.canvas_width.clamp(320, 8192)),
            ZoomMode::ZoomBySize => SizeSpec::TilePixels {
                width: self.tile_width.clamp(32, 4096),
                height: self.tile_height.clamp(24, 4096),
            },
        };

        out.accurate_seek = out.process_method == ProcessMethod::FrameByFrame;

        if out.font_source == FontSource::SystemDefault {
            out.font_path = None;
        }

        if self.use_custom_outdir {
            let trimmed = self.outdir_input.trim();
            if trimmed.is_empty() {
                return Err("Select an output directory or switch to With the Media Files.".into());
            }
            out.outdir = Some(PathBuf::from(trimmed));
        } else {
            out.outdir = None;
        }

        out.canvas_color = RgbColor::parse_hex(&self.bg_hex_input)
            .map_err(|_| "BG Hex must be 6 hexadecimal digits.".to_string())?;
        out.info_color = RgbColor::parse_hex(&self.info_hex_input)
            .map_err(|_| "Info Hex must be 6 hexadecimal digits.".to_string())?;
        out.timestamp_color = RgbColor::parse_hex(&self.time_hex_input)
            .map_err(|_| "Time Hex must be 6 hexadecimal digits.".to_string())?;

        Ok(out)
    }

    fn is_dirty(&self, saved: &EzConfig) -> bool {
        match self.to_config() {
            Ok(mut candidate) => {
                candidate.theme = saved.theme;
                candidate != *saved
            }
            Err(_) => true,
        }
    }
}

struct EzThumbApp {
    config: EzConfig,
    draft: SettingsDraft,
    active_tab: ActiveTab,
    entries: Vec<QueueEntry>,
    selected_index: Option<usize>,
    system_fonts: Vec<SystemFontEntry>,
    ffmpeg_path: Option<PathBuf>,
    ffprobe_path: Option<PathBuf>,
    status_message: Option<String>,
    status_kind: StatusKind,
    settings_feedback: Option<(String, StatusKind)>,
    probe_tx: Sender<ProbeEvent>,
    probe_rx: Receiver<ProbeEvent>,
    worker_rx: Option<Receiver<WorkerEvent>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    progress_fraction: f32,
    progress_label: String,
}

impl EzThumbApp {
    fn new(initial_files: Vec<PathBuf>, config: EzConfig) -> Self {
        let ffmpeg_path = find_binary("ffmpeg");
        let ffprobe_path = find_binary("ffprobe");
        let system_fonts = discover_system_fonts();
        let (probe_tx, probe_rx) = mpsc::channel();
        let draft = SettingsDraft::from_config(&config);

        let (status_message, status_kind) = match (&ffmpeg_path, &ffprobe_path) {
            (Some(_), Some(_)) => (None, StatusKind::Neutral),
            _ => (
                Some(
                    "FFmpeg or FFprobe not found on PATH. Install FFmpeg before running jobs."
                        .to_string(),
                ),
                StatusKind::Error,
            ),
        };

        let mut app = Self {
            config,
            draft,
            active_tab: ActiveTab::Queue,
            entries: Vec::new(),
            selected_index: None,
            system_fonts,
            ffmpeg_path,
            ffprobe_path,
            status_message,
            status_kind,
            settings_feedback: None,
            probe_tx,
            probe_rx,
            worker_rx: None,
            cancel_flag: None,
            progress_fraction: 0.0,
            progress_label: String::new(),
        };

        if !initial_files.is_empty() {
            app.add_paths(initial_files);
        }
        app
    }

    fn is_running(&self) -> bool {
        self.worker_rx.is_some()
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let resolved = collect_video_files(&paths, &self.config);
        let mut to_probe = Vec::new();
        for path in resolved {
            if self.entries.iter().any(|e| e.path == path) {
                continue;
            }
            let size_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
            self.entries.push(QueueEntry {
                path: path.clone(),
                size_bytes,
                duration_ms: None,
                resolution: None,
                probe_state: if self.ffprobe_path.is_some() {
                    ProbeState::Pending
                } else {
                    ProbeState::Failed("ffprobe not found on PATH".to_string())
                },
                progress: RowProgress::Queued,
                output_path: None,
            });
            to_probe.push(path);
        }

        if !to_probe.is_empty() && self.ffprobe_path.is_some() {
            let tx = self.probe_tx.clone();
            let stream_idx = self.config.video_stream_index;
            let scan_mode = self.config.duration_scan;
            thread::spawn(move || {
                for path in to_probe {
                    match probe_video_with_mode(&path, stream_idx, scan_mode) {
                        Ok(info) => {
                            let _ = tx.send(ProbeEvent::Loaded {
                                path,
                                size_bytes: info.file_size_bytes,
                                duration_ms: info.duration_ms,
                                width: info.display_width,
                                height: info.display_height,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(ProbeEvent::Failed {
                                path,
                                error: err.to_string(),
                            });
                        }
                    }
                }
            });
        }
    }

    fn remove_selected(&mut self) {
        if let Some(i) = self.selected_index.take() {
            if i < self.entries.len() {
                self.entries.remove(i);
                if !self.entries.is_empty() {
                    self.selected_index = Some(i.min(self.entries.len() - 1));
                }
            }
        }
    }

    fn start_batch(&mut self) {
        if self.entries.is_empty() {
            self.status_message = Some("Add at least one video file first.".to_string());
            self.status_kind = StatusKind::Error;
            return;
        }
        if self.ffmpeg_path.is_none() || self.ffprobe_path.is_none() {
            self.status_message = Some("FFmpeg and FFprobe must be available on PATH.".to_string());
            self.status_kind = StatusKind::Error;
            return;
        }

        for entry in &mut self.entries {
            entry.progress = RowProgress::Queued;
            entry.output_path = None;
        }

        let files: Vec<PathBuf> = self.entries.iter().map(|e| e.path.clone()).collect();
        let cfg = self.config.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();

        self.progress_fraction = 0.0;
        self.progress_label = format!("Processing {} file(s)...", files.len());
        self.status_message = None;
        self.status_kind = StatusKind::Neutral;
        self.cancel_flag = Some(cancel);
        self.worker_rx = Some(rx);

        thread::spawn(move || {
            let total_files = files.len();
            let mut succeeded = 0usize;
            let mut skipped = 0usize;
            let mut failed = 0usize;
            let mut cancelled = false;

            for (file_idx, path) in files.iter().enumerate() {
                if cancel_worker.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("video")
                    .to_string();

                let tx_prog = tx.clone();
                let path_prog = path.clone();
                let fname_prog = file_name.clone();
                let res = process_video(path, &cfg, Some(&cancel_worker), |done, total| {
                    let _ = tx_prog.send(WorkerEvent::RowProgress {
                        path: path_prog.clone(),
                        file_index: file_idx,
                        total_files,
                        shot_done: done,
                        shot_total: total,
                        file_name: fname_prog.clone(),
                    });
                });

                match res {
                    Ok(outcome) => {
                        if outcome.skipped {
                            skipped += 1;
                            let _ = tx.send(WorkerEvent::RowComplete {
                                path: path.clone(),
                                skipped: true,
                                output_path: None,
                            });
                        } else {
                            succeeded += 1;
                            let _ = tx.send(WorkerEvent::RowComplete {
                                path: path.clone(),
                                skipped: false,
                                output_path: outcome.output_paths.into_iter().next(),
                            });
                        }
                    }
                    Err(err) => {
                        if cancel_worker.load(Ordering::Relaxed) {
                            cancelled = true;
                            break;
                        }
                        failed += 1;
                        let _ = tx.send(WorkerEvent::RowError {
                            path: path.clone(),
                            error: err.to_string(),
                        });
                    }
                }
            }

            let _ = tx.send(WorkerEvent::Finished {
                succeeded,
                skipped,
                failed,
                cancelled,
            });
        });
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        let mut any_probe_pending = false;
        while let Ok(ev) = self.probe_rx.try_recv() {
            match ev {
                ProbeEvent::Loaded {
                    path,
                    size_bytes,
                    duration_ms,
                    width,
                    height,
                } => {
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                        entry.size_bytes = Some(size_bytes);
                        entry.duration_ms = Some(duration_ms);
                        entry.resolution = Some((width, height));
                        entry.probe_state = ProbeState::Ready;
                    }
                }
                ProbeEvent::Failed { path, error } => {
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                        entry.probe_state = ProbeState::Failed(error);
                    }
                }
            }
        }
        for entry in &self.entries {
            if entry.probe_state == ProbeState::Pending {
                any_probe_pending = true;
                break;
            }
        }

        let mut finished = false;
        let mut pending_events = Vec::new();
        if let Some(rx) = &self.worker_rx {
            while let Ok(ev) = rx.try_recv() {
                pending_events.push(ev);
            }
        }

        for ev in pending_events {
            match ev {
                WorkerEvent::RowProgress {
                    path,
                    file_index,
                    total_files,
                    shot_done,
                    shot_total,
                    file_name,
                } => {
                    let row_frac = if shot_total > 0 {
                        (shot_done as f32) / (shot_total as f32)
                    } else {
                        0.0
                    };
                    let pct = (row_frac * 100.0).round().clamp(0.0, 99.0) as u8;
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                        entry.progress = RowProgress::Running(pct);
                    }
                    self.progress_fraction =
                        ((file_index as f32) + row_frac) / (total_files.max(1) as f32);
                    self.progress_label = format!(
                        "[{}/{}] {} ({}/{})",
                        file_index + 1,
                        total_files,
                        file_name,
                        shot_done,
                        shot_total
                    );
                }
                WorkerEvent::RowComplete {
                    path,
                    skipped,
                    output_path,
                } => {
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                        entry.progress = if skipped {
                            RowProgress::Skipped
                        } else {
                            RowProgress::Done
                        };
                        entry.output_path = output_path;
                    }
                }
                WorkerEvent::RowError { path, error } => {
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
                        entry.progress = RowProgress::Error(error);
                    }
                }
                WorkerEvent::Finished {
                    succeeded,
                    skipped,
                    failed,
                    cancelled,
                } => {
                    finished = true;
                    if !cancelled {
                        self.progress_fraction = 1.0;
                    }
                    if cancelled {
                        self.status_message = Some(format!(
                            "Cancelled. Saved {succeeded}, skipped {skipped}, failed {failed}."
                        ));
                        self.status_kind = StatusKind::Error;
                    } else if failed > 0 {
                        self.status_message = Some(format!(
                            "Finished with errors. Saved {succeeded}, skipped {skipped}, failed {failed}."
                        ));
                        self.status_kind = StatusKind::Error;
                    } else {
                        self.status_message =
                            Some(format!("Done. Saved {succeeded}, skipped {skipped}."));
                        self.status_kind = StatusKind::Success;
                    }
                }
            }
        }

        if finished {
            self.worker_rx = None;
            self.cancel_flag = None;
        }

        if self.is_running() || any_probe_pending {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
    }

    fn selected_font_display(&self) -> String {
        match (self.draft.config.font_source, &self.draft.config.font_path) {
            (FontSource::SystemDefault, _) => "Segoe UI (Default)".to_string(),
            (FontSource::SystemFont, Some(path)) => self
                .system_fonts
                .iter()
                .find(|f| &f.path == path)
                .map(|f| f.display_name.clone())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("System Font")
                        .to_string()
                }),
            (FontSource::SystemFont, None) => "Select an installed font".to_string(),
            (FontSource::CustomFile, Some(path)) => path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Custom Font")
                .to_string(),
            (FontSource::CustomFile, None) => "No custom font selected".to_string(),
        }
    }

    fn render_queue_table(&mut self, ui: &mut Ui) {
        let dark = self.config.theme == ThemeMode::Dark;
        let running = self.is_running();

        if !running && ui.input(|i| i.key_pressed(egui::Key::Delete)) {
            self.remove_selected();
        }

        let border_color = if dark {
            Color32::from_rgb(56, 62, 74)
        } else {
            Color32::from_rgb(190, 196, 206)
        };
        let header_bg = if dark {
            Color32::from_rgb(34, 38, 46)
        } else {
            Color32::from_rgb(241, 243, 246)
        };
        let table_bg = if dark {
            Color32::from_rgb(28, 31, 37)
        } else {
            Color32::from_rgb(255, 255, 255)
        };
        let alt_row_bg = if dark {
            Color32::from_rgb(32, 36, 43)
        } else {
            Color32::from_rgb(248, 250, 252)
        };
        let selected_bg = if dark {
            Color32::from_rgb(37, 74, 138)
        } else {
            Color32::from_rgb(219, 234, 254)
        };

        let avail = ui.available_size();
        let table_rect =
            Rect::from_min_size(ui.cursor().min, egui::vec2(avail.x, avail.y.max(120.0)));

        let w_progress = 112.0_f32;
        let w_resolution = 134.0_f32;
        let w_length = 108.0_f32;
        let w_size = 118.0_f32;
        let w_files =
            (table_rect.width() - w_size - w_length - w_resolution - w_progress).max(180.0);

        let col_xs = [
            table_rect.left(),
            table_rect.left() + w_files,
            table_rect.left() + w_files + w_size,
            table_rect.left() + w_files + w_size + w_length,
            table_rect.left() + w_files + w_size + w_length + w_resolution,
            table_rect.right(),
        ];

        let header_h = 34.0_f32;
        let row_h = 32.0_f32;

        let painter = ui.painter();
        painter.rect_filled(table_rect, CornerRadius::same(2), table_bg);
        let header_rect =
            Rect::from_min_size(table_rect.min, egui::vec2(table_rect.width(), header_h));
        painter.rect_filled(header_rect, CornerRadius::ZERO, header_bg);

        let header_font = TextStyle::Body.resolve(ui.style());
        let header_text_color = ui.visuals().strong_text_color();
        let headers = ["Files", "Size", "Length", "Resolution", "Progress"];
        for (i, title) in headers.iter().enumerate() {
            let cell_rect = Rect::from_min_max(
                egui::pos2(col_xs[i] + 8.0, header_rect.top()),
                egui::pos2(col_xs[i + 1] - 6.0, header_rect.bottom()),
            );
            painter.with_clip_rect(cell_rect).text(
                egui::pos2(cell_rect.left(), cell_rect.center().y),
                egui::Align2::LEFT_CENTER,
                *title,
                header_font.clone(),
                header_text_color,
            );
        }

        let body_rect = Rect::from_min_max(
            egui::pos2(table_rect.left(), table_rect.top() + header_h),
            table_rect.max,
        );
        let mut open_output_path: Option<PathBuf> = None;
        let mut context_remove_idx: Option<usize> = None;
        let mut context_clear = false;

        ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
            ui.set_clip_rect(body_rect);
            egui::ScrollArea::vertical()
                .id_salt("queue_table_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.entries.is_empty() {
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new("Drop video files here or click Add Files.").weak(),
                            );
                        });
                    } else {
                        let text_color = ui.visuals().text_color();
                        let muted_color = if dark {
                            Color32::from_rgb(176, 185, 198)
                        } else {
                            Color32::from_rgb(82, 92, 108)
                        };
                        let row_font = FontId::proportional(19.0);

                        for idx in 0..self.entries.len() {
                            let (row_rect, response) = ui.allocate_exact_size(
                                egui::vec2(body_rect.width(), row_h),
                                Sense::click(),
                            );

                            let is_selected = self.selected_index == Some(idx);
                            if response.clicked() {
                                self.selected_index = Some(idx);
                            }
                            if response.double_clicked() {
                                if let Some(out) = &self.entries[idx].output_path {
                                    open_output_path = Some(out.clone());
                                }
                            }

                            let fill = if is_selected {
                                selected_bg
                            } else if idx % 2 == 1 {
                                alt_row_bg
                            } else {
                                table_bg
                            };
                            ui.painter().rect_filled(row_rect, CornerRadius::ZERO, fill);

                            let entry = &self.entries[idx];
                            let file_text = entry.path.display().to_string();
                            let size_text = entry
                                .size_bytes
                                .map(|b| self.config.size_unit.format_bytes(b))
                                .unwrap_or_else(|| "-".to_string());
                            let length_text = match (entry.duration_ms, &entry.probe_state) {
                                (Some(ms), _) => format_duration_compact(ms),
                                (None, ProbeState::Pending) => "...".to_string(),
                                (None, _) => "-".to_string(),
                            };
                            let res_text = match (entry.resolution, &entry.probe_state) {
                                (Some((w, h)), _) => format!("{w}x{h}"),
                                (None, ProbeState::Pending) => "...".to_string(),
                                (None, _) => "-".to_string(),
                            };
                            let prog_text = entry.progress.label();
                            let prog_color = match &entry.progress {
                                RowProgress::Done => {
                                    if dark {
                                        Color32::from_rgb(74, 222, 128)
                                    } else {
                                        Color32::from_rgb(21, 128, 61)
                                    }
                                }
                                RowProgress::Error(_) => {
                                    if dark {
                                        Color32::from_rgb(248, 113, 113)
                                    } else {
                                        Color32::from_rgb(185, 28, 28)
                                    }
                                }
                                RowProgress::Running(_) => {
                                    if dark {
                                        Color32::from_rgb(96, 165, 250)
                                    } else {
                                        Color32::from_rgb(29, 78, 216)
                                    }
                                }
                                RowProgress::Queued | RowProgress::Skipped => muted_color,
                            };

                            let cells: [(&str, Color32); 5] = [
                                (&file_text, text_color),
                                (&size_text, muted_color),
                                (&length_text, muted_color),
                                (&res_text, muted_color),
                                (&prog_text, prog_color),
                            ];

                            for (c_idx, (val, col)) in cells.iter().enumerate() {
                                let cell_rect = Rect::from_min_max(
                                    egui::pos2(col_xs[c_idx] + 8.0, row_rect.top()),
                                    egui::pos2(col_xs[c_idx + 1] - 6.0, row_rect.bottom()),
                                );
                                ui.painter().with_clip_rect(cell_rect).text(
                                    egui::pos2(cell_rect.left(), cell_rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    *val,
                                    row_font.clone(),
                                    *col,
                                );
                            }

                            let tooltip =
                                match (&entry.progress, &entry.output_path, &entry.probe_state) {
                                    (RowProgress::Error(err), _, _) => {
                                        format!("{file_text}\nError: {err}")
                                    }
                                    (_, Some(out), _) => {
                                        format!(
                                            "{file_text}\nOutput: {}\nDouble-click to open.",
                                            out.display()
                                        )
                                    }
                                    (_, _, ProbeState::Failed(err)) => {
                                        format!("{file_text}\nProbe error: {err}")
                                    }
                                    _ => file_text.clone(),
                                };
                            let has_output = entry.output_path.is_some();
                            let out_clone = entry.output_path.clone();

                            response.on_hover_text(tooltip).context_menu(|ui| {
                                if has_output && ui.button("Open Output").clicked() {
                                    open_output_path = out_clone.clone();
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(!running, egui::Button::new("Remove Selected"))
                                    .clicked()
                                {
                                    context_remove_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(!running, egui::Button::new("Clear Queue"))
                                    .clicked()
                                {
                                    context_clear = true;
                                    ui.close_menu();
                                }
                            });
                        }
                    }
                });
        });

        let stroke = Stroke::new(1.0_f32, border_color);
        let painter = ui.painter();
        painter.line_segment(
            [
                egui::pos2(table_rect.left(), table_rect.top() + header_h),
                egui::pos2(table_rect.right(), table_rect.top() + header_h),
            ],
            stroke,
        );
        for &x in &col_xs[1..5] {
            painter.line_segment(
                [
                    egui::pos2(x, table_rect.top() + header_h),
                    egui::pos2(x, table_rect.bottom()),
                ],
                stroke,
            );
        }
        painter.rect_stroke(
            table_rect,
            CornerRadius::same(2),
            stroke,
            egui::StrokeKind::Inside,
        );

        ui.allocate_rect(table_rect, Sense::hover());

        if let Some(idx) = context_remove_idx {
            self.selected_index = Some(idx);
            self.remove_selected();
        }
        if context_clear {
            self.entries.clear();
            self.selected_index = None;
        }
        if let Some(out) = open_output_path {
            open_file_in_os(&out);
        }
    }

    fn render_queue_footer(&mut self, ui: &mut Ui) {
        let running = self.is_running();

        if running {
            ui.add_space(4.0);
            ui.add(
                egui::ProgressBar::new(self.progress_fraction)
                    .text(self.progress_label.clone())
                    .desired_width(ui.available_width()),
            );
        } else if let Some(msg) = &self.status_message {
            ui.add_space(4.0);
            let status_color = status_text_color(self.status_kind, self.config.theme);
            ui.label(RichText::new(msg).color(status_color).size(16.0));
        }

        ui.add_space(6.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 38.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                let summary = format_queue_footer(&self.config);
                ui.label(RichText::new(summary).size(16.5));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        if ui
                            .add(egui::Button::new("Cancel").min_size(egui::vec2(92.0, 34.0)))
                            .clicked()
                        {
                            if let Some(flag) = &self.cancel_flag {
                                flag.store(true, Ordering::Relaxed);
                            }
                        }
                    } else if ui
                        .add_enabled(
                            !self.entries.is_empty(),
                            egui::Button::new("Run").min_size(egui::vec2(88.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.start_batch();
                    }

                    if ui
                        .add_enabled(
                            !running && self.selected_index.is_some(),
                            egui::Button::new("Remove").min_size(egui::vec2(96.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.remove_selected();
                    }

                    if ui
                        .add_enabled(
                            !running,
                            egui::Button::new("Add Folder").min_size(egui::vec2(116.0, 34.0)),
                        )
                        .clicked()
                    {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            self.add_paths(vec![folder]);
                        }
                    }

                    if ui
                        .add_enabled(
                            !running,
                            egui::Button::new("Add Files").min_size(egui::vec2(104.0, 34.0)),
                        )
                        .clicked()
                    {
                        if let Some(picked) = rfd::FileDialog::new()
                            .add_filter(
                                "Video files",
                                &[
                                    "mp4", "mkv", "avi", "mov", "webm", "wmv", "flv", "ts", "mpg",
                                    "m4v",
                                ],
                            )
                            .pick_files()
                        {
                            self.add_paths(picked);
                        }
                    }
                });
            },
        );
        ui.add_space(6.0);
    }

    fn render_settings_tab(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);

                // Section 1: Profile Selection
                section_header(ui, "Profile Selection");
                indented_form_row(ui, "Grid Setting", |ui| {
                    egui::ComboBox::from_id_salt("grid_setting_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(grid_setting_label(self.draft.config.grid_setting))
                        .show_ui(ui, |ui| {
                            for mode in [
                                GridSetting::Auto,
                                GridSetting::ColumnAndRow,
                                GridSetting::ColumnAndStep,
                                GridSetting::DiscreteByNumber,
                                GridSetting::DiscreteByStep,
                                GridSetting::DiscreteKeyFrames,
                            ] {
                                if ui
                                    .selectable_value(
                                        &mut self.draft.config.grid_setting,
                                        mode,
                                        grid_setting_label(mode),
                                    )
                                    .changed()
                                    && matches!(
                                        mode,
                                        GridSetting::ColumnAndRow | GridSetting::ColumnAndStep
                                    )
                                    && self.draft.config.grid.cols == 0
                                {
                                    self.draft.config.grid.cols = 4;
                                }
                            }
                        });

                    ui.add_space(10.0);
                    match self.draft.config.grid_setting {
                        GridSetting::Auto => {}
                        GridSetting::ColumnAndRow => {
                            ui.label("Grid");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.config.grid.cols)
                                    .range(1..=32),
                            );
                            ui.label("x");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.config.grid.rows)
                                    .range(1..=200),
                            );
                        }
                        GridSetting::ColumnAndStep => {
                            ui.label("Cols");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.config.grid.cols)
                                    .range(1..=32),
                            );
                            ui.label("Every");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.timestep_secs)
                                    .range(1..=3600)
                                    .suffix(" s"),
                            );
                        }
                        GridSetting::DiscreteByNumber => {
                            ui.label("Count");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.config.grid.rows)
                                    .range(1..=200),
                            );
                        }
                        GridSetting::DiscreteByStep => {
                            ui.label("Every");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.timestep_secs)
                                    .range(1..=3600)
                                    .suffix(" s"),
                            );
                        }
                        GridSetting::DiscreteKeyFrames => {
                            ui.label(RichText::new("I-frames").weak());
                        }
                    }
                });

                indented_form_row(ui, "Zoom Setting", |ui| {
                    egui::ComboBox::from_id_salt("zoom_mode_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(zoom_mode_label(self.draft.zoom_mode))
                        .show_ui(ui, |ui| {
                            for mode in [
                                ZoomMode::Auto,
                                ZoomMode::ZoomByRatio,
                                ZoomMode::ZoomByCanvas,
                                ZoomMode::ZoomBySize,
                            ] {
                                ui.selectable_value(
                                    &mut self.draft.zoom_mode,
                                    mode,
                                    zoom_mode_label(mode),
                                );
                            }
                        });

                    ui.add_space(10.0);
                    match self.draft.zoom_mode {
                        ZoomMode::Auto => {}
                        ZoomMode::ZoomByRatio => {
                            ui.label("Ratio");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.zoom_percent)
                                    .range(10..=400)
                                    .suffix("%"),
                            );
                        }
                        ZoomMode::ZoomByCanvas => {
                            ui.label("Width");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.canvas_width)
                                    .range(320..=8192)
                                    .suffix(" px"),
                            );
                        }
                        ZoomMode::ZoomBySize => {
                            ui.label("Tile");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.tile_width).range(32..=4096),
                            );
                            ui.label("x");
                            ui.add(
                                egui::DragValue::new(&mut self.draft.tile_height).range(24..=4096),
                            );
                        }
                    }
                });

                indented_form_row(ui, "Layout Spacing", |ui| {
                    ui.label("Gap");
                    ui.add(egui::DragValue::new(&mut self.draft.config.gap_px).range(0..=48));
                    ui.add_space(10.0);
                    ui.label("Margin");
                    ui.add(egui::DragValue::new(&mut self.draft.config.margin_px).range(0..=64));
                    ui.add_space(10.0);
                    ui.label("Edge");
                    ui.add(egui::DragValue::new(&mut self.draft.config.edge_px).range(0..=16));
                });

                ui.add_space(10.0);

                // Section 2: Media Processing
                section_header(ui, "Media Processing");
                indented_form_row(ui, "Find Media Duration By", |ui| {
                    egui::ComboBox::from_id_salt("duration_scan_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(duration_scan_label(self.draft.config.duration_scan))
                        .show_ui(ui, |ui| {
                            for mode in [
                                DurationScanMode::Auto,
                                DurationScanMode::FastScan,
                                DurationScanMode::FullScan,
                                DurationScanMode::FileHead,
                            ] {
                                ui.selectable_value(
                                    &mut self.draft.config.duration_scan,
                                    mode,
                                    duration_scan_label(mode),
                                );
                            }
                        });
                });

                indented_form_row(ui, "Media Process Method", |ui| {
                    egui::ComboBox::from_id_salt("process_method_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(process_method_label(self.draft.config.process_method))
                        .show_ui(ui, |ui| {
                            for method in [
                                ProcessMethod::Auto,
                                ProcessMethod::UponKeyFrames,
                                ProcessMethod::FrameByFrame,
                                ProcessMethod::SinglePass,
                                ProcessMethod::TwoPass,
                                ProcessMethod::SafeMode,
                                ProcessMethod::KeyFrameRip,
                            ] {
                                ui.selectable_value(
                                    &mut self.draft.config.process_method,
                                    method,
                                    process_method_label(method),
                                );
                            }
                        });
                });

                indented_form_row(ui, "Capture Options", |ui| {
                    ui.checkbox(&mut self.draft.config.include_first_frame, "First frame");
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.draft.config.include_last_frame, "Last frame");
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.draft.config.recursive, "Recursive folders");
                });

                ui.add_space(10.0);

                // Section 3: Font
                section_header(ui, "Font");
                let font_display = self.selected_font_display();
                indented_form_row(ui, "Choose Font", |ui| {
                    egui::ComboBox::from_id_salt("font_source_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(font_source_label(self.draft.config.font_source))
                        .show_ui(ui, |ui| {
                            for src in [
                                FontSource::SystemDefault,
                                FontSource::SystemFont,
                                FontSource::CustomFile,
                            ] {
                                if ui
                                    .selectable_value(
                                        &mut self.draft.config.font_source,
                                        src,
                                        font_source_label(src),
                                    )
                                    .changed()
                                {
                                    match src {
                                        FontSource::SystemDefault => {
                                            self.draft.config.font_path = None;
                                        }
                                        FontSource::SystemFont => {
                                            if self.draft.config.font_path.is_none() {
                                                if let Some(first) = self.system_fonts.first() {
                                                    self.draft.config.font_path =
                                                        Some(first.path.clone());
                                                }
                                            }
                                        }
                                        FontSource::CustomFile => {}
                                    }
                                }
                            }
                        });

                    match self.draft.config.font_source {
                        FontSource::SystemDefault => {}
                        FontSource::SystemFont => {
                            ui.add_space(8.0);
                            egui::ComboBox::from_id_salt("installed_font_combo")
                                .width(270.0)
                                .selected_text(&font_display)
                                .show_ui(ui, |ui| {
                                    if self.system_fonts.is_empty() {
                                        ui.label("No system fonts found");
                                    } else {
                                        for entry in &self.system_fonts {
                                            let selected = self.draft.config.font_path.as_ref()
                                                == Some(&entry.path);
                                            if ui
                                                .selectable_label(selected, &entry.display_name)
                                                .clicked()
                                            {
                                                self.draft.config.font_path =
                                                    Some(entry.path.clone());
                                            }
                                        }
                                    }
                                });
                        }
                        FontSource::CustomFile => {
                            ui.add_space(8.0);
                            if ui.button("Browse...").clicked() {
                                let mut dlg = rfd::FileDialog::new()
                                    .add_filter("TrueType / OpenType", &["ttf", "otf"]);
                                let win_fonts = Path::new(r"C:\Windows\Fonts");
                                if win_fonts.is_dir() {
                                    dlg = dlg.set_directory(win_fonts);
                                }
                                if let Some(font_file) = dlg.pick_file() {
                                    self.draft.config.font_path = Some(font_file);
                                }
                            }
                        }
                    }
                });

                indented_form_row(ui, "", |ui| {
                    let mut readonly =
                        format!("{font_display}, {}", self.draft.config.info_font_size);
                    ui.add_enabled(
                        false,
                        egui::TextEdit::singleline(&mut readonly).desired_width(COMBO_WIDTH),
                    );
                });

                indented_form_row(ui, "Font Sizes", |ui| {
                    ui.label("Header");
                    ui.add(
                        egui::DragValue::new(&mut self.draft.config.info_font_size).range(10..=48),
                    );
                    ui.add_space(12.0);
                    ui.label("Timestamp");
                    ui.add(
                        egui::DragValue::new(&mut self.draft.config.timestamp_font_size)
                            .range(10..=36),
                    );
                });

                indented_form_row(ui, "Text Overlays", |ui| {
                    ui.checkbox(&mut self.draft.config.show_info, "Media info");
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.draft.config.show_timestamp, "Timestamps");
                    ui.add_space(10.0);
                    egui::ComboBox::from_id_salt("ts_anchor_combo")
                        .width(155.0)
                        .selected_text(position_label(self.draft.config.timestamp_position))
                        .show_ui(ui, |ui| {
                            for pos in [
                                TextPosition::LeftTop,
                                TextPosition::MiddleTop,
                                TextPosition::RightTop,
                                TextPosition::LeftBottom,
                                TextPosition::MiddleBottom,
                                TextPosition::RightBottom,
                            ] {
                                ui.selectable_value(
                                    &mut self.draft.config.timestamp_position,
                                    pos,
                                    position_label(pos),
                                );
                            }
                        });
                });

                indented_form_row(ui, "Colors (Hex)", |ui| {
                    color_hex_field(ui, "BG", &mut self.draft.bg_hex_input);
                    ui.add_space(8.0);
                    color_hex_field(ui, "Info", &mut self.draft.info_hex_input);
                    ui.add_space(8.0);
                    color_hex_field(ui, "Time", &mut self.draft.time_hex_input);
                });

                ui.add_space(10.0);

                // Section 4: Output Directory
                section_header(ui, "Output Directory");
                indented_form_row(ui, "Save Thumbnails", |ui| {
                    egui::ComboBox::from_id_salt("outdir_mode_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(if self.draft.use_custom_outdir {
                            "Custom Directory"
                        } else {
                            "With the Media Files"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.draft.use_custom_outdir,
                                false,
                                "With the Media Files",
                            );
                            ui.selectable_value(
                                &mut self.draft.use_custom_outdir,
                                true,
                                "Custom Directory",
                            );
                        });
                });

                if self.draft.use_custom_outdir {
                    indented_form_row(ui, "Folder Path", |ui| {
                        let edit_w = (ui.available_width() - 120.0).max(200.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.draft.outdir_input)
                                .desired_width(edit_w),
                        );
                        if ui.button("Browse...").clicked() {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.draft.outdir_input = dir.to_string_lossy().to_string();
                            }
                        }
                    });
                }

                ui.add_space(10.0);

                // Section 5: Output Thumbnails
                section_header(ui, "Output Thumbnails");
                indented_form_row(ui, "Suffix of Thumbnails", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.config.suffix)
                            .desired_width(COMBO_WIDTH),
                    );
                });

                indented_form_row(ui, "Existing Thumbnails", |ui| {
                    egui::ComboBox::from_id_salt("conflict_policy_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(conflict_policy_label(self.draft.config.conflict_policy))
                        .show_ui(ui, |ui| {
                            for policy in [
                                ConflictPolicy::CreateNew,
                                ConflictPolicy::Copy,
                                ConflictPolicy::Overwrite,
                                ConflictPolicy::Skip,
                            ] {
                                ui.selectable_value(
                                    &mut self.draft.config.conflict_policy,
                                    policy,
                                    conflict_policy_label(policy),
                                );
                            }
                        });
                });

                indented_form_row(ui, "Save Picture As", |ui| {
                    let mut fmt_idx = match self.draft.config.format {
                        OutputFormat::Jpeg { .. } => 0,
                        OutputFormat::Png => 1,
                        OutputFormat::Gif => 2,
                        OutputFormat::AnimatedGif { .. } => 3,
                    };
                    egui::ComboBox::from_id_salt("save_picture_combo")
                        .width(COMBO_WIDTH)
                        .selected_text(match fmt_idx {
                            0 => "JPEG",
                            1 => "PNG",
                            2 => "GIF",
                            _ => "Animated GIF",
                        })
                        .show_ui(ui, |ui| {
                            if ui.selectable_value(&mut fmt_idx, 0, "JPEG").changed() {
                                self.draft.config.format = OutputFormat::Jpeg { quality: 95 };
                            }
                            if ui.selectable_value(&mut fmt_idx, 1, "PNG").changed() {
                                self.draft.config.format = OutputFormat::Png;
                            }
                            if ui.selectable_value(&mut fmt_idx, 2, "GIF").changed() {
                                self.draft.config.format = OutputFormat::Gif;
                            }
                            if ui
                                .selectable_value(&mut fmt_idx, 3, "Animated GIF")
                                .changed()
                            {
                                self.draft.config.format =
                                    OutputFormat::AnimatedGif { delay_ms: 800 };
                            }
                        });

                    ui.add_space(10.0);
                    match self.draft.config.format {
                        OutputFormat::Jpeg { ref mut quality } => {
                            ui.label("Quality");
                            ui.add(egui::DragValue::new(quality).range(1..=100));
                        }
                        OutputFormat::Png | OutputFormat::Gif => {
                            ui.checkbox(&mut self.draft.config.transparent_bg, "Transparent");
                        }
                        OutputFormat::AnimatedGif { ref mut delay_ms } => {
                            ui.label("Delay (ms)");
                            ui.add(egui::DragValue::new(delay_ms).range(50..=5000));
                            ui.add_space(8.0);
                            ui.checkbox(&mut self.draft.config.transparent_bg, "Transparent");
                        }
                    }
                });

                ui.add_space(12.0);
            });
    }

    fn render_settings_footer(&mut self, ui: &mut Ui) {
        let dirty = self.draft.is_dirty(&self.config);

        ui.add_space(6.0);
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 38.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                if ui
                    .add(egui::Button::new("Reset Defaults").min_size(egui::vec2(140.0, 34.0)))
                    .clicked()
                {
                    let defaults = EzConfig {
                        theme: self.config.theme,
                        ..Default::default()
                    };
                    self.draft = SettingsDraft::from_config(&defaults);
                    self.settings_feedback = Some((
                        "Defaults loaded. Click OK to save.".to_string(),
                        StatusKind::Neutral,
                    ));
                }

                ui.add_space(10.0);
                if let Some((msg, kind)) = &self.settings_feedback {
                    let col = status_text_color(*kind, self.config.theme);
                    ui.label(RichText::new(msg).color(col).size(16.0));
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            dirty,
                            egui::Button::new("OK").min_size(egui::vec2(96.0, 34.0)),
                        )
                        .clicked()
                    {
                        match self.draft.to_config() {
                            Ok(mut next) => {
                                next.theme = self.config.theme;
                                self.config = next;
                                self.draft = SettingsDraft::from_config(&self.config);
                                match self.config.save() {
                                    Ok(()) => {
                                        self.settings_feedback = Some((
                                            "Settings saved.".to_string(),
                                            StatusKind::Success,
                                        ));
                                    }
                                    Err(err) => {
                                        self.settings_feedback = Some((
                                            format!("Save failed: {err}"),
                                            StatusKind::Error,
                                        ));
                                    }
                                }
                            }
                            Err(err) => {
                                self.settings_feedback = Some((err, StatusKind::Error));
                            }
                        }
                    }

                    if ui
                        .add_enabled(
                            dirty,
                            egui::Button::new("Cancel").min_size(egui::vec2(96.0, 34.0)),
                        )
                        .clicked()
                    {
                        self.draft = SettingsDraft::from_config(&self.config);
                        self.settings_feedback =
                            Some(("Changes discarded.".to_string(), StatusKind::Neutral));
                    }
                });
            },
        );
        ui.add_space(6.0);
    }

    fn render_about_tab(&self, ui: &mut Ui) {
        egui::ScrollArea::vertical()
            .id_salt("about_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("EzThumb {}", env!("CARGO_PKG_VERSION")))
                            .strong()
                            .size(24.0 * FONT_SCALE),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        "A video thumbnail contact-sheet generator built in Rust with FFmpeg.",
                    );

                    ui.add_space(10.0);
                    ui.label("Rust Port Maintainer: Aaron Christopher Ramos");
                    ui.label("Original EzThumb: Copyright (C) 2011-2015 Andy Xuming");
                    ui.label("License: GNU General Public License v3.0 (GPL-3.0-only)");
                    ui.label(
                        RichText::new(
                            "This is free software: you may modify and redistribute it under GPL-3.0.",
                        )
                        .weak(),
                    );
                    ui.label(
                        RichText::new("There is NO WARRANTY, to the extent permitted by law.")
                            .weak(),
                    );

                    ui.add_space(12.0);
                    ui.label(RichText::new("External Media Backend").strong());
                    ui.label(
                        "Video probing and frame extraction invoke external FFmpeg and FFprobe:",
                    );
                    ui.hyperlink_to("https://www.ffmpeg.org", "https://www.ffmpeg.org");
                    ui.add_space(2.0);
                    let ff_text = match &self.ffmpeg_path {
                        Some(p) => format!("FFmpeg: {}", p.display()),
                        None => "FFmpeg: not found on PATH".to_string(),
                    };
                    let fp_text = match &self.ffprobe_path {
                        Some(p) => format!("FFprobe: {}", p.display()),
                        None => "FFprobe: not found on PATH".to_string(),
                    };
                    ui.label(RichText::new(ff_text).weak().size(15.5));
                    ui.label(RichText::new(fp_text).weak().size(15.5));

                    ui.add_space(12.0);
                    ui.label(RichText::new("Core Rust Crates").strong());
                    for crate_line in [
                        "eframe / egui 0.31 - Desktop GUI framework",
                        "image 0.25 - JPEG, PNG, and GIF encoding",
                        "ab_glyph 0.2 - TrueType font rendering",
                        "rfd 0.15 - Native file and folder dialogs",
                        "serde / serde_json 1 - Configuration persistence",
                        "walkdir 2 - Recursive folder scanning",
                    ] {
                        ui.label(crate_line);
                    }

                    ui.add_space(12.0);
                    ui.label(RichText::new("Project Links and Credits").strong());
                    ui.hyperlink_to(
                        "https://github.com/aaron-c-s-ramos/ezthumb",
                        "https://github.com/aaron-c-s-ramos/ezthumb",
                    );
                    ui.hyperlink_to(
                        "Original Upstream: https://github.com/xuminic/ezthumb",
                        "https://github.com/xuminic/ezthumb",
                    );
                    ui.hyperlink_to(
                        "Inspired by Movie Thumbnailer (mtn): https://sourceforge.net/projects/moviethumbnail",
                        "https://sourceforge.net/projects/moviethumbnail",
                    );
                    ui.add_space(14.0);
                });
            });
    }
}

impl eframe::App for EzThumbApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() && !self.is_running() {
            self.add_paths(dropped);
            self.active_tab = ActiveTab::Queue;
        }

        let settings_dirty = self.draft.is_dirty(&self.config);

        egui::TopBottomPanel::top("header_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 40.0_f32),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.spacing_mut().interact_size.y = 36.0_f32;
                    ui.label(
                        RichText::new("EzThumb")
                            .strong()
                            .size(BASE_HEADER_FONT_SIZE * FONT_SCALE),
                    );
                    ui.add_space(16.0);
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::Queue, "Queue")
                        .clicked()
                    {
                        self.active_tab = ActiveTab::Queue;
                    }
                    let settings_label = if settings_dirty {
                        "Settings *"
                    } else {
                        "Settings"
                    };
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::Settings, settings_label)
                        .clicked()
                    {
                        self.active_tab = ActiveTab::Settings;
                    }
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::About, "About")
                        .clicked()
                    {
                        self.active_tab = ActiveTab::About;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let theme_label = match self.config.theme {
                            ThemeMode::Dark => "Light",
                            ThemeMode::Light => "Dark",
                        };
                        if ui
                            .add(
                                egui::Button::new(theme_label)
                                    .min_size(egui::vec2(78.0_f32, 34.0_f32)),
                            )
                            .clicked()
                        {
                            self.config.theme = match self.config.theme {
                                ThemeMode::Dark => ThemeMode::Light,
                                ThemeMode::Light => ThemeMode::Dark,
                            };
                            self.draft.config.theme = self.config.theme;
                            apply_style(ctx, self.config.theme);
                            let _ = self.config.save();
                        }
                    });
                },
            );
            ui.add_space(6.0);
        });

        match self.active_tab {
            ActiveTab::Queue => {
                egui::TopBottomPanel::bottom("queue_footer").show(ctx, |ui| {
                    self.render_queue_footer(ui);
                });
                egui::CentralPanel::default()
                    .frame(
                        Frame::central_panel(&ctx.style())
                            .inner_margin(egui::Margin::symmetric(10, 10)),
                    )
                    .show(ctx, |ui| {
                        self.render_queue_table(ui);
                    });
            }
            ActiveTab::Settings => {
                egui::TopBottomPanel::bottom("settings_footer").show(ctx, |ui| {
                    self.render_settings_footer(ui);
                });
                egui::CentralPanel::default()
                    .frame(
                        Frame::central_panel(&ctx.style())
                            .inner_margin(egui::Margin::symmetric(14, 10)),
                    )
                    .show(ctx, |ui| {
                        self.render_settings_tab(ui);
                    });
            }
            ActiveTab::About => {
                egui::CentralPanel::default()
                    .frame(
                        Frame::central_panel(&ctx.style())
                            .inner_margin(egui::Margin::symmetric(16, 10)),
                    )
                    .show(ctx, |ui| {
                        self.render_about_tab(ui);
                    });
            }
        }
    }
}

fn section_header(ui: &mut Ui, title: &str) {
    ui.label(RichText::new(title).strong());
    ui.add_space(4.0);
}

fn indented_form_row<R>(ui: &mut Ui, label: &str, add_controls: impl FnOnce(&mut Ui) -> R) {
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        let (label_rect, _) =
            ui.allocate_exact_size(egui::vec2(LABEL_COL_WIDTH, 28.0), Sense::hover());
        if !label.is_empty() {
            let font = TextStyle::Body.resolve(ui.style());
            let color = ui.visuals().text_color();
            ui.painter().with_clip_rect(label_rect).text(
                egui::pos2(label_rect.left(), label_rect.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                font,
                color,
            );
        }
        add_controls(ui);
    });
    ui.add_space(4.0);
}

fn color_hex_field(ui: &mut Ui, label: &str, hex: &mut String) {
    ui.label(label);
    let parsed = RgbColor::parse_hex(hex).ok();
    let swatch_color = parsed
        .map(|c| Color32::from_rgb(c.r, c.g, c.b))
        .unwrap_or(Color32::TRANSPARENT);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(2), swatch_color);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(2),
        Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
        egui::StrokeKind::Inside,
    );
    ui.add(egui::TextEdit::singleline(hex).desired_width(82.0));
}

fn grid_setting_label(mode: GridSetting) -> &'static str {
    match mode {
        GridSetting::Auto => "Auto",
        GridSetting::ColumnAndRow => "Column and Row",
        GridSetting::ColumnAndStep => "Column and Step (Time Step)",
        GridSetting::DiscreteByNumber => "Discrete by Number (Separate Frames)",
        GridSetting::DiscreteByStep => "Discrete by Step",
        GridSetting::DiscreteKeyFrames => "Discrete Key Frames",
    }
}

fn zoom_mode_label(mode: ZoomMode) -> &'static str {
    match mode {
        ZoomMode::Auto => "Auto",
        ZoomMode::ZoomByRatio => "Zoom by Ratio",
        ZoomMode::ZoomByCanvas => "Zoom by Canvas (Canvas Width)",
        ZoomMode::ZoomBySize => "Zoom by Size (Fixed Tile Size)",
    }
}

fn duration_scan_label(mode: DurationScanMode) -> &'static str {
    match mode {
        DurationScanMode::Auto => "Auto",
        DurationScanMode::FastScan => "Fast Scan",
        DurationScanMode::FullScan => "Full Scan (Deep Probe)",
        DurationScanMode::FileHead => "File Head",
    }
}

fn process_method_label(method: ProcessMethod) -> &'static str {
    match method {
        ProcessMethod::Auto => "Auto",
        ProcessMethod::UponKeyFrames => "Upon Key Frames (Fast)",
        ProcessMethod::FrameByFrame => "Frame by Frame (Accurate)",
        ProcessMethod::SinglePass => "Single Pass",
        ProcessMethod::TwoPass => "Two Pass",
        ProcessMethod::SafeMode => "Safe Mode",
        ProcessMethod::KeyFrameRip => "Key Frame Rip",
    }
}

fn font_source_label(source: FontSource) -> &'static str {
    match source {
        FontSource::SystemDefault => "System Default",
        FontSource::SystemFont => "Choose From System",
        FontSource::CustomFile => "Custom Font File",
    }
}

fn conflict_policy_label(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::CreateNew => "Create New Thumbnails",
        ConflictPolicy::Copy => "Keep Copy (.1, .2)",
        ConflictPolicy::Overwrite => "Overwrite Existing Thumbnails",
        ConflictPolicy::Skip => "Skip Existing Thumbnails",
    }
}

fn position_label(pos: TextPosition) -> &'static str {
    match pos {
        TextPosition::LeftTop => "Top Left",
        TextPosition::MiddleTop => "Top Center",
        TextPosition::RightTop => "Top Right",
        TextPosition::LeftBottom => "Bottom Left",
        TextPosition::MiddleBottom => "Bottom Center",
        TextPosition::RightBottom => "Bottom Right",
    }
}

fn status_text_color(kind: StatusKind, theme: ThemeMode) -> Color32 {
    match (kind, theme) {
        (StatusKind::Success, ThemeMode::Dark) => Color32::from_rgb(74, 222, 128),
        (StatusKind::Success, ThemeMode::Light) => Color32::from_rgb(21, 128, 61),
        (StatusKind::Error, ThemeMode::Dark) => Color32::from_rgb(248, 113, 113),
        (StatusKind::Error, ThemeMode::Light) => Color32::from_rgb(185, 28, 28),
        (StatusKind::Neutral, ThemeMode::Dark) => Color32::from_rgb(203, 213, 225),
        (StatusKind::Neutral, ThemeMode::Light) => Color32::from_rgb(51, 65, 85),
    }
}

fn open_file_in_os(path: &Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// Applies neutral dark or light visuals together with 1.5x scaled typography.
fn apply_style(ctx: &egui::Context, theme: ThemeMode) {
    let mut style = Style {
        text_styles: scaled_text_styles(),
        ..Default::default()
    };

    style.spacing.icon_width = 20.0_f32;
    style.spacing.icon_width_inner = 10.0_f32;
    style.spacing.button_padding = egui::vec2(10.0_f32, 6.0_f32);

    let mut visuals = match theme {
        ThemeMode::Dark => {
            let mut v = Visuals::dark();
            v.panel_fill = Color32::from_rgb(24, 26, 31);
            v.window_fill = Color32::from_rgb(24, 26, 31);
            v.widgets.inactive.bg_fill = Color32::from_rgb(38, 42, 50);
            v.widgets.hovered.bg_fill = Color32::from_rgb(50, 56, 67);
            v.widgets.active.bg_fill = Color32::from_rgb(37, 99, 235);
            v
        }
        ThemeMode::Light => {
            let mut v = Visuals::light();
            v.panel_fill = Color32::from_rgb(255, 255, 255);
            v.window_fill = Color32::from_rgb(255, 255, 255);
            v.widgets.inactive.bg_fill = Color32::from_rgb(236, 239, 244);
            v.widgets.hovered.bg_fill = Color32::from_rgb(220, 226, 234);
            v.widgets.active.bg_fill = Color32::from_rgb(37, 99, 235);
            v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(203, 213, 225));
            v
        }
    };

    let radius = CornerRadius::same(3);
    visuals.widgets.noninteractive.corner_radius = radius;
    visuals.widgets.inactive.corner_radius = radius;
    visuals.widgets.hovered.corner_radius = radius;
    visuals.widgets.active.corner_radius = radius;
    visuals.widgets.open.corner_radius = radius;

    style.visuals = visuals;
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GridSpec;

    #[test]
    fn scaled_text_styles_multiplies_all_defaults_by_font_scale() {
        let defaults = Style::default().text_styles;
        let scaled = scaled_text_styles();
        assert_eq!(defaults.len(), scaled.len());

        for (key, base_font) in defaults {
            let scaled_font = scaled.get(&key).expect("Scaled text style must exist.");
            let expected = base_font.size * FONT_SCALE;
            assert!(
                (scaled_font.size - expected).abs() < 0.001_f32,
                "Expected {expected} for {key:?}, got {}",
                scaled_font.size
            );
        }
    }

    #[test]
    fn formats_compact_duration_for_queue_column() {
        assert_eq!(format_duration_compact(51_000), "0:00:51");
        assert_eq!(format_duration_compact(3_665_000), "1:01:05");
    }

    #[test]
    fn formats_queue_footer_for_auto_and_custom_modes() {
        let def = EzConfig::default();
        assert_eq!(
            format_queue_footer(&def),
            "Grid: Auto  |  Zoom: Auto  |  Duration: Auto"
        );

        let mut cfg = EzConfig {
            grid_setting: GridSetting::ColumnAndRow,
            grid: GridSpec { cols: 3, rows: 3 },
            size: SizeSpec::ZoomPercent(50),
            duration_scan: DurationScanMode::FastScan,
            ..Default::default()
        };
        assert_eq!(
            format_queue_footer(&cfg),
            "Grid: 3x3  |  Zoom to 50%  |  Fast Scan"
        );

        cfg.grid_setting = GridSetting::DiscreteByStep;
        cfg.timestep_ms = Some(15_000);
        cfg.size = SizeSpec::TilePixels {
            width: 320,
            height: 180,
        };
        cfg.duration_scan = DurationScanMode::FullScan;
        assert_eq!(
            format_queue_footer(&cfg),
            "Discrete: Every 15s  |  Size 320x180  |  Full Scan"
        );

        cfg.grid_setting = GridSetting::DiscreteKeyFrames;
        cfg.duration_scan = DurationScanMode::FileHead;
        assert_eq!(
            format_queue_footer(&cfg),
            "Discrete: Key Frames  |  Size 320x180  |  File Head"
        );
    }

    #[test]
    fn draft_roundtrip_preserves_clean_state_and_tracks_changes() {
        let saved = EzConfig::default();
        let mut draft = SettingsDraft::from_config(&saved);
        assert!(!draft.is_dirty(&saved));

        draft.zoom_mode = ZoomMode::ZoomByRatio;
        draft.zoom_percent = 50;
        assert!(draft.is_dirty(&saved));

        let applied = draft.to_config().unwrap();
        assert_eq!(applied.size, SizeSpec::ZoomPercent(50));
    }
}
