use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;

use eframe::egui::{
    self, Color32, ColorImage, CornerRadius, FontId, RichText, Stroke, Style, TextStyle,
    TextureHandle, TextureOptions, Visuals,
};

use crate::cli::collect_video_files;
use crate::config::{
    ConflictPolicy, EzConfig, OutputFormat, RgbColor, SizeSpec, TextPosition, ThemeMode,
};
use crate::probe::find_binary;
use crate::render::process_video;

/// Global typography multiplier applied across all egui text styles.
pub const FONT_SCALE: f32 = 1.5_f32;

const BASE_HEADER_FONT_SIZE: f32 = 17.0_f32;

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

/// Launches the eframe/egui desktop configuration and batch-runner window.
pub fn run_gui(initial_files: Vec<PathBuf>, config: EzConfig) -> eframe::Result {
    let viewport = egui::ViewportBuilder::default()
        .with_title("EzThumb")
        .with_inner_size([860.0, 680.0])
        .with_min_inner_size([740.0, 600.0]);

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Neutral,
    Success,
    Error,
}

enum WorkerEvent {
    Progress {
        file_index: usize,
        total_files: usize,
        shot_done: usize,
        shot_total: usize,
        file_name: String,
    },
    FileComplete {
        message: String,
        preview: Option<image::RgbaImage>,
    },
    FileError {
        message: String,
    },
    Finished {
        succeeded: usize,
        skipped: usize,
        failed: usize,
        cancelled: bool,
    },
}

struct EzThumbApp {
    config: EzConfig,
    active_tab: ActiveTab,
    files: Vec<PathBuf>,
    selected_index: Option<usize>,
    ffmpeg_path: Option<PathBuf>,
    ffprobe_path: Option<PathBuf>,
    outdir_input: String,
    bg_hex_input: String,
    info_hex_input: String,
    time_hex_input: String,
    status_message: String,
    status_kind: StatusKind,
    log_lines: Vec<String>,
    worker_rx: Option<Receiver<WorkerEvent>>,
    cancel_flag: Option<Arc<AtomicBool>>,
    progress_fraction: f32,
    progress_label: String,
    preview_texture: Option<TextureHandle>,
    preview_size: [usize; 2],
}

impl EzThumbApp {
    fn new(initial_files: Vec<PathBuf>, config: EzConfig) -> Self {
        let ffmpeg_path = find_binary("ffmpeg");
        let ffprobe_path = find_binary("ffprobe");
        let files = collect_video_files(&initial_files, &config);
        let outdir_input = config
            .outdir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let bg_hex_input = config.canvas_color.to_hex();
        let info_hex_input = config.info_color.to_hex();
        let time_hex_input = config.timestamp_color.to_hex();

        let (status_message, status_kind) = match (&ffmpeg_path, &ffprobe_path) {
            (Some(_), Some(_)) => (
                "Ready. Add videos or drop files into the window.".to_string(),
                StatusKind::Neutral,
            ),
            _ => (
                "FFmpeg or FFprobe not found on PATH. Install FFmpeg before generating thumbnails."
                    .to_string(),
                StatusKind::Error,
            ),
        };

        Self {
            config,
            active_tab: ActiveTab::Queue,
            files,
            selected_index: None,
            ffmpeg_path,
            ffprobe_path,
            outdir_input,
            bg_hex_input,
            info_hex_input,
            time_hex_input,
            status_message,
            status_kind,
            log_lines: Vec::new(),
            worker_rx: None,
            cancel_flag: None,
            progress_fraction: 0.0,
            progress_label: String::new(),
            preview_texture: None,
            preview_size: [0, 0],
        }
    }

    fn is_running(&self) -> bool {
        self.worker_rx.is_some()
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        let resolved = collect_video_files(&paths, &self.config);
        let mut added = 0usize;
        for path in resolved {
            if !self.files.contains(&path) {
                self.files.push(path);
                added += 1;
            }
        }
        if added > 0 {
            self.status_message = format!("Added {added} video file(s).");
            self.status_kind = StatusKind::Neutral;
        }
    }

