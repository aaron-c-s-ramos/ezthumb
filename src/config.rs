use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Lists default video file extensions accepted during recursive directory scans.
pub const DEFAULT_VIDEO_EXTENSIONS: &[&str] = &[
    "3gp", "3g2", "asf", "asx", "avi", "avs", "divx", "flv", "f4v", "f4p", "f4a", "f4b", "m1v",
    "m2v", "m4p", "m4v", "mjpg", "mkv", "mov", "movie", "mp2", "mp4", "mpa", "mpe", "mpeg", "mpg",
    "mpv", "mv", "ogv", "ogg", "qt", "rm", "rmvb", "rv", "ts", "viv", "vivo", "vob", "webm", "wmv",
    "yuv",
];

/// Represents validation and persistence errors for configuration values.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Invalid grid specification '{0}'. Expected CxR (for example, 4x4 or 0x12).")]
    InvalidGrid(String),
    #[error(
        "Invalid size specification '{0}'. Expected WxH or NN% (for example, 320x180 or 50%)."
    )]
    InvalidSize(String),
    #[error("Invalid output format '{0}'. Expected jpg[@Q], png, or gif[@MS].")]
    InvalidFormat(String),
    #[error("Invalid hex color '{0}'. Expected 6 hex digits (RRGGBB).")]
    InvalidColor(String),
    #[error("Invalid time specification '{0}'. Expected HH:MM:SS, seconds, or NN%.")]
    InvalidTime(String),
    #[error("Invalid position code '{0}'.")]
    InvalidPosition(String),
    #[error("Invalid option value '{0}'.")]
    InvalidValue(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Defines an RGB color triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// Creates an RGB color from byte components.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses a 6-digit hexadecimal color string (optional leading #).
    pub fn parse_hex(input: &str) -> Result<Self, ConfigError> {
        let trimmed = input.trim().trim_start_matches('#');
        if trimmed.len() != 6 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ConfigError::InvalidColor(input.to_string()));
        }
        let r = u8::from_str_radix(&trimmed[0..2], 16)
            .map_err(|_| ConfigError::InvalidColor(input.to_string()))?;
        let g = u8::from_str_radix(&trimmed[2..4], 16)
            .map_err(|_| ConfigError::InvalidColor(input.to_string()))?;
        let b = u8::from_str_radix(&trimmed[4..6], 16)
            .map_err(|_| ConfigError::InvalidColor(input.to_string()))?;
        Ok(Self { r, g, b })
    }

    /// Formats the color as lowercase 6-digit hexadecimal.
    pub fn to_hex(self) -> String {
        format!("{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// Selects how captured frames are arranged or saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSpec {
    /// Number of columns. Zero saves individual numbered image files.
    pub cols: u32,
    /// Number of rows (or total frame count when cols is zero).
    pub rows: u32,
}

impl Default for GridSpec {
    fn default() -> Self {
        Self { cols: 4, rows: 4 }
    }
}

impl GridSpec {
    /// Parses a CxR grid string such as 4x4 or 0x16.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let lower = input.trim().to_ascii_lowercase();
        let (c_str, r_str) = lower
            .split_once('x')
            .ok_or_else(|| ConfigError::InvalidGrid(input.to_string()))?;
        let cols: u32 = c_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidGrid(input.to_string()))?;
        let rows: u32 = r_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidGrid(input.to_string()))?;
        if cols == 0 && rows == 0 {
            return Err(ConfigError::InvalidGrid(input.to_string()));
        }
        Ok(Self {
            cols: cols.min(32),
            rows: rows.clamp(1, 200),
        })
    }

    /// Returns true when frames save to separate files instead of a tiled sheet.
    pub fn is_individual(self) -> bool {
        self.cols == 0
    }

    /// Computes total shot count for this grid.
    pub fn shot_count(self) -> u32 {
        if self.cols == 0 {
            self.rows.max(1)
        } else {
            (self.cols * self.rows).max(1)
        }
    }
}

/// Selects the high-level grid and frame-sampling profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GridSetting {
    /// Automatically chooses columns and rows from video duration.
    #[default]
    Auto,
    /// Fixed columns x rows contact sheet.
    ColumnAndRow,
    /// Fixed columns with shots spaced every X seconds.
    ColumnAndStep,
    /// Fixed count of separate numbered image files.
    DiscreteByNumber,
    /// Separate numbered image files spaced every X seconds.
    DiscreteByStep,
    /// Separate numbered image files extracted from video key frames (I-frames).
    DiscreteKeyFrames,
}

