mod app;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use app::CutDubApp;
use redub_export::{execute_automation_script, AutomationScript};
use redub_media::probe_media_file;
use redub_platform::FfmpegBinaries;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("CutDub (ReDub) {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("CutDub (ReDub) {} — Cross-Platform Precision Video Processor", env!("CARGO_PKG_VERSION"));
        println!("\nUSAGE:");
        println!("  cutdub [VIDEO_OR_PROJECT_PATH]       Launch desktop GUI");
        println!("  cutdub --probe <VIDEO_PATH>          Inspect media streams, keyframes, and VFR/CFR timing");
        println!("  cutdub --script <SCRIPT.json>        Execute a declarative CutDub automation script headlessly");
        println!("  cutdub --version                     Print version information");
        return ExitCode::SUCCESS;
    }

    // Headless CLI: --probe <file>
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        let Some(target) = args.get(pos + 1) else {
            eprintln!("Error: --probe requires a file path");
            return ExitCode::FAILURE;
        };
        let bins = match FfmpegBinaries::discover(None, None) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Error discovering FFmpeg/ffprobe: {e}");
                return ExitCode::FAILURE;
            }
        };
        match probe_media_file(&bins, &PathBuf::from(target), true) {
            Ok(clip) => {
                println!("File:       {}", clip.path.display());
                println!("Container:  {}", clip.container_format);
                println!("Video:      {} {}x{} ({}) @ {} ({})",
                    clip.video_codec,
                    clip.width,
                    clip.height,
                    clip.pixel_format,
                    clip.timing.nominal_fps,
                    if clip.timing.is_vfr { "VFR" } else { "CFR" }
                );
                println!("Frames:     {} ({} keyframes, duration {})",
                    clip.timing.total_frames(),
                    clip.timing.keyframe_count(),
                    clip.timing.total_duration_us()
                );
                println!("Audio:      {} track(s)", clip.audio_streams.len());
                println!("Subtitles:  {} track(s)", clip.subtitle_streams.len());
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("Probe failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Headless CLI: --script <script.json>
    if let Some(pos) = args.iter().position(|a| a == "--script") {
        let Some(script_path) = args.get(pos + 1) else {
            eprintln!("Error: --script requires a path to a .cutdub-script.json file");
            return ExitCode::FAILURE;
        };
        let bins = match FfmpegBinaries::discover(None, None) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Error discovering FFmpeg/ffprobe: {e}");
                return ExitCode::FAILURE;
            }
        };
        let script = match AutomationScript::load_from_file(&PathBuf::from(script_path)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to load script '{script_path}': {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("Executing CutDub script '{}'...", script.name);
        match execute_automation_script(&bins, &script) {
            Ok(dur) => {
                println!(
                    "Script '{}' completed successfully -> '{}' (duration {})",
                    script.name,
                    script.output_path.display(),
                    dur
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                eprintln!("Script execution failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let initial_file = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(PathBuf::from);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CutDub — Precision Video Processor")
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([960.0, 620.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    match eframe::run_native(
        "CutDub",
        native_options,
        Box::new(move |cc| Ok(Box::new(CutDubApp::new(cc, initial_file)))),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("GUI error: {e}");
            ExitCode::FAILURE
        }
    }
}
