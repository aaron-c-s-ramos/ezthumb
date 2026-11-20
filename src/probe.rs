use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use thiserror::Error;

use crate::config::SizeUnit;

/// Represents errors produced while locating or invoking FFprobe.
#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("ffprobe binary not found on PATH. Install FFmpeg to use ezthumb.")]
    FfprobeNotFound,
    #[error("Failed to execute ffprobe: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("ffprobe failed for '{path}': {stderr}")]
    Failed { path: String, stderr: String },
    #[error("Failed to parse ffprobe JSON output: {0}")]
    Json(#[from] serde_json::Error),
    #[error("No video stream found in '{0}'.")]
    NoVideoStream(String),
}

/// Holds parsed metadata for a single video file.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub file_name: String,
    pub file_size_bytes: u64,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub display_width: u32,
    pub display_height: u32,
    pub video_codec: String,
    pub fps: f64,
    pub bit_rate_bps: u64,
    pub audio_summary: Option<String>,
    pub video_stream_count: usize,
    pub container_format: String,
}

impl MediaInfo {
    /// Formats duration as HH:MM:SS.
    pub fn duration_hms(&self) -> String {
        format_timestamp_hms(self.duration_ms)
    }

    /// Formats the one-line summary printed by CLI -i mode.
    pub fn short_line(&self, unit: SizeUnit) -> String {
        format!(
            "{} | {} | {}x{} | {} | {}",
            self.file_name,
            self.duration_hms(),
            self.display_width,
            self.display_height,
            self.video_codec,
            unit.format_bytes(self.file_size_bytes)
        )
    }

    /// Builds multi-line header text for the thumbnail canvas and CLI -I mode.
    pub fn header_lines(&self, unit: SizeUnit) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!("File: {}", self.file_name));

        let bitrate_part = if self.bit_rate_bps > 0 {
            format!(", {} kbps", self.bit_rate_bps / 1000)
        } else {
            String::new()
        };
        lines.push(format!(
            "Size: {} | Duration: {}{}",
            unit.format_bytes(self.file_size_bytes),
            self.duration_hms(),
            bitrate_part
        ));

        let fps_part = if self.fps > 0.01 {
            format!(", {:.2} fps", self.fps)
        } else {
            String::new()
        };
        let mut stream_line = format!(
            "Video: {} {}x{}{}",
            self.video_codec, self.display_width, self.display_height, fps_part
        );
        if let Some(audio) = &self.audio_summary {
            stream_line.push_str(" | Audio: ");
            stream_line.push_str(audio);
        }
        lines.push(stream_line);
        lines
    }
}