/// Selects how thumbnail dimensions are calculated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SizeSpec {
    /// Automatically selects thumbnail dimensions from video resolution and layout.
    #[default]
    Auto,
    /// Fixed total canvas width in pixels (thumbnail width derives from columns and gaps).
    CanvasWidth(u32),
    /// Fixed dimensions per thumbnail tile in pixels.
    TilePixels { width: u32, height: u32 },
    /// Uniform percentage scale relative to the source video frame.
    ZoomPercent(u32),
}

impl SizeSpec {
    /// Parses WxH pixel dimensions or NN% zoom ratio.
    pub fn parse_ssize(input: &str) -> Result<Self, ConfigError> {
        let trimmed = input.trim().to_ascii_lowercase();
        if trimmed == "auto" {
            return Ok(Self::Auto);
        }
        if let Some(pct_str) = trimmed.strip_suffix('%') {
            let pct: u32 = pct_str
                .trim()
                .parse()
                .map_err(|_| ConfigError::InvalidSize(input.to_string()))?;
            if pct == 0 || pct > 400 {
                return Err(ConfigError::InvalidSize(input.to_string()));
            }
            return Ok(Self::ZoomPercent(pct));
        }
        let (w_str, h_str) = trimmed
            .split_once('x')
            .ok_or_else(|| ConfigError::InvalidSize(input.to_string()))?;
        let width: u32 = w_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidSize(input.to_string()))?;
        let height: u32 = h_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidSize(input.to_string()))?;
        if width < 32 || height < 24 || width > 4096 || height > 4096 {
            return Err(ConfigError::InvalidSize(input.to_string()));
        }
        Ok(Self::TilePixels { width, height })
    }
}

/// Selects the output image container and format parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    Jpeg { quality: u8 },
    Png,
    Gif,
    AnimatedGif { delay_ms: u32 },
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Jpeg { quality: 95 }
    }
}

impl OutputFormat {
    /// Parses jpg[@Q], png, or gif[@MS] format strings.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let lower = input.trim().to_ascii_lowercase();
        if let Some((kind, param)) = lower.split_once('@') {
            let val: u32 = param
                .trim()
                .parse()
                .map_err(|_| ConfigError::InvalidFormat(input.to_string()))?;
            match kind.trim() {
                "jpg" | "jpeg" => Ok(Self::Jpeg {
                    quality: val.clamp(1, 100) as u8,
                }),
                "gif" => Ok(Self::AnimatedGif {
                    delay_ms: val.clamp(50, 10_000),
                }),
                _ => Err(ConfigError::InvalidFormat(input.to_string())),
            }
        } else {
            match lower.as_str() {
                "jpg" | "jpeg" => Ok(Self::Jpeg { quality: 95 }),
                "png" => Ok(Self::Png),
                "gif" => Ok(Self::Gif),
                _ => Err(ConfigError::InvalidFormat(input.to_string())),
            }
        }
    }

    /// Returns the file extension for this output format.
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Jpeg { .. } => "jpg",
            Self::Png => "png",
            Self::Gif | Self::AnimatedGif { .. } => "gif",
        }
    }
}

/// Defines how existing output files are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// Appends _1 .. _255 before the extension to create a new thumbnail file.
    #[default]
    CreateNew,
    /// Appends .1 .. .255 before the extension when the output file exists.
    Copy,
    /// Replaces existing output files in place.
    Overwrite,
    /// Skips generation when the output file already exists.
    Skip,
}

impl ConflictPolicy {
    /// Parses new, copy, on, or off values from CLI flags.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        match input.trim().to_ascii_lowercase().as_str() {
            "new" | "create" | "createnew" => Ok(Self::CreateNew),
            "on" | "yes" | "true" | "overwrite" => Ok(Self::Overwrite),
            "off" | "no" | "false" | "skip" => Ok(Self::Skip),
            "copy" => Ok(Self::Copy),
            _ => Err(ConfigError::InvalidValue(input.to_string())),
        }
    }
}

/// Defines anchor positions for header info or inset timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextPosition {
    LeftTop,
    MiddleTop,
    RightTop,
    LeftBottom,
    MiddleBottom,
    RightBottom,
}

impl TextPosition {
    /// Parses standard ezthumb two-letter position codes.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        match input.trim().to_ascii_lowercase().as_str() {
            "lt" => Ok(Self::LeftTop),
            "mt" => Ok(Self::MiddleTop),
            "rt" => Ok(Self::RightTop),
            "lb" => Ok(Self::LeftBottom),
            "mb" => Ok(Self::MiddleBottom),
            "rb" => Ok(Self::RightBottom),
            _ => Err(ConfigError::InvalidPosition(input.to_string())),
        }
    }
}

