use std::path::PathBuf;

use walkdir::WalkDir;

use crate::config::{
    ConflictPolicy, EzConfig, FontSource, GridSetting, GridSpec, OutputFormat, ProcessMethod,
    RgbColor, SizeSpec, SizeUnit, TextPosition, TimeSpec,
};
use crate::gui::run_gui;
use crate::probe::probe_video;
use crate::render::process_video;

/// Represents the resolved top-level action requested on the command line.
#[derive(Debug, PartialEq, Eq)]
pub enum CliAction {
    Help,
    Version,
    FactoryReset,
    LaunchGui(Vec<PathBuf>),
    RunBatch {
        inputs: Vec<PathBuf>,
        info_short: bool,
        info_detail: bool,
    },
}

/// Prints version information matching GPL-3.0 licensing.
pub fn version_text() -> String {
    format!(
        "ezthumb {} (Rust)\nCopyright (C) 2011-2024 Andy Xuming, 2026 Aaron Christopher Ramos\nLicense GPL-3.0-only: GNU GPL version 3 <https://www.gnu.org/licenses/gpl-3.0.html>\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// Returns CLI usage text covering all supported options.
pub fn help_text() -> &'static str {
    "Usage: ezthumb [OPTIONS] [VIDEO_OR_DIR...]\n\
\n\
Run with no input paths or pass -G / --gui to open the desktop GUI.\n\
\n\
OPTIONS:\n\
  -g, --grid CxR            Grid columns x rows (default auto, 0xN saves separate files)\n\
  -w, --width PIXELS        Total canvas width in pixels\n\
  -s, --ssize WxH|NN%|auto  Per-tile size in pixels, percentage zoom, or auto\n\
  -m, --format FMT          Output format: jpg[@Q] (default jpg@95), png, gif[@MS]\n\
  -t, --timestep MS         Fixed interval between screenshots in milliseconds\n\
  -o, --outdir DIR          Directory for output images (default: source folder)\n\
  -x, --suffix TEXT         Filename suffix (default _thumb)\n\
  -c, --colour MI:TM:BG     Hex colors RRGGBB for info, timestamp, and background\n\
  -f, --font PATH           Path to a TrueType font file\n\
  -F, --fontsize MI:TM      Font sizes in pixels for info and timestamp\n\
  -i, --info                Print short media info lines without writing images\n\
  -I, --inside              Print detailed media info without writing images\n\
  -G, --gui                 Launch GUI mode (preloading any passed video paths)\n\
  -R, --recursive           Scan directories recursively\n\
      --depth N             Max recursion depth (0 = unlimited)\n\
      --filter EXT1,EXT2    Comma-separated allowed video extensions\n\
      --accurate            Use slower frame-accurate FFmpeg seek\n\
      --transparent         Transparent background for PNG and GIF outputs\n\
      --override MODE       Existing file policy: new (default), copy, on, off\n\
      --opt-info VAL        Media info header: on, off, lt, mt, rt\n\
      --opt-time VAL        Inset timestamp: on, off, lt, mt, rt, lb, mb, rb\n\
      --opt-ffr on|off      Start capture at first frame (default off)\n\
      --opt-lfr on|off      End capture at last frame (default off)\n\
      --gap-shots PX        Gap in pixels between tiles (default 4)\n\
      --gap-margin PX       Outer canvas margin in pixels (default 8)\n\
      --edge PX             Tile border thickness in pixels (default 0)\n\
      --time-from SPEC      Start timestamp (HH:MM:SS, seconds, or NN%)\n\
      --time-end SPEC       End timestamp (HH:MM:SS, seconds, or NN%)\n\
      --size-unit UNIT      Header size unit: auto, byte, kb, mb, gb\n\
      --vindex N            Zero-based video stream index\n\
      --factory-reset       Remove saved configuration file and restore defaults\n\
  -h, --help                Show this help text\n\
  -V, --version             Show version information\n"
}

/// Parses EZTHUMB environment defaults followed by CLI arguments.
pub fn parse_args(args: &[String], config: &mut EzConfig) -> Result<CliAction, String> {
    let mut combined = Vec::new();
    if let Ok(env_val) = std::env::var("EZTHUMB") {
        combined.extend(split_env_args(&env_val));
    }
    combined.extend(args.iter().cloned());

    let mut inputs = Vec::new();
    let mut force_gui = false;
    let mut info_short = false;
    let mut info_detail = false;

    let mut idx = 0usize;
    while idx < combined.len() {
        let arg = &combined[idx];
        let next_val = |i: &mut usize, flag: &str| -> Result<String, String> {
            *i += 1;
            combined
                .get(*i)
                .cloned()
                .ok_or_else(|| format!("Option '{flag}' requires a value."))
        };

        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--factory-reset" => return Ok(CliAction::FactoryReset),
            "-G" | "--gui" => force_gui = true,
            "-i" | "--info" | "--list" => info_short = true,
            "-I" | "--inside" => info_detail = true,
            "-R" | "--recursive" => config.recursive = true,
            "--accurate" => {
                config.accurate_seek = true;
                config.process_method = ProcessMethod::FrameByFrame;
            }
            "--transparent" => config.transparent_bg = true,
            "-g" | "--grid" => {
                let val = next_val(&mut idx, arg)?;
                config.grid = GridSpec::parse(&val).map_err(|e| e.to_string())?;
                config.grid_setting = if config.grid.is_individual() {
                    GridSetting::DiscreteByNumber
                } else {
                    GridSetting::ColumnAndRow
                };
            }
            "-w" | "--width" => {
                let val = next_val(&mut idx, arg)?;
                let px: u32 = val.parse().map_err(|_| format!("Invalid width '{val}'."))?;
                config.size = SizeSpec::CanvasWidth(px.clamp(160, 16_384));
            }
            "-s" | "--ssize" => {
                let val = next_val(&mut idx, arg)?;
                config.size = SizeSpec::parse_ssize(&val).map_err(|e| e.to_string())?;
            }
            "-m" | "--format" => {
                let val = next_val(&mut idx, arg)?;
                config.format = OutputFormat::parse(&val).map_err(|e| e.to_string())?;
            }
            "-t" | "--timestep" => {
                let val = next_val(&mut idx, arg)?;
                let ms: u64 = val
                    .parse()
                    .map_err(|_| format!("Invalid timestep '{val}'."))?;
                config.timestep_ms = if ms == 0 { None } else { Some(ms) };
                if ms > 0 {
                    config.grid_setting = if config.grid.is_individual() {
                        GridSetting::DiscreteByStep
                    } else {
                        GridSetting::ColumnAndStep
                    };
                }
            }
            "-o" | "--outdir" => {
                let val = next_val(&mut idx, arg)?;
                config.outdir = Some(PathBuf::from(val));
            }
            "-x" | "--suffix" => {
                config.suffix = next_val(&mut idx, arg)?;
            }
            "-c" | "--colour" | "--color" => {
                let val = next_val(&mut idx, arg)?;
                let parts: Vec<&str> = val.split(':').collect();
                if parts.len() != 3 {
                    return Err(format!("Invalid colour '{val}'. Expected MI:TM:BG."));
                }
                if !parts[0].is_empty() {
                    config.info_color = RgbColor::parse_hex(parts[0]).map_err(|e| e.to_string())?;
                }
                if !parts[1].is_empty() {
                    config.timestamp_color =
                        RgbColor::parse_hex(parts[1]).map_err(|e| e.to_string())?;
                }
                if !parts[2].is_empty() {
                    config.canvas_color =
                        RgbColor::parse_hex(parts[2]).map_err(|e| e.to_string())?;
                }
            }
            "-f" | "--font" => {
                let val = next_val(&mut idx, arg)?;
                config.font_source = FontSource::CustomFile;
                config.font_path = Some(PathBuf::from(val));
            }
            "-F" | "--fontsize" => {
                let val = next_val(&mut idx, arg)?;
                if let Some((mi_s, tm_s)) = val.split_once(':') {
                    if !mi_s.is_empty() {
                        config.info_font_size = mi_s
                            .parse::<u32>()
                            .map_err(|_| format!("Invalid info font size '{mi_s}'."))?
                            .clamp(10, 48);
                    }
                    if !tm_s.is_empty() {
                        config.timestamp_font_size = tm_s
                            .parse::<u32>()
                            .map_err(|_| format!("Invalid timestamp font size '{tm_s}'."))?
                            .clamp(10, 36);
                    }
                } else {
                    let sz = val
                        .parse::<u32>()
                        .map_err(|_| format!("Invalid font size '{val}'."))?
                        .clamp(10, 48);
                    config.info_font_size = sz;
                    config.timestamp_font_size = sz.min(36);
                }
            }
            "--depth" => {
                let val = next_val(&mut idx, arg)?;
                config.max_depth = val.parse().map_err(|_| format!("Invalid depth '{val}'."))?;
            }
            "--filter" => {
                let val = next_val(&mut idx, arg)?;
                config.extension_filter = val
                    .split(',')
                    .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "--override" => {
                let val = next_val(&mut idx, arg)?;
                config.conflict_policy = ConflictPolicy::parse(&val).map_err(|e| e.to_string())?;
            }
            "--opt-info" => {
                let val = next_val(&mut idx, arg)?;
                match val.to_ascii_lowercase().as_str() {
                    "off" | "no" | "false" => config.show_info = false,
                    "on" | "yes" | "true" => config.show_info = true,
                    other => {
                        config.show_info = true;
                        config.info_position =
                            TextPosition::parse(other).map_err(|e| e.to_string())?;
                    }
                }
            }
            "--opt-time" => {
                let val = next_val(&mut idx, arg)?;
                match val.to_ascii_lowercase().as_str() {
                    "off" | "no" | "false" => config.show_timestamp = false,
                    "on" | "yes" | "true" => config.show_timestamp = true,
                    other => {
                        config.show_timestamp = true;
                        config.timestamp_position =
                            TextPosition::parse(other).map_err(|e| e.to_string())?;
                    }
                }
            }
            "--opt-ffr" => {
                let val = next_val(&mut idx, arg)?;
                config.include_first_frame = parse_on_off(&val)?;
            }
            "--opt-lfr" => {
                let val = next_val(&mut idx, arg)?;
                config.include_last_frame = parse_on_off(&val)?;
            }
            "--gap-shots" => {
                let val = next_val(&mut idx, arg)?;
                config.gap_px = val
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid gap '{val}'."))?
                    .min(128);
            }
            "--gap-margin" => {
                let val = next_val(&mut idx, arg)?;
                config.margin_px = val
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid margin '{val}'."))?
                    .min(128);
            }
            "--edge" => {
                let val = next_val(&mut idx, arg)?;
                config.edge_px = val
                    .parse::<u32>()
                    .map_err(|_| format!("Invalid edge width '{val}'."))?
                    .min(32);
            }
            "--time-from" => {
                let val = next_val(&mut idx, arg)?;
                config.time_from = Some(TimeSpec::parse(&val).map_err(|e| e.to_string())?);
            }
            "--time-end" => {
                let val = next_val(&mut idx, arg)?;
                config.time_end = Some(TimeSpec::parse(&val).map_err(|e| e.to_string())?);
            }
            "--size-unit" => {
                let val = next_val(&mut idx, arg)?;
                config.size_unit = SizeUnit::parse(&val).map_err(|e| e.to_string())?;
            }
            "--vindex" => {
                let val = next_val(&mut idx, arg)?;
                config.video_stream_index = Some(
                    val.parse()
                        .map_err(|_| format!("Invalid video stream index '{val}'."))?,
                );
            }
            other if other.starts_with('-') => {
                return Err(format!("Unknown option '{other}'. Run 'ezthumb --help'."));
            }
            path_arg => {
                inputs.push(PathBuf::from(path_arg));
            }
        }
        idx += 1;
    }

    if force_gui || inputs.is_empty() {
        return Ok(CliAction::LaunchGui(inputs));
    }

    Ok(CliAction::RunBatch {
        inputs,
        info_short,
        info_detail,
    })
}

/// Expands files and directories into a deduplicated list of target videos.
pub fn collect_video_files(inputs: &[PathBuf], config: &EzConfig) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for input in inputs {
        if input.is_file() {
            files.push(input.clone());
        } else if input.is_dir() {
            let mut walker = WalkDir::new(input);
            if !config.recursive {
                walker = walker.max_depth(1);
            } else if config.max_depth > 0 {
                walker = walker.max_depth(config.max_depth);
            }
            for entry in walker.into_iter().flatten() {
                if entry.file_type().is_file() && config.matches_filter(entry.path()) {
                    files.push(entry.into_path());
                }
            }
        }
    }
    files
}

