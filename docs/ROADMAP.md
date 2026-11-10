# CutDub (ReDub) — Development Roadmap

## Phase 1 — Working Precision Video Utility (Completed)
- [x] Modular 7-crate Cargo workspace (`redub-app`, `redub-core`, `redub-media`, `redub-preview`, `redub-filters`, `redub-export`, `redub-platform`).
- [x] Native `egui`/`eframe` (`wgpu`) dark-themed desktop interface inspired by VirtualDub2.
- [x] Cross-platform `ffmpeg` and `ffprobe` discovery (`PATH`, platform directories, environment overrides, persistent settings).
- [x] File open dialog, drag-and-drop video loading, recent files history, and built-in synthetic sample video generator.
- [x] Container, video, audio, and subtitle metadata inspection via `ffprobe`.
- [x] Background exact packet PTS & keyframe (`[K]`) indexing supporting both CFR and VFR media.
- [x] Interactive timeline with keyframe markers, selection region shading, and frame/timestamp navigation.
- [x] Play/Pause, 1-frame step, 10-frame step, previous/next keyframe jump, start/end jump, and Go-To Frame/Timestamp dialog.
- [x] Mark In / Mark Out selection markers and lossless Direct Stream Copy export with keyframe constraint analysis.
- [x] Asynchronous background export with real-time progress bar, FPS/speed telemetry, and instant cancellation.
- [x] Full keyboard shortcut coverage.

## Phase 2 — Non-Destructive Editing & Full Processing (Completed)
- [x] Non-destructive Edit Decision List (EDL) with multi-segment timeline mapping (`EditSegment`, `ResolvedFrame`).
- [x] Delete Selection, Trim to Selection, Append Compatible Video, and Reset Timeline operations.
- [x] Full Undo / Redo stack with operation descriptions.
- [x] Dual Source and Output preview panels with Fit / 50% / 100% / 200% zoom and layout switcher.
- [x] Modular, reorderable, serializable Video Filter Pipeline (`Crop`, `Resize`, `Rotate`, `Flip`, `BrightnessContrast`, `ColorAdjust`, `Deinterlace`, `Sharpen`, `Denoise`, `Grayscale`) with real-time Output preview and FFmpeg `-vf` compilation.
- [x] Multi-track Audio & Subtitle stream configuration (Direct Stream Copy, Re-encode with codec/bitrate selection, Disable, and Audio Sync Delay in `ms`).
- [x] Dynamic FFmpeg encoder and hardware accelerator discovery (`MediaCapabilities`).
- [x] User-defined encoding presets (`.json`) and CutDub Project file save/load (`.cutdub.json`).

## Phase 3 — Advanced Capabilities (Completed)
- [x] Hardware-accelerated decoding (`-hwaccel auto` / configurable backend with automatic software fallback).
- [x] Improved bidirectional window prefetching and LRU frame cache telemetry (hit rate %, memory footprint, cached frame count).
- [x] Background audio waveform extraction (`AudioWaveform` peak + RMS envelopes) and dynamic EDL-aware timeline waveform visualization.
- [x] Asynchronous Batch Processing Queue (`BatchJobItem`, `start_batch_queue`) with live per-item progress and cancellation in the GUI.
- [x] Both 60fps real-time CPU RGBA filter preview and on-demand pixel-exact FFmpeg `-vf` frame render verification (`decode_filtered_frame`).
- [x] Declarative Scripting & Headless CLI Automation (`AutomationScript`, `cutdub --script <file.json>`, `cutdub --probe <video>`).
- [x] Cross-platform release packaging scripts (`scripts/package-macos.sh`, `scripts/package-linux.sh`, `scripts/package-windows.ps1`) and GitHub Actions CI matrix (`.github/workflows/ci.yml`).

## Phase 4 — Production Polish & High-Precision Enhancements (Completed)
- [x] Continuous backpressure-paced playback streaming (`ContinuousFrameStream`) for pause-free playback of multi-gigabyte files on external hard drives.
- [x] Real-time speaker audio playback (`AudioPreviewPlayer` via `rodio`) synchronized to playhead, EDL segments, volume/mute controls, and live Audio Sync Delay (`ms`).
- [x] Ultra-fast local-window keyframe indexing (`probe_local_keyframes` via `ffprobe -read_intervals`) for instant exact keyframe navigation and stream-copy analysis on large (`> 150 MB`) files without full-file disk scans.
- [x] Smart Cut / Smart Rendering (`VideoProcessingMode::SmartCopy`) for hybrid GOP boundary re-encoding + lossless stream copy.
- [x] Interactive on-canvas Crop handles in the Source preview canvas, Live 64-bin RGB/Luma Histogram & Clipping Scope (`FrameHistogram`), and Full-Resolution PNG/JPEG Frame Snapshot export (`Cmd/Ctrl+Shift+S`).