/// Selects the display unit for media file size in the header block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeUnit {
    Auto,
    Byte,
    Kb,
    Mb,
    Gb,
}

impl SizeUnit {
    /// Parses auto, byte, kb, mb, or gb unit names.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        match input.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "byte" | "bytes" | "b" => Ok(Self::Byte),
            "kb" | "k" => Ok(Self::Kb),
            "mb" | "m" => Ok(Self::Mb),
            "gb" | "g" => Ok(Self::Gb),
            _ => Err(ConfigError::InvalidValue(input.to_string())),
        }
    }

    /// Formats a byte count according to the chosen unit.
    pub fn format_bytes(self, bytes: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * 1024.0;
        const GB: f64 = 1024.0 * 1024.0 * 1024.0;
        let b = bytes as f64;
        match self {
            Self::Byte => format!("{bytes} bytes"),
            Self::Kb => format!("{:.2} KB", b / KB),
            Self::Mb => format!("{:.2} MB", b / MB),
            Self::Gb => format!("{:.2} GB", b / GB),
            Self::Auto => {
                if b >= GB {
                    format!("{:.2} GB", b / GB)
                } else if b >= MB {
                    format!("{:.2} MB", b / MB)
                } else if b >= KB {
                    format!("{:.2} KB", b / KB)
                } else {
                    format!("{bytes} bytes")
                }
            }
        }
    }
}

/// Represents a time boundary in milliseconds or percentage of video duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeSpec {
    Milliseconds(u64),
    Percent(u8),
}

impl TimeSpec {
    /// Parses HH:MM:SS, MM:SS, integer seconds, or NN% strings.
    pub fn parse(input: &str) -> Result<Self, ConfigError> {
        let trimmed = input.trim();
        if let Some(pct_str) = trimmed.strip_suffix('%') {
            let pct: u8 = pct_str
                .trim()
                .parse()
                .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
            if pct > 100 {
                return Err(ConfigError::InvalidTime(input.to_string()));
            }
            return Ok(Self::Percent(pct));
        }
        let parts: Vec<&str> = trimmed.split(':').collect();
        let secs: u64 = match parts.as_slice() {
            [s] => s
                .parse()
                .map_err(|_| ConfigError::InvalidTime(input.to_string()))?,
            [m, s] => {
                let mm: u64 = m
                    .parse()
                    .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
                let ss: u64 = s
                    .parse()
                    .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
                mm * 60 + ss
            }
            [h, m, s] => {
                let hh: u64 = h
                    .parse()
                    .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
                let mm: u64 = m
                    .parse()
                    .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
                let ss: u64 = s
                    .parse()
                    .map_err(|_| ConfigError::InvalidTime(input.to_string()))?;
                hh * 3600 + mm * 60 + ss
            }
            _ => return Err(ConfigError::InvalidTime(input.to_string())),
        };
        Ok(Self::Milliseconds(secs * 1000))
    }

    /// Resolves this specification to milliseconds against a total video duration.
    pub fn resolve_ms(self, duration_ms: u64) -> u64 {
        match self {
            Self::Milliseconds(ms) => ms.min(duration_ms),
            Self::Percent(pct) => (duration_ms.saturating_mul(u64::from(pct))) / 100,
        }
    }
}

/// Selects dark or light visual styling in the desktop GUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// Selects how FFprobe inspects media duration and stream headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DurationScanMode {
    /// Runs fast scan first and falls back to full scan when duration is absent.
    #[default]
    Auto,
    /// Standard header-level FFprobe metadata inspection.
    FastScan,
    /// Extended FFprobe analyze duration and probe size for complex containers.
    #[serde(alias = "DeepProbe")]
    FullScan,
    /// Minimal header-head read with small probe size and analyze duration.
    FileHead,
}

/// Selects how FFmpeg seeks and decodes frames during thumbnail extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProcessMethod {
    /// Fast input seek with automatic accurate retry fallback.
    #[default]
    Auto,
    /// Fast keyframe-aligned input seek before -i.
    UponKeyFrames,
    /// Frame-accurate output seek after -i.
    FrameByFrame,
    /// Single-pass FFmpeg extraction for sequential streams.
    SinglePass,
    /// Hybrid coarse input seek followed by fine output seek.
    TwoPass,
    /// Single-threaded error-tolerant accurate decode for damaged media.
    SafeMode,
    /// Extracts nearest key frames directly with -skip_frame nokey.
    KeyFrameRip,
}

