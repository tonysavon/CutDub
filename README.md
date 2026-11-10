# CutDub (ReDub) — Cross-Platform Precision Video Processor

**CutDub** (also known as **ReDub**) is a modern, open-source, cross-platform successor to **VirtualDub2**, written in Rust with a hardware-accelerated `egui` / `eframe` (`wgpu`) desktop interface and powered by **FFmpeg**.

Unlike heavy non-linear editors (Premiere Pro, DaVinci Resolve), CutDub is engineered as a fast, lightweight, keyboard-driven video-processing utility focused on:

- **Frame-accurate navigation** across both Constant Frame Rate (CFR) and Variable Frame Rate (VFR) video streams using 64-bit microsecond presentation timestamps (`TimestampUs`) and FFmpeg rational time bases (`Rational`).
- **Lossless Direct Stream Copy** (`-c:v copy`) with automatic keyframe constraint analysis and explicit keyframe pre-roll reporting — never silently claiming frame-exact lossless cuts where inter-frame dependencies make it impossible.
- **Smart Cut / Smart Rendering (`SmartCopy`)** that frame-accurately re-encodes only the partial lead-in GOP at non-keyframe cut boundaries and lossless stream-copies the remainder of the video from the next keyframe onward.
- **Full Processing Mode** with a modular, reorderable, real-time previewed video filter pipeline mapped directly to FFmpeg `-vf` filtergraphs, interactive on-canvas crop handles, and a live RGB/Luma histogram & clipping scope.
- **Continuous Streaming Playback & Live Audio Output**, engineered with a persistent backpressure-paced FFmpeg frame pipe and `rodio` speaker output to play multi-gigabyte videos smoothly directly off external drives without frame pauses.
- **Non-destructive Edit Decision List (EDL)** with Mark In / Mark Out selection, Delete Selection, Trim to Selection, Append Video Segment, and full Undo/Redo history.
- **Multi-track Audio & Subtitle Stream Control**, including per-track Direct Stream Copy, re-encoding, disabling, and real-time previewed audio synchronization delay (`ms`).

---

## Workspace Architecture

CutDub is organized as a modular Cargo workspace:

| Crate | Responsibility |
| :--- | :--- |
| `crates/redub-app` | Application entry point (`cutdub` and `redub` binaries) and `egui`/`eframe` (`wgpu`) dark-themed desktop GUI. |
| `crates/redub-core` | 64-bit timestamps (`TimestampUs`), rational time bases (`Rational`), VFR/CFR `FrameTimingMap`, non-destructive `Project` EDL, stream configs, and Undo/Redo history. |
| `crates/redub-media` | `ffprobe` metadata & stream inspection, fast packet-header PTS/keyframe indexing, dynamic FFmpeg encoder/hwaccel capability detection, and keyframe-assisted burst frame decoding. |
| `crates/redub-preview` | Bounded LRU `FrameCache`, wall-clock synchronized VFR/CFR `PlaybackClock`, and non-blocking background frame decode worker (`PreviewEngine`). |
| `crates/redub-filters` | Serializable, composable, reorderable `FilterPipeline` (`Crop`, `Resize`, `Rotate`, `Flip`, `BrightnessContrast`, `ColorAdjust`, `Deinterlace`) with both FFmpeg `-vf` compilation and real-time RGBA Output preview processing. |
| `crates/redub-export` | Asynchronous background export runner with Direct Stream Copy, Full Processing, multi-segment EDL concatenation, real-time progress telemetry, cancellation, and encoding presets. |
| `crates/redub-platform` | Cross-platform OS integration, configuration/cache directories (`AppPaths`), persistent `AppSettings`, and `ffmpeg`/`ffprobe` binary discovery. |

---

## Prerequisites & Cross-Platform Build Instructions

CutDub requires:
1. **Rust stable toolchain** (`1.80+`, installed via [rustup](https://rustup.rs/))
2. **FFmpeg & ffprobe** (`ffmpeg` and `ffprobe` available in `PATH` or configured via `CUTDUB_FFMPEG` / `CUTDUB_FFPROBE`)

### macOS (Apple Silicon `arm64` & Intel `x86_64`)

```bash
# Install FFmpeg via Homebrew
brew install ffmpeg

# Build and run CutDub in release mode
cargo run --release --bin cutdub

# Run all unit and integration tests
cargo test --workspace
```

### Linux (`x86_64`)

```bash
# Ubuntu / Debian dependencies
sudo apt-get update
sudo apt-get install -y ffmpeg build-essential pkg-config libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libgtk-3-dev

# Build and run
cargo run --release --bin cutdub
```

### Windows (`x86_64`)

```powershell
# Install FFmpeg via Winget or Chocolatey
winget install Gyan.FFmpeg

# Build and run
cargo run --release --bin cutdub
```

---

## Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Space` | Play / Pause |
| `Left` / `Right` | Step backward / forward 1 frame |
| `Shift + Left` / `Shift + Right` (or `PgUp` / `PgDn`) | Jump to previous / next keyframe (`[K]`) |
| `Alt + Left` / `Alt + Right` | Step backward / forward 10 frames |
| `Home` / `End` | Jump to start / end of timeline |
| `[` or `I` | Set selection start (Mark In) |
| `]` or `O` | Set selection end (Mark Out) |
| `Delete` / `Backspace` | Delete selected range from timeline |
| `Cmd/Ctrl + T` | Trim timeline to active selection |
| `Cmd/Ctrl + Z` | Undo edit operation |
| `Cmd/Ctrl + Y` / `Cmd/Ctrl + Shift + Z` | Redo edit operation |
| `Cmd/Ctrl + O` | Open video file |
| `Cmd/Ctrl + E` | Export video |
| `Cmd/Ctrl + G` | Go to exact frame number or timestamp |
| `Cmd/Ctrl + F` | Open Video Filters inspector |

---

## Headless CLI & Automation Scripting

In addition to the desktop GUI, `cutdub` supports headless media inspection and declarative automation scripts (`.cutdub-script.json`):

```bash
# Inspect streams, keyframes, and VFR/CFR timing from the terminal
cargo run --release --bin cutdub -- --probe path/to/video.mp4

# Execute a saved CutDub automation script headlessly
cargo run --release --bin cutdub -- --script workflow.cutdub-script.json
```

---

## Cross-Platform Release Packaging

- **macOS (`CutDub.app` bundle + `.tar.gz`):** `./scripts/package-macos.sh 0.1.0`
- **Linux (`.desktop` + portable `.tar.gz`):** `./scripts/package-linux.sh 0.1.0`
- **Windows (portable `.zip`):** `powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1`
- **GitHub Actions CI:** `.github/workflows/ci.yml`

---

Release archives are written to `releases/<version>/`. They require FFmpeg and ffprobe installed separately. macOS builds are unsigned and not notarized.

## Documentation

- [Architectural Decisions & Design (`docs/ARCHITECTURE.md`)](docs/ARCHITECTURE.md)
- [Development Roadmap & Milestones (`docs/ROADMAP.md`)](docs/ROADMAP.md)
