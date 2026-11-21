use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use image::codecs::gif::{GifEncoder, Repeat};
use image::codecs::jpeg::JpegEncoder;
use image::{Delay, DynamicImage, Frame, ImageFormat, Rgba, RgbaImage};
use thiserror::Error;
use walkdir::WalkDir;

use crate::config::{ConflictPolicy, EzConfig, OutputFormat, TextPosition};
use crate::ffmpeg::{extract_frames, CapturedFrame, FfmpegError};
use crate::probe::{format_timestamp_hms, probe_video_with_mode, MediaInfo, ProbeError};

/// Represents an installed operating system TrueType or OpenType font.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFontEntry {
    pub display_name: String,
    pub path: PathBuf,
}

/// Scans standard OS font directories and returns installed TTF/OTF fonts sorted by name.
pub fn discover_system_fonts() -> Vec<SystemFontEntry> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        if let Some(windir) = std::env::var_os("WINDIR") {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        } else {
            dirs.push(PathBuf::from(r"C:\Windows\Fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Microsoft")
                    .join("Windows")
                    .join("Fonts"),
            );
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library").join("Fonts"));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
            dirs.push(PathBuf::from(home).join(".fonts"));
        }
    }

    let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(dir).max_depth(3).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase());
            if !matches!(ext.as_deref(), Some("ttf" | "otf")) {
                continue;
            }
            let name = read_opentype_display_name(&path).unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Font")
                    .to_string()
            });
            by_name.entry(name).or_insert(path);
        }
    }

    by_name
        .into_iter()
        .map(|(display_name, path)| SystemFontEntry { display_name, path })
        .collect()
}

/// Reads the Full Font Name (ID 4) or Family Name (ID 1) from an OpenType name table.
fn read_opentype_display_name(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    if data.len() < 12 {
        return None;
    }
    let num_tables = u16::from_be_bytes([data[4], data[5]]) as usize;
    let mut name_offset = None;
    let mut name_len = 0usize;
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > data.len() {
            break;
        }
        if &data[rec..rec + 4] == b"name" {
            name_offset = Some(u32::from_be_bytes([
                data[rec + 8],
                data[rec + 9],
                data[rec + 10],
                data[rec + 11],
            ]) as usize);
            name_len = u32::from_be_bytes([
                data[rec + 12],
                data[rec + 13],
                data[rec + 14],
                data[rec + 15],
            ]) as usize;
            break;
        }
    }

    let base = name_offset?;
    if base + 6 > data.len() || base + name_len > data.len() {
        return None;
    }
    let count = u16::from_be_bytes([data[base + 2], data[base + 3]]) as usize;
    let str_storage = base + u16::from_be_bytes([data[base + 4], data[base + 5]]) as usize;

    let mut full_name: Option<String> = None;
    let mut family_name: Option<String> = None;

    for i in 0..count {
        let rec = base + 6 + i * 12;
        if rec + 12 > data.len() {
            break;
        }
        let platform_id = u16::from_be_bytes([data[rec], data[rec + 1]]);
        let name_id = u16::from_be_bytes([data[rec + 6], data[rec + 7]]);
        if name_id != 4 && name_id != 1 {
            continue;
        }
        let len = u16::from_be_bytes([data[rec + 8], data[rec + 9]]) as usize;
        let off = u16::from_be_bytes([data[rec + 10], data[rec + 11]]) as usize;
        let start = str_storage + off;
        let end = start + len;
        if end > data.len() {
            continue;
        }
        let slice = &data[start..end];
        let decoded = if platform_id == 3 || platform_id == 0 {
            if !len.is_multiple_of(2) {
                continue;
            }
            let u16s: Vec<u16> = slice
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            String::from_utf16(&u16s).ok()
        } else if platform_id == 1 {
            Some(String::from_utf8_lossy(slice).to_string())
        } else {
            None
        };

        if let Some(text) = decoded {
            let clean = text.trim().to_string();
            if clean.is_empty() {
                continue;
            }
            if name_id == 4 && (full_name.is_none() || platform_id == 3) {
                full_name = Some(clean);
            } else if name_id == 1 && (family_name.is_none() || platform_id == 3) {
                family_name = Some(clean);
            }
        }
    }

    full_name.or(family_name)
}