/// Selects the font source used for thumbnail text overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FontSource {
    /// Uses the built-in system fallback chain.
    #[default]
    SystemDefault,
    /// Uses a font chosen from installed operating system font directories.
    SystemFont,
    /// Uses an explicit custom TrueType or OpenType font file path.
    CustomFile,
}

/// Stores all persisted and runtime EzThumb configuration values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EzConfig {
    pub grid_setting: GridSetting,
    pub grid: GridSpec,
    pub size: SizeSpec,
    pub format: OutputFormat,
    pub transparent_bg: bool,
    pub timestep_ms: Option<u64>,
    pub time_from: Option<TimeSpec>,
    pub time_end: Option<TimeSpec>,
    pub include_first_frame: bool,
    pub include_last_frame: bool,
    pub accurate_seek: bool,
    pub process_method: ProcessMethod,
    pub duration_scan: DurationScanMode,
    pub show_info: bool,
    pub info_position: TextPosition,
    pub show_timestamp: bool,
    pub timestamp_position: TextPosition,
    pub info_font_size: u32,
    pub timestamp_font_size: u32,
    pub font_source: FontSource,
    pub font_path: Option<PathBuf>,
    pub gap_px: u32,
    pub margin_px: u32,
    pub edge_px: u32,
    pub canvas_color: RgbColor,
    pub info_color: RgbColor,
    pub timestamp_color: RgbColor,
    pub suffix: String,
    pub outdir: Option<PathBuf>,
    pub conflict_policy: ConflictPolicy,
    pub size_unit: SizeUnit,
    pub video_stream_index: Option<usize>,
    pub recursive: bool,
    pub max_depth: usize,
    pub extension_filter: Vec<String>,
    pub theme: ThemeMode,
}

impl Default for EzConfig {
    fn default() -> Self {
        Self {
            grid_setting: GridSetting::Auto,
            grid: GridSpec::default(),
            size: SizeSpec::Auto,
            format: OutputFormat::default(),
            transparent_bg: false,
            timestep_ms: None,
            time_from: None,
            time_end: None,
            include_first_frame: false,
            include_last_frame: false,
            accurate_seek: false,
            process_method: ProcessMethod::Auto,
            duration_scan: DurationScanMode::Auto,
            show_info: true,
            info_position: TextPosition::LeftTop,
            show_timestamp: true,
            timestamp_position: TextPosition::RightBottom,
            info_font_size: 16,
            timestamp_font_size: 14,
            font_source: FontSource::SystemDefault,
            font_path: None,
            gap_px: 4,
            margin_px: 8,
            edge_px: 0,
            canvas_color: RgbColor::new(24, 26, 31),
            info_color: RgbColor::new(238, 242, 247),
            timestamp_color: RgbColor::new(255, 255, 255),
            suffix: "_thumb".to_string(),
            outdir: None,
            conflict_policy: ConflictPolicy::CreateNew,
            size_unit: SizeUnit::Auto,
            video_stream_index: None,
            recursive: false,
            max_depth: 0,
            extension_filter: DEFAULT_VIDEO_EXTENSIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            theme: ThemeMode::Dark,
        }
    }
}

impl EzConfig {
    /// Returns true when frames save as separate numbered image files.
    pub fn is_discrete_output(&self) -> bool {
        match self.grid_setting {
            GridSetting::DiscreteByNumber
            | GridSetting::DiscreteByStep
            | GridSetting::DiscreteKeyFrames => true,
            GridSetting::ColumnAndRow => self.grid.is_individual(),
            GridSetting::Auto | GridSetting::ColumnAndStep => false,
        }
    }

    /// Resolves effective columns and rows for a video duration.
    pub fn resolved_grid(&self, duration_ms: u64) -> GridSpec {
        match self.grid_setting {
            GridSetting::Auto => {
                if duration_ms <= 60_000 {
                    GridSpec { cols: 2, rows: 2 }
                } else if duration_ms <= 300_000 {
                    GridSpec { cols: 3, rows: 3 }
                } else if duration_ms <= 1_200_000 {
                    GridSpec { cols: 4, rows: 4 }
                } else if duration_ms <= 3_600_000 {
                    GridSpec { cols: 4, rows: 5 }
                } else {
                    GridSpec { cols: 4, rows: 6 }
                }
            }
            GridSetting::ColumnAndRow => self.grid,
            GridSetting::ColumnAndStep => GridSpec {
                cols: self.grid.cols.max(1),
                rows: self.grid.rows.max(1),
            },
            GridSetting::DiscreteByNumber => GridSpec {
                cols: 0,
                rows: self.grid.rows.max(1),
            },
            GridSetting::DiscreteByStep | GridSetting::DiscreteKeyFrames => {
                GridSpec { cols: 0, rows: 200 }
            }
        }
    }

