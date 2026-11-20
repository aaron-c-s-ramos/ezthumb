use std::io::Cursor;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use image::RgbaImage;
use thiserror::Error;

use crate::config::{EzConfig, SizeSpec};
use crate::probe::{find_binary, MediaInfo};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Represents errors produced during frame extraction with external FFmpeg.
#[derive(Debug, Error)]
pub enum FfmpegError {
    #[error("ffmpeg binary not found on PATH. Install FFmpeg to use ezthumb.")]
    FfmpegNotFound,
    #[error("Failed to execute ffmpeg: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffmpeg failed at {timestamp_ms} ms: {stderr}")]
    Failed { timestamp_ms: u64, stderr: String },
    #[error("Failed to decode extracted PNG frame at {timestamp_ms} ms: {source}")]
    ImageDecode {
        timestamp_ms: u64,
        source: image::ImageError,
    },
    #[error("Operation cancelled by user.")]
    Cancelled,
}

/// Holds one decoded video frame and its target timestamp in milliseconds.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub timestamp_ms: u64,
    pub image: RgbaImage,
}

/// Computes target tile width and height in pixels while preserving aspect ratio.
pub fn compute_tile_dimensions(info: &MediaInfo, config: &EzConfig) -> (u32, u32) {
    let src_w = info.display_width.max(1) as f64;
    let src_h = info.display_height.max(1) as f64;
    let aspect = src_h / src_w;

    match config.size {
        SizeSpec::TilePixels { width, height } => (width.max(32), height.max(24)),
        SizeSpec::ZoomPercent(pct) => {
            let scale = (pct as f64) / 100.0;
            let w = (src_w * scale).round().clamp(32.0, 4096.0) as u32;
            let h = (src_h * scale).round().clamp(24.0, 4096.0) as u32;
            (w, h)
        }
        SizeSpec::CanvasWidth(canvas_w) => {
            let cols = if config.grid.is_individual() {
                1
            } else {
                config.grid.cols.max(1)
            };
            let total_margins = config.margin_px.saturating_mul(2);
            let total_gaps = config.gap_px.saturating_mul(cols.saturating_sub(1));
            let usable = canvas_w.saturating_sub(total_margins + total_gaps).max(64);
            let tile_w = (usable / cols).max(32);
            let tile_h = ((tile_w as f64) * aspect).round().max(24.0) as u32;
            (tile_w, tile_h)
        }
    }
}

/// Computes all capture timestamps in milliseconds for the configured grid or timestep.
pub fn compute_shot_timestamps(duration_ms: u64, config: &EzConfig) -> Vec<u64> {
    if duration_ms == 0 {
        return vec![0];
    }

    let start_ms = config
        .time_from
        .map(|t| t.resolve_ms(duration_ms))
        .unwrap_or(0);
    let end_ms = config
        .time_end
        .map(|t| t.resolve_ms(duration_ms))
        .unwrap_or(duration_ms)
        .max(start_ms);

    let span_ms = end_ms.saturating_sub(start_ms);
    if span_ms == 0 {
        return vec![start_ms];
    }

    if let Some(step_ms) = config.timestep_ms.filter(|s| *s > 0) {
        let mut points = Vec::new();
        let mut t = if config.include_first_frame {
            start_ms
        } else {
            start_ms.saturating_add(step_ms / 2)
        };
        let cap = config.grid.shot_count().max(1) as usize;
        while t <= end_ms && points.len() < cap {
            points.push(t.min(duration_ms.saturating_sub(1)));
            t = t.saturating_add(step_ms);
        }
        if points.is_empty() {
            points.push(start_ms);
        }
        return points;
    }

    let count = config.grid.shot_count().max(1) as u64;
    if count == 1 {
        let t = if config.include_first_frame {
            start_ms
        } else if config.include_last_frame {
            end_ms.saturating_sub(100)
        } else {
            start_ms + span_ms / 2
        };
        return vec![t];
    }

    let safe_end = end_ms.saturating_sub(100).max(start_ms);
    let safe_span = safe_end.saturating_sub(start_ms);

    let mut points = Vec::with_capacity(count as usize);
    for i in 0..count {
        let ts = match (config.include_first_frame, config.include_last_frame) {
            (true, true) => start_ms + (safe_span.saturating_mul(i)) / (count - 1),
            (true, false) => start_ms + (safe_span.saturating_mul(i)) / count,
            (false, true) => start_ms + (safe_span.saturating_mul(i + 1)) / count,
            (false, false) => start_ms + (safe_span.saturating_mul(i + 1)) / (count + 1),
        };
        points.push(ts.min(safe_end));
    }
    points
}