/// Represents errors produced while composing or writing output images.
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Ffmpeg(#[from] FfmpegError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Image encoding error: {0}")]
    Image(#[from] image::ImageError),
    #[error("No video frames were captured.")]
    NoFrames,
}

/// Reports generated file paths and an optional preview image.
#[derive(Debug, Clone)]
pub struct RenderOutcome {
    pub output_paths: Vec<PathBuf>,
    pub preview_image: Option<RgbaImage>,
    pub skipped: bool,
}

/// Resolves the output path and applies Copy, Overwrite, or Skip conflict rules.
pub fn resolve_output_path(
    video_path: &Path,
    config: &EzConfig,
    individual_index: Option<usize>,
) -> Result<Option<PathBuf>, RenderError> {
    let parent = match &config.outdir {
        Some(dir) => {
            fs::create_dir_all(dir)?;
            dir.clone()
        }
        None => video_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };

    let stem = video_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let ext = config.format.extension();
    let base_name = match individual_index {
        Some(idx) => format!("{stem}{}_{idx:03}", config.suffix),
        None => format!("{stem}{}", config.suffix),
    };

    let primary = parent.join(format!("{base_name}.{ext}"));
    if !primary.exists() {
        return Ok(Some(primary));
    }

    match config.conflict_policy {
        ConflictPolicy::CreateNew => {
            for serial in 1..=255u16 {
                let candidate = parent.join(format!("{base_name}_{serial}.{ext}"));
                if !candidate.exists() {
                    return Ok(Some(candidate));
                }
            }
            Ok(Some(parent.join(format!("{base_name}_255.{ext}"))))
        }
        ConflictPolicy::Overwrite => Ok(Some(primary)),
        ConflictPolicy::Skip => Ok(None),
        ConflictPolicy::Copy => {
            for serial in 1..=255u16 {
                let candidate = parent.join(format!("{base_name}.{serial}.{ext}"));
                if !candidate.exists() {
                    return Ok(Some(candidate));
                }
            }
            Ok(Some(parent.join(format!("{base_name}.255.{ext}"))))
        }
    }
}

/// Runs the complete pipeline (probe, extract, compose, save) for one video file.
pub fn process_video<F>(
    path: &Path,
    config: &EzConfig,
    cancel: Option<&AtomicBool>,
    on_progress: F,
) -> Result<RenderOutcome, RenderError>
where
    F: FnMut(usize, usize),
{
    if !config.is_discrete_output()
        && config.conflict_policy == ConflictPolicy::Skip
        && resolve_output_path(path, config, None)?.is_none()
    {
        return Ok(RenderOutcome {
            output_paths: Vec::new(),
            preview_image: None,
            skipped: true,
        });
    }

    let info = probe_video_with_mode(path, config.video_stream_index, config.duration_scan)?;
    let frames = extract_frames(&info, config, cancel, on_progress)?;
    if frames.is_empty() {
        return Err(RenderError::NoFrames);
    }

    save_outputs(&info, &frames, config)
}

/// Composes and writes output image files according to the configured format and layout.
pub fn save_outputs(
    info: &MediaInfo,
    frames: &[CapturedFrame],
    config: &EzConfig,
) -> Result<RenderOutcome, RenderError> {
    if frames.is_empty() {
        return Err(RenderError::NoFrames);
    }

    let font = load_font(config.font_path.as_deref());

    if let OutputFormat::AnimatedGif { delay_ms } = config.format {
        let Some(out_path) = resolve_output_path(&info.path, config, None)? else {
            return Ok(RenderOutcome {
                output_paths: Vec::new(),
                preview_image: None,
                skipped: true,
            });
        };
        let stamped: Vec<RgbaImage> = frames
            .iter()
            .map(|f| {
                let mut img = f.image.clone();
                if config.show_timestamp {
                    draw_timestamp_on_tile(&mut img, f.timestamp_ms, config, font.as_ref());
                }
                if config.transparent_bg {
                    let pad = config.margin_px.max(2);
                    let mut padded = RgbaImage::from_pixel(
                        img.width() + pad * 2,
                        img.height() + pad * 2,
                        Rgba([0, 0, 0, 0]),
                    );
                    blit_image(&mut padded, &img, pad, pad);
                    padded
                } else {
                    img
                }
            })
            .collect();
        write_animated_gif(&out_path, &stamped, delay_ms, config.transparent_bg)?;
        return Ok(RenderOutcome {
            output_paths: vec![out_path],
            preview_image: stamped.into_iter().next(),
            skipped: false,
        });
    }

    if config.is_discrete_output() {
        let mut paths = Vec::new();
        let mut first_preview = None;
        for (i, frame) in frames.iter().enumerate() {
            let Some(out_path) = resolve_output_path(&info.path, config, Some(i + 1))? else {
                continue;
            };
            let mut img = frame.image.clone();
            if config.show_timestamp {
                draw_timestamp_on_tile(&mut img, frame.timestamp_ms, config, font.as_ref());
            }
            write_single_image(&out_path, &img, config.format)?;
            if first_preview.is_none() {
                first_preview = Some(img);
            }
            paths.push(out_path);
        }
        return Ok(RenderOutcome {
            skipped: paths.is_empty(),
            output_paths: paths,
            preview_image: first_preview,
        });
    }

    let Some(out_path) = resolve_output_path(&info.path, config, None)? else {
        return Ok(RenderOutcome {
            output_paths: Vec::new(),
            preview_image: None,
            skipped: true,
        });
    };
    let sheet = compose_contact_sheet_with_font(info, frames, config, font.as_ref());
    write_single_image(&out_path, &sheet, config.format)?;
    Ok(RenderOutcome {
        output_paths: vec![out_path],
        preview_image: Some(sheet),
        skipped: false,
    })
}

/// Composes a tiled contact sheet image with optional media info header and timestamps.
pub fn compose_contact_sheet(
    info: &MediaInfo,
    frames: &[CapturedFrame],
    config: &EzConfig,
) -> RgbaImage {
    let font = load_font(config.font_path.as_deref());
    compose_contact_sheet_with_font(info, frames, config, font.as_ref())
}

fn compose_contact_sheet_with_font(
    info: &MediaInfo,
    frames: &[CapturedFrame],
    config: &EzConfig,
    font: Option<&FontVec>,
) -> RgbaImage {
    let cols = config.resolved_grid(info.duration_ms).cols.max(1);
    let rows = (frames.len() as u32).div_ceil(cols).max(1);
    let tile_w = frames[0].image.width().max(1);
    let tile_h = frames[0].image.height().max(1);

    let gap = config.gap_px;
    let margin = config.margin_px;
    let info_lines = if config.show_info {
        info.header_lines(config.size_unit)
    } else {
        Vec::new()
    };

    let info_size = config.info_font_size.clamp(10, 48) as f32;
    let line_height = (info_size * 1.35).ceil() as u32;
    let header_height = if info_lines.is_empty() {
        0
    } else {
        (info_lines.len() as u32) * line_height + 10
    };

    let canvas_w = margin * 2 + cols * tile_w + cols.saturating_sub(1) * gap;
    let canvas_h = margin * 2 + header_height + rows * tile_h + rows.saturating_sub(1) * gap;

    let bg_alpha = if config.transparent_bg
        && matches!(config.format, OutputFormat::Png | OutputFormat::Gif)
    {
        0
    } else {
        255
    };
    let bg_px = Rgba([
        config.canvas_color.r,
        config.canvas_color.g,
        config.canvas_color.b,
        bg_alpha,
    ]);

    let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, bg_px);

    if !info_lines.is_empty() {
        let info_px = Rgba([
            config.info_color.r,
            config.info_color.g,
            config.info_color.b,
            255,
        ]);
        for (i, line) in info_lines.iter().enumerate() {
            let text_w = measure_text_width(line, info_size, font);
            let x = match config.info_position {
                TextPosition::MiddleTop | TextPosition::MiddleBottom => {
                    canvas_w.saturating_sub(text_w) / 2
                }
                TextPosition::RightTop | TextPosition::RightBottom => {
                    canvas_w.saturating_sub(margin + text_w)
                }
                TextPosition::LeftTop | TextPosition::LeftBottom => margin,
            };
            let y = margin + (i as u32) * line_height;
            draw_text(
                &mut canvas,
                line,
                x as i32,
                y as i32,
                info_size,
                info_px,
                font,
            );
        }
    }

    let edge_px_color = Rgba([60, 66, 78, 255]);
    for (idx, captured) in frames.iter().enumerate() {
        let col = (idx as u32) % cols;
        let row = (idx as u32) / cols;
        let dest_x = margin + col * (tile_w + gap);
        let dest_y = margin + header_height + row * (tile_h + gap);

        let mut tile = captured.image.clone();
        if config.edge_px > 0 {
            draw_border(&mut tile, config.edge_px, edge_px_color);
        }
        if config.show_timestamp {
            draw_timestamp_on_tile(&mut tile, captured.timestamp_ms, config, font);
        }

        blit_image(&mut canvas, &tile, dest_x, dest_y);
    }

    canvas
}