    fn sync_text_inputs_to_config(&mut self) {
        let trimmed_out = self.outdir_input.trim();
        self.config.outdir = if trimmed_out.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed_out))
        };
        if let Ok(c) = RgbColor::parse_hex(&self.bg_hex_input) {
            self.config.canvas_color = c;
        }
        if let Ok(c) = RgbColor::parse_hex(&self.info_hex_input) {
            self.config.info_color = c;
        }
        if let Ok(c) = RgbColor::parse_hex(&self.time_hex_input) {
            self.config.timestamp_color = c;
        }
    }

    fn start_batch(&mut self) {
        if self.files.is_empty() {
            self.status_message = "Add at least one video file first.".to_string();
            self.status_kind = StatusKind::Error;
            return;
        }
        if self.ffmpeg_path.is_none() || self.ffprobe_path.is_none() {
            self.status_message = "FFmpeg and FFprobe must be available on PATH.".to_string();
            self.status_kind = StatusKind::Error;
            return;
        }

        self.sync_text_inputs_to_config();
        let _ = self.config.save();

        let files = self.files.clone();
        let cfg = self.config.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();

        self.log_lines.clear();
        self.progress_fraction = 0.0;
        self.progress_label = format!("Starting batch ({} file(s))...", files.len());
        self.status_message = self.progress_label.clone();
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
                let fname_prog = file_name.clone();
                let res = process_video(path, &cfg, Some(&cancel_worker), |done, total| {
                    let _ = tx_prog.send(WorkerEvent::Progress {
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
                            let _ = tx.send(WorkerEvent::FileComplete {
                                message: format!("Skipped (exists): {file_name}"),
                                preview: None,
                            });
                        } else {
                            succeeded += 1;
                            let out_label = outcome
                                .output_paths
                                .first()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| file_name.clone());
                            let _ = tx.send(WorkerEvent::FileComplete {
                                message: format!("Saved: {out_label}"),
                                preview: outcome.preview_image,
                            });
                        }
                    }
                    Err(err) => {
                        if cancel_worker.load(Ordering::Relaxed) {
                            cancelled = true;
                            break;
                        }
                        failed += 1;
                        let _ = tx.send(WorkerEvent::FileError {
                            message: format!("Error ({file_name}): {err}"),
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

    fn poll_worker(&mut self, ctx: &egui::Context) {
        let mut finished = false;
        let mut pending_events = Vec::new();
        if let Some(rx) = &self.worker_rx {
            while let Ok(ev) = rx.try_recv() {
                pending_events.push(ev);
            }
        }

        for ev in pending_events {
            match ev {
                WorkerEvent::Progress {
                    file_index,
                    total_files,
                    shot_done,
                    shot_total,
                    file_name,
                } => {
                    let file_frac = if shot_total > 0 {
                        (shot_done as f32) / (shot_total as f32)
                    } else {
                        0.0
                    };
                    self.progress_fraction =
                        ((file_index as f32) + file_frac) / (total_files.max(1) as f32);
                    self.progress_label = format!(
                        "[{}/{}] {} ({}/{})",
                        file_index + 1,
                        total_files,
                        file_name,
                        shot_done,
                        shot_total
                    );
                }
                WorkerEvent::FileComplete { message, preview } => {
                    self.log_lines.push(message);
                    if let Some(img) = preview {
                        let size = [img.width() as usize, img.height() as usize];
                        let color_img = ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                        self.preview_size = size;
                        self.preview_texture = Some(ctx.load_texture(
                            "latest_contact_sheet",
                            color_img,
                            TextureOptions::LINEAR,
                        ));
                    }
                }
                WorkerEvent::FileError { message } => {
                    self.log_lines.push(message);
                }
                WorkerEvent::Finished {
                    succeeded,
                    skipped,
                    failed,
                    cancelled,
                } => {
                    finished = true;
                    self.progress_fraction = if cancelled {
                        self.progress_fraction
                    } else {
                        1.0
                    };
                    if cancelled {
                        self.status_message = format!(
                            "Cancelled. Saved {succeeded}, skipped {skipped}, failed {failed}."
                        );
                        self.status_kind = StatusKind::Error;
                    } else if failed > 0 {
                        self.status_message = format!(
                            "Completed with errors. Saved {succeeded}, skipped {skipped}, failed {failed}."
                        );
                        self.status_kind = StatusKind::Error;
                    } else {
                        self.status_message =
                            format!("Done. Saved {succeeded}, skipped {skipped}.");
                        self.status_kind = StatusKind::Success;
                    }
                    self.progress_label = self.status_message.clone();
                }
            }
        }

        if finished {
            self.worker_rx = None;
            self.cancel_flag = None;
        } else if self.is_running() {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
    }

    fn render_queue_tab(&mut self, ui: &mut egui::Ui) {
        let running = self.is_running();

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!running, egui::Button::new("Add Files"))
                .clicked()
            {
                if let Some(picked) = rfd::FileDialog::new()
                    .add_filter(
                        "Video files",
                        &[
                            "mp4", "mkv", "avi", "mov", "webm", "wmv", "flv", "ts", "mpg", "m4v",
                        ],
                    )
                    .pick_files()
                {
                    self.add_paths(picked);
                }
            }
            if ui
                .add_enabled(!running, egui::Button::new("Add Folder"))
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.add_paths(vec![folder]);
                }
            }
            if ui
                .add_enabled(
                    !running && self.selected_index.is_some(),
                    egui::Button::new("Remove Selected"),
                )
                .clicked()
            {
                if let Some(i) = self.selected_index.take() {
                    if i < self.files.len() {
                        self.files.remove(i);
                    }
                }
            }
            if ui
                .add_enabled(
                    !running && !self.files.is_empty(),
                    egui::Button::new("Clear"),
                )
                .clicked()
            {
                self.files.clear();
                self.selected_index = None;
            }
        });

        ui.add_space(6.0);

        ui.columns(2, |cols| {
            cols[0].label(RichText::new(format!("Video Queue ({})", self.files.len())).strong());
            egui::ScrollArea::vertical()
                .id_salt("queue_scroll")
                .max_height(260.0)
                .show(&mut cols[0], |ui| {
                    if self.files.is_empty() {
                        ui.label(RichText::new("Drop video files here or click Add Files.").weak());
                    } else {
                        for (idx, path) in self.files.iter().enumerate() {
                            let label =
                                path.file_name().and_then(|n| n.to_str()).unwrap_or("video");
                            let selected = self.selected_index == Some(idx);
                            if ui.selectable_label(selected, label).clicked() {
                                self.selected_index = Some(idx);
                            }
                        }
                    }
                });

            cols[1].label(RichText::new("Latest Preview").strong());
            if let Some(tex) = &self.preview_texture {
                let avail_w = cols[1].available_width().max(120.0);
                let aspect =
                    (self.preview_size[1].max(1) as f32) / (self.preview_size[0].max(1) as f32);
                let draw_h = (avail_w * aspect).min(260.0);
                let draw_w = draw_h / aspect;
                cols[1].image((tex.id(), egui::vec2(draw_w, draw_h)));
            } else {
                cols[1].label(
                    RichText::new("Generated thumbnail sheet appears here after a run.").weak(),
                );
            }
        });

        ui.separator();

        ui.horizontal(|ui| {
            let grid_text = if self.config.grid.is_individual() {
                format!("Mode: {} separate frames", self.config.grid.rows)
            } else {
                format!(
                    "Grid: {}x{} | Format: {}",
                    self.config.grid.cols,
                    self.config.grid.rows,
                    self.config.format.extension().to_ascii_uppercase()
                )
            };
            ui.label(grid_text);
        });

        let bin_text = match (&self.ffmpeg_path, &self.ffprobe_path) {
            (Some(ff), Some(_)) => format!("FFmpeg: {}", ff.display()),
            _ => "FFmpeg: not found on PATH".to_string(),
        };
        ui.label(RichText::new(bin_text).weak().small());

        ui.add_space(6.0);

        ui.columns(2, |cols| {
            if cols[0]
                .add_enabled(
                    !running && !self.files.is_empty(),
                    egui::Button::new("Generate Thumbnails")
                        .min_size(egui::vec2(cols[0].available_width(), 44.0)),
                )
                .clicked()
            {
                self.start_batch();
            }
            if cols[1]
                .add_enabled(
                    running,
                    egui::Button::new("Cancel")
                        .min_size(egui::vec2(cols[1].available_width(), 44.0)),
                )
                .clicked()
            {
                if let Some(flag) = &self.cancel_flag {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        });

        ui.add_space(6.0);

        if running || self.progress_fraction > 0.0 {
            ui.add(
                egui::ProgressBar::new(self.progress_fraction)
                    .text(self.progress_label.clone())
                    .desired_width(ui.available_width()),
            );
        }

        let status_color = match (self.status_kind, self.config.theme) {
            (StatusKind::Success, ThemeMode::Dark) => Color32::from_rgb(74, 222, 128),
            (StatusKind::Success, ThemeMode::Light) => Color32::from_rgb(21, 128, 61),
            (StatusKind::Error, ThemeMode::Dark) => Color32::from_rgb(248, 113, 113),
            (StatusKind::Error, ThemeMode::Light) => Color32::from_rgb(185, 28, 28),
            (StatusKind::Neutral, ThemeMode::Dark) => Color32::from_rgb(203, 213, 225),
            (StatusKind::Neutral, ThemeMode::Light) => Color32::from_rgb(51, 65, 85),
        };
        ui.label(RichText::new(&self.status_message).color(status_color));

        if !self.log_lines.is_empty() {
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("log_scroll")
                .max_height(88.0)
                .show(ui, |ui| {
                    for line in &self.log_lines {
                        ui.label(RichText::new(line).small());
                    }
                });
        }
    }

    fn render_settings_tab(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;

                ui.horizontal(|ui| {
                    ui.label("Columns (0 = separate):");
                    ui.add(egui::DragValue::new(&mut self.config.grid.cols).range(0..=16));
                    ui.add_space(12.0);
                    ui.label("Rows / Count:");
                    ui.add(egui::DragValue::new(&mut self.config.grid.rows).range(1..=100));
                });

                ui.horizontal(|ui| {
                    ui.label("Canvas width (px):");
                    let mut width_px = match self.config.size {
                        SizeSpec::CanvasWidth(w) => w,
                        SizeSpec::TilePixels { width, .. } => width * self.config.grid.cols.max(1),
                        SizeSpec::ZoomPercent(_) => 1280,
                    };
                    if ui
                        .add(egui::DragValue::new(&mut width_px).range(320..=8192))
                        .changed()
                    {
                        self.config.size = SizeSpec::CanvasWidth(width_px);
                    }
                    ui.add_space(12.0);
                    ui.label("Gap:");
                    ui.add(egui::DragValue::new(&mut self.config.gap_px).range(0..=48));
                    ui.label("Margin:");
                    ui.add(egui::DragValue::new(&mut self.config.margin_px).range(0..=64));
                    ui.label("Edge:");
                    ui.add(egui::DragValue::new(&mut self.config.edge_px).range(0..=16));
                });

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Output format:");
                    let mut fmt_idx = match self.config.format {
                        OutputFormat::Jpeg { .. } => 0,
                        OutputFormat::Png => 1,
                        OutputFormat::Gif => 2,
                        OutputFormat::AnimatedGif { .. } => 3,
                    };
                    egui::ComboBox::from_id_salt("output_format_box")
                        .selected_text(match fmt_idx {
                            0 => "JPEG",
                            1 => "PNG",
                            2 => "GIF",
                            _ => "Animated GIF",
                        })
                        .show_ui(ui, |ui| {
                            if ui.selectable_value(&mut fmt_idx, 0, "JPEG").changed() {
                                self.config.format = OutputFormat::Jpeg { quality: 85 };
                            }
                            if ui.selectable_value(&mut fmt_idx, 1, "PNG").changed() {
                                self.config.format = OutputFormat::Png;
                            }
                            if ui.selectable_value(&mut fmt_idx, 2, "GIF").changed() {
                                self.config.format = OutputFormat::Gif;
                            }
                            if ui
                                .selectable_value(&mut fmt_idx, 3, "Animated GIF")
                                .changed()
                            {
                                self.config.format = OutputFormat::AnimatedGif { delay_ms: 800 };
                            }
                        });

                    if let OutputFormat::Jpeg { ref mut quality } = self.config.format {
                        ui.label("Quality:");
                        ui.add(egui::DragValue::new(quality).range(1..=100));
                    } else if let OutputFormat::AnimatedGif { ref mut delay_ms } =
                        self.config.format
                    {
                        ui.label("Frame delay (ms):");
                        ui.add(egui::DragValue::new(delay_ms).range(50..=5000));
                    }
                });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.show_info, "Show media info header");
                    ui.checkbox(&mut self.config.show_timestamp, "Show frame timestamps");
                    ui.checkbox(&mut self.config.accurate_seek, "Accurate seek");
                });

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.include_first_frame, "Include first frame");
                    ui.checkbox(&mut self.config.include_last_frame, "Include last frame");
                    ui.checkbox(&mut self.config.transparent_bg, "Transparent PNG/GIF");
                    ui.checkbox(&mut self.config.recursive, "Recursive folder scan");
                });

                ui.horizontal(|ui| {
                    ui.label("Timestamp anchor:");
                    egui::ComboBox::from_id_salt("ts_pos_box")
                        .selected_text(position_label(self.config.timestamp_position))
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
                                    &mut self.config.timestamp_position,
                                    pos,
                                    position_label(pos),
                                );
                            }
                        });

                    ui.add_space(12.0);
                    ui.label("Existing file:");
                    egui::ComboBox::from_id_salt("conflict_box")
                        .selected_text(match self.config.conflict_policy {
                            ConflictPolicy::Copy => "Keep copy (.1, .2)",
                            ConflictPolicy::Overwrite => "Overwrite",
                            ConflictPolicy::Skip => "Skip",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.config.conflict_policy,
                                ConflictPolicy::Copy,
                                "Keep copy (.1, .2)",
                            );
                            ui.selectable_value(
                                &mut self.config.conflict_policy,
                                ConflictPolicy::Overwrite,
                                "Overwrite",
                            );
                            ui.selectable_value(
                                &mut self.config.conflict_policy,
                                ConflictPolicy::Skip,
                                "Skip",
                            );
                        });
                });

                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("BG Hex:");
                    ui.add(egui::TextEdit::singleline(&mut self.bg_hex_input).desired_width(90.0));
                    ui.label("Info Hex:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.info_hex_input).desired_width(90.0),
                    );
                    ui.label("Time Hex:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.time_hex_input).desired_width(90.0),
                    );
                });

                ui.horizontal(|ui| {
                    ui.label("Filename suffix:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.suffix).desired_width(140.0),
                    );
                    ui.label("Output folder (blank = source):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.outdir_input)
                            .desired_width(ui.available_width() - 95.0),
                    );
                    if ui.button("Browse").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.outdir_input = dir.to_string_lossy().to_string();
                        }
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save Settings").clicked() {
                        self.sync_text_inputs_to_config();
                        match self.config.save() {
                            Ok(()) => {
                                self.status_message = "Settings saved.".to_string();
                                self.status_kind = StatusKind::Success;
                            }
                            Err(err) => {
                                self.status_message = format!("Save failed: {err}");
                                self.status_kind = StatusKind::Error;
                            }
                        }
                    }
                    if ui.button("Reset Defaults").clicked() {
                        let current_theme = self.config.theme;
                        self.config = EzConfig {
                            theme: current_theme,
                            ..Default::default()
                        };
                        self.outdir_input.clear();
                        self.bg_hex_input = self.config.canvas_color.to_hex();
                        self.info_hex_input = self.config.info_color.to_hex();
                        self.time_hex_input = self.config.timestamp_color.to_hex();
                        let _ = self.config.save();
                        self.status_message = "Defaults restored.".to_string();
                        self.status_kind = StatusKind::Neutral;
                    }
                });
            });
    }
}

