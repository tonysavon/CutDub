# CutDub (ReDub) — Architecture & Decision Records

## 1. System Overview

CutDub is architected as a modular 7-crate Rust workspace separating platform integration, core timeline/EDL domain logic, FFmpeg media operations, video filtering, bounded frame caching/playback, asynchronous export, and the `egui`/`eframe` (`wgpu`) GUI.

```
                  ┌──────────────────────────────────────┐
                  │              redub-app               │
                  │   (egui / eframe + wgpu Desktop UI)  │
                  └──────┬──────────┬──────────┬─────────┘
                         │          │          │
          ┌──────────────┘          │          └──────────────┐
          ▼                         ▼                         ▼
┌───────────────────┐     ┌───────────────────┐     ┌───────────────────┐
│   redub-preview   │     │   redub-export    │     │   redub-filters   │
│  LRU FrameCache,  │     │  Stream Copy &    │     │ Crop/Scale/Rotate │
│  PlaybackClock,   │     │  Full Processing  │     │ Color/Deinterlace │
│  Async Worker     │     │  Async Job Runner │     │ FFmpeg -vf & RGBA │
└─────────┬─────────┘     └─────────┬─────────┘     └─────────┬─────────┘
          │                         │                         │
          └──────────────►┌─────────┴─────────┐◄──────────────┘
                          │    redub-media    │
                          │ ffprobe, Packet   │
                          │ Indexer, Encoders,│
                          │ Frame Burst Dec.  │
                          └─────────┬─────────┘
                                    │
                          ┌─────────▼─────────┐
                          │    redub-core     │
                          │ TimestampUs (i64),│
                          │ Rational, VFR Map,│
                          │ Project EDL, Undo │
                          └─────────┬─────────┘
                                    │
                          ┌─────────▼─────────┐
                          │  redub-platform   │
                          │ Binary Discovery, │
                          │ AppPaths, Settings│
                          └───────────────────┘
```

---

## 2. Architectural Decision Records (ADRs)

### ADR-001: 64-Bit Microsecond Timestamps (`TimestampUs`) + Exact Packet Timing Table (`FrameTimingMap`)
- **Context:** Variable Frame Rate (VFR) recordings (screen captures, smartphones, WebM/MKV streams) and B-frame reordered streams violate the naive assumption that `PTS = frame_number * nominal_frame_duration`.
- **Decision:**
  - All timestamps in `redub-core` use signed 64-bit integer microseconds (`TimestampUs(i64)`) alongside exact rational numbers (`Rational { num: i64, den: i64 }`) for FFmpeg `time_base` and `r_frame_rate` / `avg_frame_rate`.
  - When a file is opened, CutDub immediately constructs an initial timing map from container metadata so the UI responds instantaneously, and spawns a background packet-header scanner (`ffprobe -show_entries packet=pts_time,dts_time,duration_time,flags`) that sorts packets into presentation order, records every exact frame `pts_us` and `duration_us`, flags every keyframe (`[K]`), and detects whether the stream is CFR or VFR.
  - Frame-to-timestamp lookup is $O(1)$ by index; timestamp-to-frame lookup is $O(\log N)$ via binary search over `FrameEntry::pts_us`.

### ADR-002: Keyframe-Aware Two-Stage Seeking & Burst Decoding (`FfmpegProcessDecoder`)
- **Context:** Spawning an external `ffmpeg` process to decode a single isolated frame by seeking from the start of a long file is slow, whereas linking C `libavcodec`/`libavformat` directly complicates cross-platform builds when system FFmpeg versions vary from FFmpeg 5.x to 9.x.
- **Decision:**
  - Isolate decoding behind the `VideoFrameDecoder` trait in `redub-media`.
  - Implement two-stage keyframe-assisted seeking: use fast demuxer input seeking (`-ss <keyframe_pts>` before `-i`) to jump directly to the nearest preceding keyframe recorded in `FrameTimingMap`, followed by exact frame offset seeking (`-ss <fine_offset>` after `-i`), and decode a contiguous **burst** of RGBA frames (`-f rawvideo -pix_fmt rgba pipe:1`) into the bounded LRU `FrameCache` in `redub-preview`.
  - Coalesce rapid timeline scrubbing via a single-slot mutex (`latest_job`) in `PreviewEngine` so the background decode worker always services the most recent scrub position without building a queue backlog.

### ADR-003: Honest Direct Stream Copy vs. Full Processing Distinction
- **Context:** Compressed inter-frame codecs (H.264, H.265, VP9, AV1) can only begin decoding cleanly at an IDR/keyframe (`[K]`). Tools that claim "lossless frame-accurate cutting" without re-encoding either silently shift cut points or produce undecodable leading inter-frames.
- **Decision:**
  - Provide explicit `DirectStreamCopy` and `FullProcessing` modes inspired by VirtualDub2.
  - Provide `Project::analyze_stream_copy()`, which inspects the `FrameTimingMap` at segment start points and reports in the UI whether the cut begins on a keyframe or how many frames/milliseconds of keyframe pre-roll precede the cut point.
  - Allow the user to choose `KeyframeCutPolicy::SnapToPrecedingKeyframe` (default, guaranteeing clean decodability) or `KeyframeCutPolicy::ExactTimestamp`.

### ADR-004: Dual-Target Filter Pipeline (FFmpeg `-vf` + Real-Time CPU RGBA Preview)
- **Context:** Users configuring Crop, Resize, Rotate, Flip, Brightness/Contrast, Colour Adjustments, or Deinterlace need immediate visual feedback in the Output preview panel, while exports should execute through native FFmpeg filtergraphs.
- **Decision:**
  - `redub-filters::FilterPipeline` compiles any sequence of enabled `FilterInstance`s into both:
    1. An exact FFmpeg `-vf` filtergraph string (`to_ffmpeg_filtergraph`) used by `redub-export`, and
    2. An in-memory RGBA buffer processor (`apply_to_rgba`) used by `redub-app` to update the Output preview panel as frames or filter settings change; processing cost depends on the active filters.