fn draw_timestamp_on_tile(
    tile: &mut RgbaImage,
    timestamp_ms: u64,
    config: &EzConfig,
    font: Option<&FontVec>,
) {
    let label = format_timestamp_hms(timestamp_ms);
    let size = config.timestamp_font_size.clamp(10, 36) as f32;
    let text_w = measure_text_width(&label, size, font);
    let text_h = size.ceil() as u32;
    let pad = 6u32;

    let x = match config.timestamp_position {
        TextPosition::LeftTop | TextPosition::LeftBottom => pad,
        TextPosition::MiddleTop | TextPosition::MiddleBottom => {
            tile.width().saturating_sub(text_w) / 2
        }
        TextPosition::RightTop | TextPosition::RightBottom => {
            tile.width().saturating_sub(text_w + pad)
        }
    };
    let y = match config.timestamp_position {
        TextPosition::LeftTop | TextPosition::MiddleTop | TextPosition::RightTop => pad,
        TextPosition::LeftBottom | TextPosition::MiddleBottom | TextPosition::RightBottom => {
            tile.height().saturating_sub(text_h + pad)
        }
    };

    // Draw a dark translucent badge behind the timestamp for legibility on bright frames.
    let bx0 = x.saturating_sub(4);
    let by0 = y.saturating_sub(2);
    let bx1 = (x + text_w + 4).min(tile.width());
    let by1 = (y + text_h + 3).min(tile.height());
    for py in by0..by1 {
        for px in bx0..bx1 {
            blend_pixel(tile, px as i32, py as i32, Rgba([0, 0, 0, 165]));
        }
    }

    let fg = Rgba([
        config.timestamp_color.r,
        config.timestamp_color.g,
        config.timestamp_color.b,
        255,
    ]);
    draw_text(tile, &label, x as i32, y as i32, size, fg, font);
}