impl eframe::App for EzThumbApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker(ctx);

        // Accept video files dropped onto the application window.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() && !self.is_running() {
            self.add_paths(dropped);
        }

        egui::TopBottomPanel::top("header_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 40.0_f32),
                egui::Layout::left_to_right(egui::Align::Center),
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
                    if ui
                        .selectable_label(self.active_tab == ActiveTab::Settings, "Settings")
                        .clicked()
                    {
                        self.active_tab = ActiveTab::Settings;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let theme_label = match self.config.theme {
                            ThemeMode::Dark => "Light",
                            ThemeMode::Light => "Dark",
                        };
                        if ui
                            .add(
                                egui::Button::new(theme_label)
                                    .min_size(egui::vec2(76.0_f32, 36.0_f32)),
                            )
                            .clicked()
                        {
                            self.config.theme = match self.config.theme {
                                ThemeMode::Dark => ThemeMode::Light,
                                ThemeMode::Light => ThemeMode::Dark,
                            };
                            apply_style(ctx, self.config.theme);
                            let _ = self.config.save();
                        }
                    });
                },
            );
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            match self.active_tab {
                ActiveTab::Queue => self.render_queue_tab(ui),
                ActiveTab::Settings => self.render_settings_tab(ui),
            }
        });
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
}