/// Builds ffmpeg CLI arguments to extract one scaled PNG frame to stdout.
pub fn build_ffmpeg_frame_args(
    video_path: &str,
    timestamp_ms: u64,
    tile_w: u32,
    tile_h: u32,
    accurate_seek: bool,
    video_stream_index: Option<usize>,
) -> Vec<String> {
    let seek_secs = format!("{:.3}", (timestamp_ms as f64) / 1000.0);
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];

    if !accurate_seek {
        args.push("-ss".to_string());
        args.push(seek_secs.clone());
    }

    args.push("-i".to_string());
    args.push(video_path.to_string());

    if accurate_seek {
        args.push("-ss".to_string());
        args.push(seek_secs);
    }

    if let Some(idx) = video_stream_index {
        args.push("-map".to_string());
        args.push(format!("0:v:{idx}"));
    }

    args.extend([
        "-frames:v".to_string(),
        "1".to_string(),
        "-vf".to_string(),
        format!("scale={tile_w}:{tile_h}:flags=lanczos"),
        "-f".to_string(),
        "image2pipe".to_string(),
        "-vcodec".to_string(),
        "png".to_string(),
        "-".to_string(),
    ]);
    args
}

/// Extracts all configured frames from a video file using external FFmpeg.
pub fn extract_frames<F>(
    info: &MediaInfo,
    config: &EzConfig,
    cancel: Option<&AtomicBool>,
    mut on_progress: F,
) -> Result<Vec<CapturedFrame>, FfmpegError>
where
    F: FnMut(usize, usize),
{
    let ffmpeg = find_binary("ffmpeg").ok_or(FfmpegError::FfmpegNotFound)?;
    let (tile_w, tile_h) = compute_tile_dimensions(info, config);
    let timestamps = compute_shot_timestamps(info.duration_ms, config);
    let total = timestamps.len();
    let video_str = info.path.to_string_lossy().to_string();

    let mut frames = Vec::with_capacity(total);
    for (idx, ts_ms) in timestamps.into_iter().enumerate() {
        if let Some(flag) = cancel {
            if flag.load(Ordering::Relaxed) {
                return Err(FfmpegError::Cancelled);
            }
        }

        let args = build_ffmpeg_frame_args(
            &video_str,
            ts_ms,
            tile_w,
            tile_h,
            config.accurate_seek,
            config.video_stream_index,
        );
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let output = cmd.output()?;
        if !output.status.success() || output.stdout.is_empty() {
            // Retry with a slight fallback offset when seeking near the end of a clip.
            if ts_ms > 500 {
                let retry_ts = ts_ms.saturating_sub(500);
                let retry_args = build_ffmpeg_frame_args(
                    &video_str,
                    retry_ts,
                    tile_w,
                    tile_h,
                    true,
                    config.video_stream_index,
                );
                let mut retry_cmd = Command::new(&ffmpeg);
                retry_cmd
                    .args(&retry_args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                #[cfg(windows)]
                retry_cmd.creation_flags(CREATE_NO_WINDOW);

                let retry_out = retry_cmd.output()?;
                if retry_out.status.success() && !retry_out.stdout.is_empty() {
                    let img = image::load(Cursor::new(&retry_out.stdout), image::ImageFormat::Png)
                        .map_err(|source| FfmpegError::ImageDecode {
                            timestamp_ms: retry_ts,
                            source,
                        })?
                        .to_rgba8();
                    frames.push(CapturedFrame {
                        timestamp_ms: ts_ms,
                        image: img,
                    });
                    on_progress(idx + 1, total);
                    continue;
                }
            }

            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(FfmpegError::Failed {
                timestamp_ms: ts_ms,
                stderr: if stderr.is_empty() {
                    "No frame output received from ffmpeg.".to_string()
                } else {
                    stderr
                },
            });
        }

        let img = image::load(Cursor::new(&output.stdout), image::ImageFormat::Png)
            .map_err(|source| FfmpegError::ImageDecode {
                timestamp_ms: ts_ms,
                source,
            })?
            .to_rgba8();
        frames.push(CapturedFrame {
            timestamp_ms: ts_ms,
            image: img,
        });
        on_progress(idx + 1, total);
    }

    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GridSpec;

    #[test]
    fn computes_evenly_spaced_timestamps_without_black_endpoints() {
        let cfg = EzConfig {
            grid: GridSpec { cols: 2, rows: 2 },
            include_first_frame: false,
            include_last_frame: false,
            ..Default::default()
        };
        let pts = compute_shot_timestamps(10_100, &cfg);
        assert_eq!(pts, vec![2000, 4000, 6000, 8000]);
    }

    #[test]
    fn includes_first_and_last_frames_when_enabled() {
        let cfg = EzConfig {
            grid: GridSpec { cols: 3, rows: 1 },
            include_first_frame: true,
            include_last_frame: true,
            ..Default::default()
        };
        let pts = compute_shot_timestamps(10_100, &cfg);
        assert_eq!(pts, vec![0, 5000, 10_000]);
    }

    #[test]
    fn builds_fast_and_accurate_ffmpeg_seek_positions() {
        let fast = build_ffmpeg_frame_args("demo.mp4", 12_500, 320, 180, false, None);
        let ss_pos = fast.iter().position(|a| a == "-ss").unwrap();
        let i_pos = fast.iter().position(|a| a == "-i").unwrap();
        assert!(ss_pos < i_pos);

        let accurate = build_ffmpeg_frame_args("demo.mp4", 12_500, 320, 180, true, Some(1));
        let ss_pos = accurate.iter().position(|a| a == "-ss").unwrap();
        let i_pos = accurate.iter().position(|a| a == "-i").unwrap();
        assert!(ss_pos > i_pos);
        assert!(accurate.contains(&"0:v:1".to_string()));
    }
}