fn write_single_image(
    out_path: &Path,
    img: &RgbaImage,
    format: OutputFormat,
) -> Result<(), RenderError> {
    match format {
        OutputFormat::Jpeg { quality } => {
            let rgb = DynamicImage::ImageRgba8(img.clone()).to_rgb8();
            let file = File::create(out_path)?;
            let mut enc = JpegEncoder::new_with_quality(BufWriter::new(file), quality);
            enc.encode_image(&rgb)?;
            Ok(())
        }
        OutputFormat::Png => {
            img.save_with_format(out_path, ImageFormat::Png)?;
            Ok(())
        }
        OutputFormat::Gif | OutputFormat::AnimatedGif { .. } => {
            img.save_with_format(out_path, ImageFormat::Gif)?;
            Ok(())
        }
    }
}

fn write_animated_gif(
    out_path: &Path,
    frames: &[RgbaImage],
    delay_ms: u32,
    transparent_bg: bool,
) -> Result<(), RenderError> {
    let file = File::create(out_path)?;
    let mut encoder = GifEncoder::new(BufWriter::new(file));
    encoder.set_repeat(Repeat::Infinite)?;
    let delay = Delay::from_numer_denom_ms(delay_ms.max(50), 1);
    for img in frames {
        let mut frame = Frame::from_parts(img.clone(), 0, 0, delay);
        if transparent_bg {
            // Preserve the alpha channel so the GIF encoder writes a transparency layer.
            encoder.encode_frame(frame)?;
        } else {
            // Clear partial transparency to opaque pixels when transparency is disabled.
            for px in frame.buffer_mut().pixels_mut() {
                px[3] = 255;
            }
            encoder.encode_frame(frame)?;
        }
    }
    Ok(())
}