/// Executes the parsed CLI or launches the desktop GUI.
pub fn run_cli(args: &[String]) -> i32 {
    let mut config = EzConfig::load();
    let action = match parse_args(args, &mut config) {
        Ok(act) => act,
        Err(msg) => {
            eprintln!("Error: {msg}");
            return 2;
        }
    };

    match action {
        CliAction::Help => {
            print!("{}", help_text());
            0
        }
        CliAction::Version => {
            print!("{}", version_text());
            0
        }
        CliAction::FactoryReset => match EzConfig::factory_reset() {
            Ok(()) => {
                println!("Configuration reset to factory defaults.");
                0
            }
            Err(err) => {
                eprintln!("Factory reset failed: {err}");
                1
            }
        },
        CliAction::LaunchGui(initial_files) => {
            hide_standalone_windows_console();
            if let Err(err) = run_gui(initial_files, config) {
                eprintln!("GUI error: {err}");
                return 1;
            }
            0
        }
        CliAction::RunBatch {
            inputs,
            info_short,
            info_detail,
        } => {
            let targets = collect_video_files(&inputs, &config);
            if targets.is_empty() {
                eprintln!("No matching video files found.");
                return 1;
            }

            let mut exit_code = 0;
            for target in &targets {
                if info_short || info_detail {
                    match probe_video(target, config.video_stream_index) {
                        Ok(info) => {
                            if info_detail {
                                for line in info.header_lines(config.size_unit) {
                                    println!("{line}");
                                }
                                println!();
                            } else {
                                println!("{}", info.short_line(config.size_unit));
                            }
                        }
                        Err(err) => {
                            eprintln!("Failed to probe '{}': {err}", target.display());
                            exit_code = 1;
                        }
                    }
                    continue;
                }

                match process_video(target, &config, None, |_, _| {}) {
                    Ok(outcome) => {
                        if outcome.skipped {
                            println!("Skipped (exists): {}", target.display());
                        } else {
                            for out in outcome.output_paths {
                                println!("Saved: {}", out.display());
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("Failed '{}': {err}", target.display());
                        exit_code = 1;
                    }
                }
            }
            exit_code
        }
    }
}

fn parse_on_off(val: &str) -> Result<bool, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "on" | "yes" | "true" | "1" => Ok(true),
        "off" | "no" | "false" | "0" => Ok(false),
        other => Err(format!("Expected 'on' or 'off', got '{other}'.")),
    }
}