    /// Resolves the active processing method while respecting legacy accurate_seek.
    pub fn effective_process_method(&self) -> ProcessMethod {
        if self.accurate_seek && self.process_method == ProcessMethod::Auto {
            ProcessMethod::FrameByFrame
        } else {
            self.process_method
        }
    }

    /// Returns the default per-user configuration file path.
    pub fn config_file_path() -> PathBuf {
        #[cfg(windows)]
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(std::env::temp_dir);
        base.join("ezthumb").join("config.json")
    }

    /// Loads persisted configuration from disk or returns defaults when absent.
    pub fn load() -> Self {
        let path = Self::config_file_path();
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    /// Saves configuration to disk as formatted JSON.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_file_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        fs::write(&path, body)?;
        Ok(())
    }

    /// Removes the persisted configuration file to restore factory defaults.
    pub fn factory_reset() -> Result<(), ConfigError> {
        let path = Self::config_file_path();
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Checks whether a file path matches the configured extension filter.
    pub fn matches_filter(&self, path: &Path) -> bool {
        if self.extension_filter.is_empty() {
            return true;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let lower = ext.to_ascii_lowercase();
        self.extension_filter
            .iter()
            .any(|allowed| allowed == &lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_grid_specs_and_rejects_zero_by_zero() {
        assert_eq!(
            GridSpec::parse("4x4").unwrap(),
            GridSpec { cols: 4, rows: 4 }
        );
        assert!(GridSpec::parse("0x12").unwrap().is_individual());
        assert_eq!(GridSpec::parse("0x12").unwrap().shot_count(), 12);
        assert!(GridSpec::parse("0x0").is_err());
        assert!(GridSpec::parse("bad").is_err());
    }

    #[test]
    fn parses_output_formats_and_clamps_quality() {
        assert_eq!(
            OutputFormat::parse("jpg@92").unwrap(),
            OutputFormat::Jpeg { quality: 92 }
        );
        assert_eq!(
            OutputFormat::parse("jpg").unwrap(),
            OutputFormat::Jpeg { quality: 95 }
        );
        assert_eq!(OutputFormat::parse("png").unwrap(), OutputFormat::Png);
        assert_eq!(
            OutputFormat::parse("gif@750").unwrap(),
            OutputFormat::AnimatedGif { delay_ms: 750 }
        );
        assert!(OutputFormat::parse("bmp").is_err());
    }

    #[test]
    fn defaults_match_requested_auto_and_jpeg_95_profiles() {
        let cfg = EzConfig::default();
        assert_eq!(cfg.grid_setting, GridSetting::Auto);
        assert_eq!(cfg.size, SizeSpec::Auto);
        assert_eq!(cfg.duration_scan, DurationScanMode::Auto);
        assert_eq!(cfg.process_method, ProcessMethod::Auto);
        assert_eq!(cfg.font_source, FontSource::SystemDefault);
        assert_eq!(cfg.conflict_policy, ConflictPolicy::CreateNew);
        assert_eq!(cfg.format, OutputFormat::Jpeg { quality: 95 });
    }

    #[test]
    fn resolves_auto_grid_by_video_duration() {
        let cfg = EzConfig::default();
        assert_eq!(cfg.resolved_grid(45_000), GridSpec { cols: 2, rows: 2 });
        assert_eq!(cfg.resolved_grid(180_000), GridSpec { cols: 3, rows: 3 });
        assert_eq!(cfg.resolved_grid(900_000), GridSpec { cols: 4, rows: 4 });
        assert_eq!(cfg.resolved_grid(5_400_000), GridSpec { cols: 4, rows: 6 });
    }

    #[test]
    fn parses_colors_and_times() {
        assert_eq!(
            RgbColor::parse_hex("#1a2b3c").unwrap(),
            RgbColor::new(0x1a, 0x2b, 0x3c)
        );
        assert_eq!(
            TimeSpec::parse("01:02:03").unwrap().resolve_ms(9_999_999),
            3_723_000
        );
        assert_eq!(TimeSpec::parse("25%").unwrap().resolve_ms(80_000), 20_000);
    }

    #[test]
    fn formats_file_sizes_for_each_unit() {
        assert_eq!(SizeUnit::Byte.format_bytes(512), "512 bytes");
        assert_eq!(SizeUnit::Kb.format_bytes(2048), "2.00 KB");
        assert_eq!(SizeUnit::Mb.format_bytes(1024 * 1024 * 3), "3.00 MB");
        assert_eq!(SizeUnit::Auto.format_bytes(1024 * 1024 * 5), "5.00 MB");
    }
}