fn blit_image(canvas: &mut RgbaImage, src: &RgbaImage, dx: u32, dy: u32) {
    let cw = canvas.width();
    let ch = canvas.height();
    for sy in 0..src.height() {
        let ty = dy + sy;
        if ty >= ch {
            break;
        }
        for sx in 0..src.width() {
            let tx = dx + sx;
            if tx >= cw {
                break;
            }
            canvas.put_pixel(tx, ty, *src.get_pixel(sx, sy));
        }
    }
}

fn draw_border(img: &mut RgbaImage, width: u32, color: Rgba<u8>) {
    let w = img.width();
    let h = img.height();
    for t in 0..width {
        for x in 0..w {
            if t < h {
                img.put_pixel(x, t, color);
                img.put_pixel(x, h - 1 - t, color);
            }
        }
        for y in 0..h {
            if t < w {
                img.put_pixel(t, y, color);
                img.put_pixel(w - 1 - t, y, color);
            }
        }
    }
}

fn load_font(custom: Option<&Path>) -> Option<FontVec> {
    if let Some(path) = custom {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(font) = FontVec::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }

    let candidates: &[&str] = &[
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\arial.ttf",
        r"C:\Windows\Fonts\consola.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = fs::read(path) {
            if let Ok(font) = FontVec::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }
    None
}

fn measure_text_width(text: &str, size: f32, font: Option<&FontVec>) -> u32 {
    if let Some(font) = font {
        let scaled = font.as_scaled(PxScale::from(size));
        let mut width = 0.0_f32;
        let mut prev = None;
        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            if let Some(p) = prev {
                width += scaled.kern(p, gid);
            }
            width += scaled.h_advance(gid);
            prev = Some(gid);
        }
        return width.ceil().max(1.0) as u32;
    }
    let scale = (size / 7.0).round().max(1.0) as u32;
    (text.chars().count() as u32) * (6 * scale)
}

fn draw_text(
    img: &mut RgbaImage,
    text: &str,
    x: i32,
    y: i32,
    size: f32,
    color: Rgba<u8>,
    font: Option<&FontVec>,
) {
    if let Some(font) = font {
        let scale = PxScale::from(size);
        let scaled = font.as_scaled(scale);
        let mut caret_x = x as f32;
        let baseline_y = (y as f32) + scaled.ascent();
        let mut prev = None;

        for ch in text.chars() {
            let gid = font.glyph_id(ch);
            if let Some(p) = prev {
                caret_x += scaled.kern(p, gid);
            }
            let glyph = gid.with_scale_and_position(scale, ab_glyph::point(caret_x, baseline_y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, cov| {
                    let alpha = (cov * f32::from(color[3])).round().clamp(0.0, 255.0) as u8;
                    if alpha > 0 {
                        let px = bounds.min.x.floor() as i32 + gx as i32;
                        let py = bounds.min.y.floor() as i32 + gy as i32;
                        blend_pixel(img, px, py, Rgba([color[0], color[1], color[2], alpha]));
                    }
                });
            }
            caret_x += scaled.h_advance(gid);
            prev = Some(gid);
        }
        return;
    }

    // Draw fallback 5x7 ASCII glyphs when no TrueType font file exists.
    let s = (size / 7.0).round().max(1.0) as i32;
    let mut cx = x;
    for ch in text.chars() {
        let bitmap = fallback_glyph_5x7(ch);
        for (row, bits) in bitmap.iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 == 1 {
                    for sy in 0..s {
                        for sx in 0..s {
                            blend_pixel(img, cx + col * s + sx, y + (row as i32) * s + sy, color);
                        }
                    }
                }
            }
        }
        cx += 6 * s;
    }
}