fn split_env_args(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(ToString::to_string).collect()
}

/// Closes the console window when Windows spawned it exclusively for a GUI double-click.
#[cfg(windows)]
fn hide_standalone_windows_console() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    let mut pids = [0u32; 4];
    // SAFETY: GetConsoleProcessList writes at most pids.len() process IDs into pids.
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    if count == 1 {
        // SAFETY: Only this process is attached to the console, so detaching is safe.
        unsafe { FreeConsole() };
    }
}

#[cfg(not(windows))]
fn hide_standalone_windows_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_batch_cli_flags_and_targets() {
        let args: Vec<String> = [
            "-g",
            "3x5",
            "-w",
            "1600",
            "-m",
            "png",
            "--transparent",
            "--override",
            "on",
            "--opt-time",
            "lb",
            "movie.mkv",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect();

        let mut cfg = EzConfig::default();
        let action = parse_args(&args, &mut cfg).unwrap();
        assert_eq!(cfg.grid, GridSpec { cols: 3, rows: 5 });
        assert_eq!(cfg.size, SizeSpec::CanvasWidth(1600));
        assert_eq!(cfg.format, OutputFormat::Png);
        assert!(cfg.transparent_bg);
        assert_eq!(cfg.conflict_policy, ConflictPolicy::Overwrite);
        assert_eq!(cfg.timestamp_position, TextPosition::LeftBottom);
        assert_eq!(
            action,
            CliAction::RunBatch {
                inputs: vec![PathBuf::from("movie.mkv")],
                info_short: false,
                info_detail: false,
            }
        );
    }

    #[test]
    fn launches_gui_when_no_inputs_are_supplied() {
        let mut cfg = EzConfig::default();
        let action = parse_args(&[], &mut cfg).unwrap();
        assert_eq!(action, CliAction::LaunchGui(Vec::new()));
    }
}