/// Formats milliseconds as HH:MM:SS.
pub fn format_timestamp_hms(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

/// Locates an executable binary beside the current executable or on PATH.
pub fn find_binary(name: &str) -> Option<PathBuf> {
    let candidate_names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for bin in &candidate_names {
                let p = dir.join(bin);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for bin in &candidate_names {
                let p = dir.join(bin);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    None
}

/// Probes a video file using external ffprobe and returns structured metadata.
pub fn probe_video(
    path: &Path,
    video_stream_index: Option<usize>,
) -> Result<MediaInfo, ProbeError> {
    let ffprobe = find_binary("ffprobe").ok_or(ProbeError::FfprobeNotFound)?;
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ProbeError::Failed {
            path: path.display().to_string(),
            stderr,
        });
    }

    let raw_json = String::from_utf8_lossy(&output.stdout);
    let file_size_fs = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    parse_ffprobe_json(path, &raw_json, file_size_fs, video_stream_index)
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_aspect_ratio: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    sample_rate: Option<String>,
}

/// Parses raw ffprobe JSON into a MediaInfo structure.
pub fn parse_ffprobe_json(
    path: &Path,
    raw_json: &str,
    fallback_file_size: u64,
    video_stream_index: Option<usize>,
) -> Result<MediaInfo, ProbeError> {
    let parsed: FfprobeOutput = serde_json::from_str(raw_json)?;

    let video_streams: Vec<&FfprobeStream> = parsed
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("video"))
        .collect();

    if video_streams.is_empty() {
        return Err(ProbeError::NoVideoStream(path.display().to_string()));
    }

    let chosen_idx = video_stream_index
        .unwrap_or(0)
        .min(video_streams.len().saturating_sub(1));
    let vstream = video_streams[chosen_idx];

    let width = vstream.width.unwrap_or(640).max(1);
    let height = vstream.height.unwrap_or(360).max(1);
    let (display_width, display_height) =
        apply_sample_aspect_ratio(width, height, vstream.sample_aspect_ratio.as_deref());

    let duration_secs = parsed
        .format
        .as_ref()
        .and_then(|f| f.duration.as_deref())
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            vstream
                .duration
                .as_deref()
                .and_then(|d| d.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        .max(0.0);
    let duration_ms = (duration_secs * 1000.0).round() as u64;

    let file_size_bytes = parsed
        .format
        .as_ref()
        .and_then(|f| f.size.as_deref())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(fallback_file_size);

    let bit_rate_bps = parsed
        .format
        .as_ref()
        .and_then(|f| f.bit_rate.as_deref())
        .and_then(|b| b.parse::<u64>().ok())
        .or_else(|| {
            vstream
                .bit_rate
                .as_deref()
                .and_then(|b| b.parse::<u64>().ok())
        })
        .unwrap_or(0);

    let fps = parse_fraction(vstream.avg_frame_rate.as_deref())
        .filter(|f| *f > 0.01 && *f < 1000.0)
        .or_else(|| {
            parse_fraction(vstream.r_frame_rate.as_deref()).filter(|f| *f > 0.01 && *f < 1000.0)
        })
        .unwrap_or(0.0);

    let audio_summary = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"))
        .map(|a| {
            let codec = a.codec_name.as_deref().unwrap_or("audio");
            let sr = a
                .sample_rate
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .map(|hz| format!("{} Hz", hz))
                .unwrap_or_default();
            let ch = a.channels.map(|c| format!("{c}ch")).unwrap_or_default();
            let parts: Vec<&str> = [codec, sr.as_str(), ch.as_str()]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
            parts.join(", ")
        });

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video")
        .to_string();

    Ok(MediaInfo {
        path: path.to_path_buf(),
        file_name,
        file_size_bytes,
        duration_ms,
        width,
        height,
        display_width,
        display_height,
        video_codec: vstream
            .codec_name
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        fps,
        bit_rate_bps,
        audio_summary,
        video_stream_count: video_streams.len(),
        container_format: parsed
            .format
            .and_then(|f| f.format_name)
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

fn parse_fraction(raw: Option<&str>) -> Option<f64> {
    let text = raw?.trim();
    if let Some((num_s, den_s)) = text.split_once('/') {
        let num: f64 = num_s.parse().ok()?;
        let den: f64 = den_s.parse().ok()?;
        if den > 0.0 {
            return Some(num / den);
        }
        return None;
    }
    text.parse().ok()
}

fn apply_sample_aspect_ratio(width: u32, height: u32, sar: Option<&str>) -> (u32, u32) {
    let Some(sar_str) = sar else {
        return (width, height);
    };
    let Some((num_s, den_s)) = sar_str.trim().split_once(':') else {
        return (width, height);
    };
    let (Ok(num), Ok(den)) = (num_s.parse::<f64>(), den_s.parse::<f64>()) else {
        return (width, height);
    };
    if num <= 0.0 || den <= 0.0 || (num - den).abs() < 0.001 {
        return (width, height);
    }
    let adjusted_w = ((width as f64) * (num / den)).round().max(1.0) as u32;
    (adjusted_w, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_ffprobe_json() {
        let sample = r#"{
            "streams": [
                {
                    "codec_type": "video",
                    "codec_name": "h264",
                    "width": 1920,
                    "height": 1080,
                    "sample_aspect_ratio": "1:1",
                    "avg_frame_rate": "30000/1001"
                },
                {
                    "codec_type": "audio",
                    "codec_name": "aac",
                    "channels": 2,
                    "sample_rate": "48000"
                }
            ],
            "format": {
                "duration": "125.5",
                "size": "10485760",
                "bit_rate": "668000",
                "format_name": "mov,mp4"
            }
        }"#;
        let info = parse_ffprobe_json(Path::new("clip.mp4"), sample, 0, None).unwrap();
        assert_eq!(info.duration_ms, 125_500);
        assert_eq!(info.duration_hms(), "00:02:05");
        assert_eq!(info.display_width, 1920);
        assert_eq!(info.display_height, 1080);
        assert_eq!(info.video_codec, "h264");
        assert_eq!(info.audio_summary.as_deref(), Some("aac, 48000 Hz, 2ch"));
        assert!((info.fps - 29.97).abs() < 0.02);
    }

    #[test]
    fn formats_timestamps_cleanly() {
        assert_eq!(format_timestamp_hms(0), "00:00:00");
        assert_eq!(format_timestamp_hms(3_661_000), "01:01:01");
    }
}