fn blend_pixel(img: &mut RgbaImage, x: i32, y: i32, src: Rgba<u8>) {
    if x < 0 || y < 0 {
        return;
    }
    let (ux, uy) = (x as u32, y as u32);
    if ux >= img.width() || uy >= img.height() {
        return;
    }
    let sa = f32::from(src[3]) / 255.0;
    if sa <= 0.0 {
        return;
    }
    let dst = *img.get_pixel(ux, uy);
    let da = f32::from(dst[3]) / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        return;
    }
    let blend = |sc: u8, dc: u8| -> u8 {
        ((f32::from(sc) * sa + f32::from(dc) * da * (1.0 - sa)) / out_a)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    img.put_pixel(
        ux,
        uy,
        Rgba([
            blend(src[0], dst[0]),
            blend(src[1], dst[1]),
            blend(src[2], dst[2]),
            (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
        ]),
    );
}

fn fallback_glyph_5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ':' => [0x00, 0x04, 0x04, 0x00, 0x04, 0x04, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        ' ' => [0x00; 7],
        _ => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GridSpec;

    fn sample_info() -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("sample.mp4"),
            file_name: "sample.mp4".to_string(),
            file_size_bytes: 2_097_152,
            duration_ms: 60_000,
            width: 320,
            height: 180,
            display_width: 320,
            display_height: 180,
            video_codec: "h264".to_string(),
            fps: 30.0,
            bit_rate_bps: 500_000,
            audio_summary: Some("aac, 48000 Hz, 2ch".to_string()),
            video_stream_count: 1,
            container_format: "mp4".to_string(),
        }
    }

    #[test]
    fn composes_contact_sheet_with_expected_canvas_dimensions() {
        let cfg = EzConfig {
            grid_setting: crate::config::GridSetting::ColumnAndRow,
            grid: GridSpec { cols: 2, rows: 2 },
            gap_px: 4,
            margin_px: 8,
            show_info: false,
            ..Default::default()
        };
        let frames: Vec<CapturedFrame> = (0..4)
            .map(|i| CapturedFrame {
                timestamp_ms: i * 10_000,
                image: RgbaImage::from_pixel(160, 90, Rgba([40, 80, 120, 255])),
            })
            .collect();
        let sheet = compose_contact_sheet(&sample_info(), &frames, &cfg);
        assert_eq!(sheet.width(), 8 * 2 + 2 * 160 + 4);
        assert_eq!(sheet.height(), 8 * 2 + 2 * 90 + 4);
    }

    #[test]
    fn resolves_create_new_copy_and_skip_conflict_policies() {
        let dir = std::env::temp_dir().join(format!("ezthumb_conflict_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let video = dir.join("clip.mp4");
        let existing = dir.join("clip_thumb.jpg");
        fs::write(&existing, b"old").unwrap();

        let new_cfg = EzConfig {
            outdir: Some(dir.clone()),
            conflict_policy: ConflictPolicy::CreateNew,
            ..Default::default()
        };
        let next_new = resolve_output_path(&video, &new_cfg, None)
            .unwrap()
            .unwrap();
        assert_eq!(next_new, dir.join("clip_thumb_1.jpg"));

        let copy_cfg = EzConfig {
            conflict_policy: ConflictPolicy::Copy,
            ..new_cfg.clone()
        };
        let next_copy = resolve_output_path(&video, &copy_cfg, None)
            .unwrap()
            .unwrap();
        assert_eq!(next_copy, dir.join("clip_thumb.1.jpg"));

        let skip_cfg = EzConfig {
            conflict_policy: ConflictPolicy::Skip,
            ..new_cfg
        };
        assert!(resolve_output_path(&video, &skip_cfg, None)
            .unwrap()
            .is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_transparent_animated_gif() {
        let dir = std::env::temp_dir().join(format!("ezthumb_anim_gif_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let out_path = dir.join("anim.gif");
        let mut frame = RgbaImage::from_pixel(24, 24, Rgba([0, 0, 0, 0]));
        for y in 4..20 {
            for x in 4..20 {
                frame.put_pixel(x, y, Rgba([220, 60, 40, 255]));
            }
        }
        write_animated_gif(&out_path, &[frame.clone(), frame], 400, true).unwrap();
        let bytes = fs::read(&out_path).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));
        assert!(bytes.len() > 64);

        let _ = fs::remove_dir_all(&dir);
    }
}
