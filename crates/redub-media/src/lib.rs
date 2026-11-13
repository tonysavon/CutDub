//! FFmpeg/ffprobe media integration for CutDub:
//! - Dynamic discovery of installed encoders and hardware accelerators
//! - Container and stream metadata inspection via `ffprobe`
//! - Exact packet/frame presentation timestamp & keyframe indexing (CFR & VFR)
//! - Clean frame-accurate decoding abstraction with keyframe-assisted seeking and burst prefetching

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use redub_core::{
    AudioStreamInfo, AudioWaveform, FrameEntry, FrameTimingMap, Rational, SourceClip,
    SubtitleStreamInfo, TimestampUs, WaveformPeak,
};
use redub_platform::{configure_background_command, FfmpegBinaries};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("Failed to execute media tool: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffprobe failed: {0}")]
    ProbeFailed(String),
    #[error("No video stream found in file '{0}'")]
    NoVideoStream(String),
    #[error("Failed to parse ffprobe output: {0}")]
    JsonParse(#[from] serde_json::Error),
    #[error("Frame decode failed: {0}")]
    DecodeFailed(String),
}

/// Information about an available FFmpeg encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderInfo {
    pub name: String,
    pub description: String,
    pub is_video: bool,
    pub is_hardware: bool,
}

/// Dynamically detected capabilities of the installed FFmpeg binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaCapabilities {
    pub video_encoders: Vec<EncoderInfo>,
    pub audio_encoders: Vec<EncoderInfo>,
    pub hwaccels: Vec<String>,
    pub encoder_names: HashSet<String>,
}

/// Curated priority order of video encoders (software and hardware across Apple VideoToolbox,
/// NVIDIA NVENC, AMD AMF, Intel QuickSync QSV, and Linux VA-API).
pub const PREFERRED_VIDEO_ENCODERS: &[&str] = &[
    // H.264 / AVC
    "libx264",
    "h264_videotoolbox",
    "h264_nvenc",
    "h264_amf",
    "h264_qsv",
    "h264_vaapi",
    // H.265 / HEVC
    "libx265",
    "hevc_videotoolbox",
    "hevc_nvenc",
    "hevc_amf",
    "hevc_qsv",
    "hevc_vaapi",
    // H.266 / VVC
    "libvvenc",
    // AV1
    "libsvtav1",
    "libaom-av1",
    "av1_nvenc",
    "av1_amf",
    "av1_qsv",
    "av1_vaapi",
    "av1_videotoolbox",
    // VP9
    "libvpx-vp9",
    "vp9_qsv",
    "vp9_vaapi",
    // ProRes / Archival / Legacy
    "prores_ks",
    "prores_videotoolbox",
    "ffv1",
    "mpeg4",
];

pub fn is_hardware_encoder_name(name: &str) -> bool {
    name.ends_with("_videotoolbox")
        || name.ends_with("_nvenc")
        || name.ends_with("_amf")
        || name.ends_with("_qsv")
        || name.ends_with("_vaapi")
        || name.ends_with("_mf")
        || name.ends_with("_d3d12va")
        || name.ends_with("_v4l2m2m")
        || name.ends_with("_omx")
        || name.ends_with("_rkmpp")
        || name.ends_with("_vulkan")
        || name.ends_with("_at")
}

/// Probes whether a hardware video encoder compiled into `ffmpeg` can actually initialize
/// and encode a 1-frame test surface on the current machine's GPU/APU.
///
/// Desktop FFmpeg builds (especially on Windows and Linux) compile in stubs for NVIDIA NVENC,
/// Intel QuickSync (QSV), AMD AMF, and VA-API simultaneously. Running a 1-frame null encode
/// filters out encoders whose vendor driver or hardware is absent on the current system.
pub fn probe_hardware_video_encoder(ffmpeg_path: &Path, encoder_name: &str) -> bool {
    let mut cmd = Command::new(ffmpeg_path);
    cmd.args(["-hide_banner", "-loglevel", "error"]);
    if encoder_name.ends_with("_vaapi") {
        cmd.args([
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30",
            "-vf",
            "format=nv12,hwupload",
            "-frames:v",
            "1",
            "-c:v",
            encoder_name,
            "-f",
            "null",
            "-",
        ]);
    } else {
        cmd.args([
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=256x256:r=30",
            "-frames:v",
            "1",
            "-c:v",
            encoder_name,
            "-f",
            "null",
            "-",
        ]);
    }
    configure_background_command(&mut cmd);
    cmd.output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

impl MediaCapabilities {
    /// Queries `ffmpeg -encoders` and `ffmpeg -hwaccels` to discover available codecs and
    /// verifies hardware video encoders against the actual host GPU/CPU hardware.
    pub fn detect(binaries: &FfmpegBinaries) -> Self {
        let mut caps = Self::default();

        // 1. Detect hwaccels
        let mut hw_cmd = Command::new(&binaries.ffmpeg_path);
        hw_cmd.args(["-hide_banner", "-hwaccels"]);
        configure_background_command(&mut hw_cmd);
        if let Some(output) = hw_cmd.output().ok().filter(|output| output.status.success()) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.ends_with(':') {
                    caps.hwaccels.push(trimmed.to_string());
                }
            }
        }

        // 2. Detect encoders compiled into FFmpeg
        let mut candidate_hw_video = Vec::new();
        let mut enc_cmd = Command::new(&binaries.ffmpeg_path);
        enc_cmd.args(["-hide_banner", "-encoders"]);
        configure_background_command(&mut enc_cmd);
        if let Some(output) = enc_cmd.output().ok().filter(|output| output.status.success()) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut in_table = false;
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("------") {
                    in_table = true;
                    continue;
                }
                if !in_table || trimmed.len() < 8 {
                    continue;
                }
                // Format: " V..... libx264              libx264 H.264 / AVC / MPEG-4 AVC / MPEG-4 part 10 (codec h264)"
                let mut parts = trimmed.splitn(3, char::is_whitespace);
                let Some(flags) = parts.next() else { continue };
                let rest = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim_start();
                let mut name_and_desc = rest.splitn(2, char::is_whitespace);
                let Some(name) = name_and_desc.next() else {
                    continue;
                };
                let desc = name_and_desc.next().unwrap_or("").trim().to_string();

                let is_video = flags.starts_with('V');
                let is_audio = flags.starts_with('A');
                if !is_video && !is_audio {
                    continue;
                }
                let is_hardware = is_hardware_encoder_name(name);

                let info = EncoderInfo {
                    name: name.to_string(),
                    description: desc,
                    is_video,
                    is_hardware,
                };
                if is_video {
                    if is_hardware {
                        // Only probe primary hardware encoders we expose (or vendor GPU families)
                        if PREFERRED_VIDEO_ENCODERS.contains(&name) {
                            candidate_hw_video.push(info);
                        }
                    } else {
                        caps.encoder_names.insert(name.to_string());
                        caps.video_encoders.push(info);
                    }
                } else {
                    caps.encoder_names.insert(name.to_string());
                    caps.audio_encoders.push(info);
                }
            }
        }

        // 2b. Probe candidate hardware video encoders in parallel to verify actual GPU/APU support
        if !candidate_hw_video.is_empty() {
            let ffmpeg_path = binaries.ffmpeg_path.as_path();
            let supported_hw: Vec<EncoderInfo> = std::thread::scope(|scope| {
                let handles: Vec<_> = candidate_hw_video
                    .into_iter()
                    .map(|info| {
                        scope.spawn(move || {
                            let ok = probe_hardware_video_encoder(ffmpeg_path, &info.name);
                            debug!(
                                encoder = %info.name,
                                available = ok,
                                "Probed hardware video encoder on host system"
                            );
                            ok.then_some(info)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .filter_map(|h| h.join().ok().flatten())
                    .collect()
            });

            for info in supported_hw {
                caps.encoder_names.insert(info.name.clone());
                caps.video_encoders.push(info);
            }
        }

        // Prune hwaccels whose corresponding vendor hardware encoders are all unavailable
        let has_nvenc = caps.encoder_names.iter().any(|n| n.ends_with("_nvenc"));
        let has_qsv = caps.encoder_names.iter().any(|n| n.ends_with("_qsv"));
        let has_vaapi = caps.encoder_names.iter().any(|n| n.ends_with("_vaapi"));
        let has_videotoolbox = caps
            .encoder_names
            .iter()
            .any(|n| n.ends_with("_videotoolbox"));
        caps.hwaccels.retain(|hw| match hw.as_str() {
            "cuda" | "nvdec" | "cuvid" => has_nvenc,
            "qsv" => has_qsv,
            "vaapi" => has_vaapi,
            "videotoolbox" => has_videotoolbox,
            _ => true,
        });

        // 3. Detect external VVenC (libvvenc / vvencapp) H.266/VVC encoder if installed on the system
        if binaries.vvencapp_path.as_deref().is_some_and(|path|
            redub_platform::query_version_flag(path, "--version").is_ok())
            && !caps.encoder_names.contains("libvvenc") {
            let desc = binaries
                .vvencapp_version
                .as_deref()
                .map(|v| format!("Fraunhofer VVenC H.266 / VVC ({v})"))
                .unwrap_or_else(|| "Fraunhofer VVenC H.266 / VVC Encoder (system libvvenc)".into());
            caps.encoder_names.insert("libvvenc".to_string());
            caps.video_encoders.push(EncoderInfo {
                name: "libvvenc".to_string(),
                description: desc,
                is_video: true,
                is_hardware: false,
            });
        }

        info!(
            video_encoders = caps.video_encoders.len(),
            audio_encoders = caps.audio_encoders.len(),
            hwaccels = ?caps.hwaccels,
            "Detected FFmpeg capabilities"
        );
        caps
    }

    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoder_names.contains(name)
    }

    /// Returns a curated list of primary video encoders verified to be available on this system,
    /// followed by any other relevant hardware/software encoders.
    pub fn preferred_video_encoders(&self) -> Vec<EncoderInfo> {
        let mut out = Vec::new();
        for &name in PREFERRED_VIDEO_ENCODERS {
            if let Some(enc) = self.video_encoders.iter().find(|e| e.name == name) {
                out.push(enc.clone());
            }
        }
        if out.is_empty() {
            out.extend(self.video_encoders.iter().take(15).cloned());
        }
        out
    }

    /// Returns a curated list of primary audio encoders present in this FFmpeg build.
    pub fn preferred_audio_encoders(&self) -> Vec<EncoderInfo> {
        let priority = [
            "aac",
            "aac_at",
            "libopus",
            "libmp3lame",
            "flac",
            "alac",
            "ac3",
            "eac3",
            "libvorbis",
            "pcm_s16le",
            "pcm_s24le",
        ];
        let mut out = Vec::new();
        for name in priority {
            if let Some(enc) = self.audio_encoders.iter().find(|e| e.name == name) {
                out.push(enc.clone());
            }
        }
        if out.is_empty() {
            out.extend(self.audio_encoders.iter().take(10).cloned());
        }
        out
    }
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    index: usize,
    codec_type: Option<String>,
    codec_name: Option<String>,
    pix_fmt: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    time_base: Option<String>,
    nb_frames: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    tags: Option<FfprobeTags>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    format_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeTags {
    language: Option<String>,
    title: Option<String>,
}

/// Probes a video file using `ffprobe` and returns a `SourceClip`.
/// If `scan_exact_packets` is true, also performs a fast packet scan to build an exact VFR/keyframe `FrameTimingMap`.
pub fn probe_media_file(
    binaries: &FfmpegBinaries,
    path: &Path,
    scan_exact_packets: bool,
) -> Result<SourceClip, MediaError> {
    let mut cmd = Command::new(&binaries.ffprobe_path);
    cmd.args(["-v", "quiet", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path);
    configure_background_command(&mut cmd);

    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MediaError::ProbeFailed(format!(
            "ffprobe exited with status {}: {}",
            output.status, stderr
        )));
    }

    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)?;

    let video_stream = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| MediaError::NoVideoStream(path.display().to_string()))?;

    let width = video_stream.width.unwrap_or(1920);
    let height = video_stream.height.unwrap_or(1080);
    let video_codec = video_stream
        .codec_name
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let pixel_format = video_stream
        .pix_fmt
        .clone()
        .unwrap_or_else(|| "yuv420p".to_string());

    let avg_fps = video_stream
        .avg_frame_rate
        .as_deref()
        .and_then(Rational::parse)
        .filter(|r| r.num > 0 && r.den > 0);
    let r_fps = video_stream
        .r_frame_rate
        .as_deref()
        .and_then(Rational::parse)
        .filter(|r| r.num > 0 && r.den > 0);
    let nominal_fps = avg_fps.or(r_fps).unwrap_or(Rational::new(30, 1));
    let time_base = video_stream
        .time_base
        .as_deref()
        .and_then(Rational::parse)
        .unwrap_or(Rational::new(1, 90000));

    let duration_secs = video_stream
        .duration
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            parsed
                .format
                .as_ref()
                .and_then(|f| f.duration.as_deref())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(1.0);

    let est_frames = video_stream
        .nb_frames
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            ((duration_secs * nominal_fps.as_f64()).round() as u64).max(1)
        });

    let timing = if scan_exact_packets {
        match probe_packet_timing_map(binaries, path, nominal_fps, time_base) {
            Ok(map) if map.total_frames() > 0 => map,
            _ => FrameTimingMap::from_cfr(est_frames, nominal_fps, time_base, 30),
        }
    } else {
        FrameTimingMap::from_cfr(est_frames, nominal_fps, time_base, 30)
    };

    let mut audio_streams = Vec::new();
    let mut subtitle_streams = Vec::new();

    for stream in &parsed.streams {
        match stream.codec_type.as_deref() {
            Some("audio") => {
                audio_streams.push(AudioStreamInfo {
                    stream_index: stream.index,
                    codec_name: stream
                        .codec_name
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    sample_rate: stream
                        .sample_rate
                        .as_deref()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(48_000),
                    channels: stream.channels.unwrap_or(2),
                    channel_layout: stream.channel_layout.clone(),
                    language: stream.tags.as_ref().and_then(|t| t.language.clone()),
                    title: stream.tags.as_ref().and_then(|t| t.title.clone()),
                    bit_rate: stream.bit_rate.as_deref().and_then(|s| s.parse().ok()),
                });
            }
            Some("subtitle") => {
                subtitle_streams.push(SubtitleStreamInfo {
                    stream_index: stream.index,
                    codec_name: stream
                        .codec_name
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    language: stream.tags.as_ref().and_then(|t| t.language.clone()),
                    title: stream.tags.as_ref().and_then(|t| t.title.clone()),
                });
            }
            _ => {}
        }
    }

    let container_format = parsed
        .format
        .as_ref()
        .and_then(|f| f.format_name.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let bit_rate = parsed
        .format
        .as_ref()
        .and_then(|f| f.bit_rate.as_deref())
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            video_stream
                .bit_rate
                .as_deref()
                .and_then(|s| s.parse().ok())
        });
    let file_size_bytes = parsed
        .format
        .as_ref()
        .and_then(|f| f.size.as_deref())
        .and_then(|s| s.parse().ok())
        .or_else(|| fs::metadata(path).ok().map(|m| m.len()))
        .unwrap_or(0);

    Ok(SourceClip {
        id: 0,
        path: path.to_path_buf(),
        container_format,
        video_stream_index: video_stream.index,
        video_codec,
        pixel_format,
        width,
        height,
        bit_rate,
        file_size_bytes,
        timing,
        audio_streams,
        subtitle_streams,
        waveform: None,
    })
}

/// Extracts a normalized peak + RMS audio waveform envelope from an audio stream using FFmpeg.
pub fn extract_audio_waveform(
    binaries: &FfmpegBinaries,
    path: &Path,
    stream_index: usize,
    num_buckets: usize,
) -> Result<AudioWaveform, MediaError> {
    let buckets = num_buckets.clamp(32, 4096);
    let sample_rate = 4000u32;

    let mut cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut cmd);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-i",
    ])
    .arg(path)
    .args([
        "-map",
        &format!("0:{stream_index}"),
        "-ac",
        "1",
        "-ar",
        &sample_rate.to_string(),
        "-f",
        "s16le",
        "pipe:1",
    ]);

    let output = cmd.output()?;
    if !output.status.success() || output.stdout.len() < 2 {
        return Err(MediaError::DecodeFailed(
            "Failed to decode audio samples for waveform".into(),
        ));
    }

    let raw_bytes = &output.stdout;
    let total_samples = raw_bytes.len() / 2;
    let samples_per_bucket = (total_samples / buckets).max(1);
    let mut peaks = Vec::with_capacity(buckets);

    for b in 0..buckets {
        let start_s = (b * total_samples) / buckets;
        let end_s = (((b + 1) * total_samples) / buckets)
            .max(start_s + 1)
            .min(total_samples);
        let mut min_v = 0.0f32;
        let mut max_v = 0.0f32;
        let mut sum_sq = 0.0f64;
        let mut count = 0usize;

        for s_idx in start_s..end_s {
            let byte_idx = s_idx * 2;
            if byte_idx + 1 >= raw_bytes.len() {
                break;
            }
            let sample = i16::from_le_bytes([raw_bytes[byte_idx], raw_bytes[byte_idx + 1]]);
            let norm = (sample as f32 / 32768.0).clamp(-1.0, 1.0);
            if norm < min_v {
                min_v = norm;
            }
            if norm > max_v {
                max_v = norm;
            }
            sum_sq += (norm as f64) * (norm as f64);
            count += 1;
        }

        let rms = if count > 0 {
            (sum_sq / count as f64).sqrt() as f32
        } else {
            0.0
        };
        peaks.push(WaveformPeak {
            min: min_v,
            max: max_v,
            rms: rms.clamp(0.0, 1.0),
        });
    }

    let _ = samples_per_bucket;
    Ok(AudioWaveform {
        stream_index,
        sample_rate,
        peaks,
    })
}

/// Scans video packet headers via `ffprobe` (without decoding video frames) to build an exact
/// per-frame `FrameTimingMap` with real PTS values and keyframe flags.
pub fn probe_packet_timing_map(
    binaries: &FfmpegBinaries,
    path: &Path,
    nominal_fps: Rational,
    time_base: Rational,
) -> Result<FrameTimingMap, MediaError> {
    let mut cmd = Command::new(&binaries.ffprobe_path);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=pts_time,dts_time,duration_time,flags",
        "-of",
        "csv=p=0",
    ])
    .arg(path);
    configure_background_command(&mut cmd);

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(MediaError::ProbeFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let default_dur = nominal_fps.frame_duration_us();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // CSV fields: pts_time,dts_time,duration_time,flags
        let parts: Vec<&str> = line.split(',').collect();
        if parts.is_empty() {
            continue;
        }
        let pts_opt = parts.first().and_then(|s| s.parse::<f64>().ok());
        let dts_opt = parts.get(1).and_then(|s| s.parse::<f64>().ok());
        let Some(time_secs) = pts_opt.or(dts_opt) else {
            continue;
        };
        let dur_us = parts
            .get(2)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|d| (d * 1_000_000.0).round() as i64)
            .unwrap_or(default_dur);
        let flags = parts.get(3).copied().unwrap_or("");
        let is_keyframe = flags.contains('K');

        entries.push(FrameEntry {
            frame_number: 0,
            pts_us: TimestampUs::from_seconds_f64(time_secs.max(0.0)),
            duration_us: dur_us.max(1),
            is_keyframe,
        });
    }

    if entries.is_empty() {
        return Err(MediaError::ProbeFailed("No video packets found".into()));
    }

    // Normalize start PTS so timeline starts at 0 if stream has a non-zero initial PTS offset
    entries.sort_by_key(|e| e.pts_us);
    if let Some(first_pts) = entries.first().map(|e| e.pts_us.0) {
        if first_pts > 0 && first_pts < 10_000_000 {
            for e in &mut entries {
                e.pts_us = TimestampUs((e.pts_us.0 - first_pts).max(0));
            }
        }
    }

    debug!(
        path = %path.display(),
        frames = entries.len(),
        "Built exact packet timing map"
    );
    Ok(FrameTimingMap::from_entries(entries, nominal_fps, time_base))
}

/// Fast local-window keyframe scanner using `ffprobe -read_intervals`.
/// Scans packet headers only within `[center_ts - radius_secs, center_ts + radius_secs]`
/// so multi-gigabyte files on external drives get exact keyframe indices around the playhead in ~15ms.
pub fn probe_local_keyframes(
    binaries: &FfmpegBinaries,
    path: &Path,
    center_ts: TimestampUs,
    radius_secs: f64,
) -> Result<(TimestampUs, TimestampUs, Vec<TimestampUs>), MediaError> {
    let center_s = center_ts.as_seconds_f64();
    let start_s = (center_s - radius_secs).max(0.0);
    let span_s = (radius_secs * 2.0).max(10.0);
    let interval_arg = format!("{start_s:.3}%+{span_s:.3}");

    let mut cmd = Command::new(&binaries.ffprobe_path);
    configure_background_command(&mut cmd);
    cmd.args([
        "-v",
        "error",
        "-read_intervals",
        &interval_arg,
        "-select_streams",
        "v:0",
        "-show_entries",
        "packet=pts_time,dts_time,flags",
        "-of",
        "csv=p=0",
    ])
    .arg(path);

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(MediaError::ProbeFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut keyframes = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.trim().split(',').collect();
        if parts.len() < 3 {
            continue;
        }
        let flags = parts[2];
        if !flags.contains('K') {
            continue;
        }
        let pts_opt = parts[0].parse::<f64>().ok();
        let dts_opt = parts[1].parse::<f64>().ok();
        if let Some(sec) = pts_opt.or(dts_opt) {
            keyframes.push(TimestampUs::from_seconds_f64(sec.max(0.0)));
        }
    }

    keyframes.sort();
    keyframes.dedup();
    let w_start = TimestampUs::from_seconds_f64(start_s);
    let w_end = TimestampUs::from_seconds_f64(start_s + span_s);
    Ok((w_start, w_end, keyframes))
}

/// Saves a full-resolution PNG or JPEG snapshot of a single video frame (optionally with an FFmpeg `-vf` filtergraph).
pub fn export_frame_snapshot(
    binaries: &FfmpegBinaries,
    clip: &SourceClip,
    source_frame: u64,
    vf_filtergraph: Option<&str>,
    output_path: &Path,
) -> Result<(), MediaError> {
    let clamped = source_frame.min(clip.timing.total_frames().saturating_sub(1));
    let target_pts = clip.timing.frame_to_timestamp(clamped);

    let mut cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut cmd);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]);
    if target_pts.0 > 0 {
        cmd.arg("-ss")
            .arg(format!("{:.6}", target_pts.as_seconds_f64()));
    }
    cmd.arg("-i").arg(&clip.path);
    cmd.args(["-map", &format!("0:{}", clip.video_stream_index)]);
    if let Some(vf) = vf_filtergraph {
        if !vf.is_empty() {
            cmd.args(["-vf", vf]);
        }
    }
    cmd.args(["-frames:v", "1"]);
    cmd.arg(output_path);

    let out = cmd.output()?;
    if !out.status.success() {
        return Err(MediaError::DecodeFailed(
            String::from_utf8_lossy(&out.stderr).to_string(),
        ));
    }
    Ok(())
}

/// Type of event detected by the Scene Change / Black-Frame analyzer.
#[derive(Debug, Clone, PartialEq)]
pub enum SceneMarkerKind {
    SceneCut { score: f32 },
    BlackSegment { duration_secs: f32 },
}

/// A scene cut or black-frame interval detected by FFmpeg's `scdet` and `blackdetect` filters.
#[derive(Debug, Clone)]
pub struct DetectedSceneMarker {
    pub timestamp: TimestampUs,
    pub kind: SceneMarkerKind,
}

/// Scans a time range `[start_pts, end_pts]` of `path` using FFmpeg's `blackdetect` and `scdet`
/// filters on a fast `160x90` proxy stream to locate scene cuts and black intervals.
pub fn detect_scenes_and_black_frames(
    binaries: &FfmpegBinaries,
    path: &Path,
    video_stream_index: usize,
    start_pts: TimestampUs,
    duration_secs: f64,
    scene_threshold: f32,
    min_black_duration_secs: f32,
) -> Result<Vec<DetectedSceneMarker>, MediaError> {
    let start_secs = start_pts.as_seconds_f64();
    let thresh = scene_threshold.clamp(3.0, 50.0);
    let black_d = min_black_duration_secs.clamp(0.05, 5.0);
    let vf = format!(
        "scale=160:90:flags=bilinear,blackdetect=d={black_d:.2}:pix_th=0.10,scdet=t={thresh:.1}"
    );

    let mut cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut cmd);
    cmd.args([
        "-hide_banner",
        "-nostdin",
        "-threads",
        "0",
        "-thread_type",
        "frame+slice",
    ]);
    if start_secs > 0.001 {
        cmd.arg("-ss").arg(format!("{start_secs:.6}"));
    }
    cmd.arg("-i").arg(path);
    cmd.arg("-t").arg(format!("{:.3}", duration_secs.max(0.5)));
    cmd.args([
        "-map",
        &format!("0:{video_stream_index}"),
        "-vf",
        &vf,
        "-an",
        "-sn",
        "-f",
        "null",
        "-",
    ]);

    let out = cmd.output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut markers = Vec::new();

    for line in stderr.lines() {
        if line.contains("black_start:") {
            let mut b_start: Option<f64> = None;
            let mut b_dur: Option<f32> = None;
            for token in line.split_whitespace() {
                if let Some(v) = token.strip_prefix("black_start:") {
                    b_start = v.parse::<f64>().ok();
                } else if let Some(v) = token.strip_prefix("black_duration:") {
                    b_dur = v.parse::<f32>().ok();
                }
            }
            if let (Some(st), Some(dur)) = (b_start, b_dur) {
                let abs_us = start_pts.0 + (st * 1_000_000.0).round() as i64;
                markers.push(DetectedSceneMarker {
                    timestamp: TimestampUs(abs_us.max(0)),
                    kind: SceneMarkerKind::BlackSegment { duration_secs: dur },
                });
            }
        } else if line.contains("lavfi.scd.time:") {
            let mut score: f32 = thresh;
            let mut sc_time: Option<f64> = None;
            for part in line.split(',') {
                let part = part.trim();
                if let Some(idx) = part.find("lavfi.scd.score:") {
                    let s = part[idx + "lavfi.scd.score:".len()..].trim();
                    if let Ok(val) = s.parse::<f32>() {
                        score = val;
                    }
                } else if let Some(idx) = part.find("lavfi.scd.time:") {
                    let t = part[idx + "lavfi.scd.time:".len()..].trim();
                    sc_time = t.parse::<f64>().ok();
                }
            }
            if let Some(st) = sc_time {
                let abs_us = start_pts.0 + (st * 1_000_000.0).round() as i64;
                markers.push(DetectedSceneMarker {
                    timestamp: TimestampUs(abs_us.max(0)),
                    kind: SceneMarkerKind::SceneCut { score },
                });
            }
        }
    }

    markers.sort_by_key(|m| m.timestamp.0);
    Ok(markers)
}

/// Decoded RGBA8 video frame ready for preview caching or display.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub source_id: usize,
    pub source_frame: u64,
    pub pts_us: TimestampUs,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// Abstraction for frame-accurate video decoding, isolating the underlying FFmpeg mechanism.
pub trait VideoFrameDecoder: Send + Sync {
    /// Decodes a contiguous burst of frames starting at `start_frame` (up to `count` frames)
    /// from `clip`, scaling down only if dimensions exceed `max_dimension`.
    fn decode_burst(
        &self,
        clip: &SourceClip,
        start_frame: u64,
        count: usize,
        max_dimension: Option<u32>,
    ) -> Result<Vec<DecodedFrame>, MediaError>;
}

/// A persistent, streaming FFmpeg decoder process that yields consecutive RGBA frames
/// one by one without re-opening the media file or re-seeking between bursts.
pub struct ContinuousFrameStream {
    child: Arc<Mutex<Child>>,
    stdout: ChildStdout,
    pub clip: Arc<SourceClip>,
    pub next_frame: u64,
    pub out_w: u32,
    pub out_h: u32,
}

impl ContinuousFrameStream {
    /// Reads the next single decoded RGBA frame from the running FFmpeg pipe.
    pub fn read_next_frame(&mut self) -> Result<Option<DecodedFrame>, MediaError> {
        let total = self.clip.timing.total_frames();
        if self.next_frame >= total {
            return Ok(None);
        }
        let frame_byte_len = (self.out_w as usize) * (self.out_h as usize) * 4;
        let mut buf = vec![0u8; frame_byte_len];
        match self.stdout.read_exact(&mut buf) {
            Ok(()) => {
                let f_num = self.next_frame;
                self.next_frame += 1;
                let pts_us = self.clip.timing.frame_to_timestamp(f_num);
                Ok(Some(DecodedFrame {
                    source_id: self.clip.id,
                    source_frame: f_num,
                    pts_us,
                    width: self.out_w,
                    height: self.out_h,
                    rgba: Arc::new(buf),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(MediaError::Io(e)),
        }
    }

    /// A handle that can kill the process while the decoder is blocked reading stdout.
    pub fn cancellation_handle(&self) -> Arc<Mutex<Child>> {
        Arc::clone(&self.child)
    }

    pub fn stop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ContinuousFrameStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Process-based FFmpeg frame decoder using keyframe-assisted seeking and raw RGBA pipe streaming.
#[derive(Debug, Clone)]
pub struct FfmpegProcessDecoder {
    pub binaries: FfmpegBinaries,
    pub prefer_hwaccel: bool,
    pub hwaccel_backend: String,
}

impl FfmpegProcessDecoder {
    pub fn new(binaries: FfmpegBinaries, prefer_hwaccel: bool) -> Self {
        Self {
            binaries,
            prefer_hwaccel,
            hwaccel_backend: "auto".to_string(),
        }
    }

    pub fn with_hwaccel_backend(mut self, backend: impl Into<String>) -> Self {
        self.hwaccel_backend = backend.into();
        self
    }

    /// Spawns a persistent continuous FFmpeg frame stream starting at `start_frame`.
    /// When `max_frames` is `None`, the stream decodes continuously until EOF or until stopped.
    pub fn spawn_stream(
        &self,
        clip: Arc<SourceClip>,
        start_frame: u64,
        max_frames: Option<usize>,
        max_dimension: Option<u32>,
        use_hwaccel: bool,
    ) -> Result<ContinuousFrameStream, MediaError> {
        let total = clip.timing.total_frames().max(1);
        let clamped_start = start_frame.min(total - 1);
        let (out_w, out_h) =
            Self::compute_preview_dimensions(clip.width, clip.height, max_dimension);
        let target_pts = clip.timing.frame_to_timestamp(clamped_start);

        let mut cmd = Command::new(&self.binaries.ffmpeg_path);
        configure_background_command(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);

        if use_hwaccel && self.hwaccel_backend != "none" {
            cmd.args(["-hwaccel", &self.hwaccel_backend]);
        }

        // Single-stage frame-accurate input seek: FFmpeg seeks the demuxer to the container's
        // actual preceding keyframe and decodes forward internally to the exact timestamp.
        if target_pts.0 > 0 {
            cmd.arg("-ss")
                .arg(format!("{:.6}", target_pts.as_seconds_f64()));
        }

        cmd.arg("-i").arg(&clip.path);
        cmd.args(["-map", &format!("0:{}", clip.video_stream_index)]);

        if out_w != clip.width || out_h != clip.height {
            cmd.args(["-vf", &format!("scale={out_w}:{out_h}:flags=fast_bilinear")]);
        }

        if let Some(limit) = max_frames {
            cmd.args(["-frames:v", &limit.to_string()]);
        }

        cmd.args([
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MediaError::DecodeFailed("Missing stdout pipe".into()))?;

        Ok(ContinuousFrameStream {
            child: Arc::new(Mutex::new(child)),
            stdout,
            clip,
            next_frame: clamped_start,
            out_w,
            out_h,
        })
    }

    /// Decodes a single frame through an exact FFmpeg `-vf` filtergraph expression.
    pub fn decode_filtered_frame(
        &self,
        clip: &SourceClip,
        source_frame: u64,
        vf_filtergraph: &str,
        out_w: u32,
        out_h: u32,
    ) -> Result<DecodedFrame, MediaError> {
        let total = clip.timing.total_frames().max(1);
        let clamped = source_frame.min(total - 1);
        let kf_frame = clip.timing.keyframe_at_or_before(clamped);
        let kf_pts = clip.timing.frame_to_timestamp(kf_frame);
        let target_pts = clip.timing.frame_to_timestamp(clamped);
        let fine_offset_secs = ((target_pts.0 - kf_pts.0 - 1_000).max(0)) as f64 / 1_000_000.0;

        let mut cmd = Command::new(&self.binaries.ffmpeg_path);
        configure_background_command(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if kf_pts.0 > 0 {
            cmd.arg("-ss")
                .arg(format!("{:.6}", kf_pts.as_seconds_f64()));
        }
        cmd.arg("-i").arg(&clip.path);
        if fine_offset_secs > 0.001 {
            cmd.arg("-ss").arg(format!("{fine_offset_secs:.6}"));
        }
        cmd.args([
            "-map",
            &format!("0:{}", clip.video_stream_index),
            "-vf",
            vf_filtergraph,
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ]);
        let output = cmd.output()?;
        let expected_bytes = (out_w as usize) * (out_h as usize) * 4;
        if output.stdout.len() < expected_bytes {
            return Err(MediaError::DecodeFailed(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(DecodedFrame {
            source_id: clip.id,
            source_frame: clamped,
            pts_us: target_pts,
            width: out_w,
            height: out_h,
            rgba: Arc::new(output.stdout[..expected_bytes].to_vec()),
        })
    }

    fn compute_preview_dimensions(src_w: u32, src_h: u32, max_dim: Option<u32>) -> (u32, u32) {
        let Some(limit) = max_dim else {
            return (src_w, src_h);
        };
        if src_w <= limit && src_h <= limit {
            return (src_w, src_h);
        }
        let scale = (limit as f64 / src_w.max(src_h) as f64).min(1.0);
        let w = (((src_w as f64 * scale).round() as u32).max(2)) & !1;
        let h = (((src_h as f64 * scale).round() as u32).max(2)) & !1;
        (w, h)
    }

    fn run_decode_command(
        &self,
        clip: &SourceClip,
        start_frame: u64,
        count: usize,
        out_w: u32,
        out_h: u32,
        use_hwaccel: bool,
    ) -> Result<Vec<DecodedFrame>, MediaError> {
        let total = clip.timing.total_frames();
        if total == 0 {
            return Ok(Vec::new());
        }
        let clamped_start = start_frame.min(total.saturating_sub(1));
        let max_count = (total - clamped_start) as usize;
        let actual_count = count.min(max_count).max(1);

        let target_pts = clip.timing.frame_to_timestamp(clamped_start);

        let mut cmd = Command::new(&self.binaries.ffmpeg_path);
        configure_background_command(&mut cmd);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);

        if use_hwaccel && self.hwaccel_backend != "none" {
            cmd.args(["-hwaccel", &self.hwaccel_backend]);
        }

        if target_pts.0 > 0 {
            cmd.arg("-ss")
                .arg(format!("{:.6}", target_pts.as_seconds_f64()));
        }

        cmd.arg("-i").arg(&clip.path);

        cmd.args(["-map", &format!("0:{}", clip.video_stream_index)]);

        if out_w != clip.width || out_h != clip.height {
            cmd.args(["-vf", &format!("scale={out_w}:{out_h}:flags=fast_bilinear")]);
        }

        cmd.args([
            "-frames:v",
            &actual_count.to_string(),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "pipe:1",
        ]);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let frame_byte_len = (out_w as usize) * (out_h as usize) * 4;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| MediaError::DecodeFailed("Missing stdout pipe".into()))?;

        let mut decoded = Vec::with_capacity(actual_count);
        for i in 0..actual_count {
            let mut buf = vec![0u8; frame_byte_len];
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    let f_num = clamped_start + i as u64;
                    let pts_us = clip.timing.frame_to_timestamp(f_num);
                    decoded.push(DecodedFrame {
                        source_id: clip.id,
                        source_frame: f_num,
                        pts_us,
                        width: out_w,
                        height: out_h,
                        rgba: Arc::new(buf),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Err(e) => return Err(MediaError::Io(e)),
            }
        }

        let status = child.wait()?;
        if decoded.is_empty() && !status.success() {
            return Err(MediaError::DecodeFailed(format!(
                "FFmpeg decode exited with {status} at frame {clamped_start}"
            )));
        }

        Ok(decoded)
    }
}

impl VideoFrameDecoder for FfmpegProcessDecoder {
    fn decode_burst(
        &self,
        clip: &SourceClip,
        start_frame: u64,
        count: usize,
        max_dimension: Option<u32>,
    ) -> Result<Vec<DecodedFrame>, MediaError> {
        let (out_w, out_h) =
            Self::compute_preview_dimensions(clip.width, clip.height, max_dimension);

        if self.prefer_hwaccel {
            match self.run_decode_command(clip, start_frame, count, out_w, out_h, true) {
                Ok(frames) if !frames.is_empty() => return Ok(frames),
                Ok(_) | Err(_) => {
                    warn!("Hardware decode attempt returned no frames, falling back to software decoding");
                }
            }
        }

        self.run_decode_command(clip, start_frame, count, out_w, out_h, false)
    }
}

/// Generates a synthetic test video file using FFmpeg's `testsrc2` and `sine` filters.
/// Useful for automated tests and quick verification.
pub fn generate_synthetic_video(
    binaries: &FfmpegBinaries,
    output_path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    duration_secs: f32,
    keyframe_interval: u32,
) -> Result<(), MediaError> {
    let mut cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut cmd);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        &format!("testsrc2=size={width}x{height}:rate={fps}:duration={duration_secs}"),
        "-f",
        "lavfi",
        "-i",
        &format!("sine=frequency=440:sample_rate=48000:duration={duration_secs}"),
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-preset",
        "ultrafast",
        "-g",
        &keyframe_interval.to_string(),
        "-keyint_min",
        &keyframe_interval.to_string(),
        "-sc_threshold",
        "0",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
    ])
    .arg(output_path);

    let out = cmd.output()?;
    if !out.status.success() {
        return Err(MediaError::ProbeFailed(
            String::from_utf8_lossy(&out.stderr).to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[test]
    fn missing_or_broken_external_vvc_is_not_advertised() {
        // Use installed executables rather than newly written scripts: Linux can
        // reject a just-written executable during parallel subprocess tests.
        let mut bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
        bins.vvencapp_path = None;
        let native_vvc = MediaCapabilities::detect(&bins).has_encoder("libvvenc");
        bins.vvencapp_path = Some("/bin/false".into());
        let broken = MediaCapabilities::detect(&bins);
        assert!(!broken.video_encoders.is_empty());
        assert_eq!(broken.has_encoder("libvvenc"), native_vvc);
        // echo provides a successful version-command stand-in, without a VVC dependency.
        bins.vvencapp_path = Some("/bin/echo".into());
        assert!(MediaCapabilities::detect(&bins).has_encoder("libvvenc"));
        bins.vvencapp_path = None;
        assert_eq!(MediaCapabilities::detect(&bins).has_encoder("libvvenc"), native_vvc);
    }

    #[test]
    fn test_capabilities_probe_and_decode() {
        let bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
        let caps = MediaCapabilities::detect(&bins);
        assert!(!caps.video_encoders.is_empty());
        assert!(!caps.audio_encoders.is_empty());

        let tmp = TempDir::new().unwrap();
        let video_path = tmp.path().join("test_clip.mp4");
        generate_synthetic_video(&bins, &video_path, 320, 240, 25, 2.0, 10)
            .expect("Failed to generate synthetic test video");

        let clip = probe_media_file(&bins, &video_path, true).expect("Failed to probe video");
        assert_eq!(clip.width, 320);
        assert_eq!(clip.height, 240);
        assert_eq!(clip.timing.total_frames(), 50);
        assert_eq!(clip.audio_streams.len(), 1);
        assert!(clip.timing.keyframe_count() >= 5);

        let wf = extract_audio_waveform(&bins, &video_path, clip.audio_streams[0].stream_index, 64)
            .expect("Failed to extract audio waveform");
        assert_eq!(wf.peaks.len(), 64);
        assert!(wf.peaks.iter().any(|p| p.rms > 0.05));

        let decoder = FfmpegProcessDecoder::new(bins, false);
        let frames = decoder
            .decode_burst(&clip, 12, 4, None)
            .expect("Failed to decode frame burst");
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].source_frame, 12);
        assert_eq!(frames[0].rgba.len(), 320 * 240 * 4);

        let filtered = decoder
            .decode_filtered_frame(&clip, 5, "scale=160:120", 160, 120)
            .expect("Failed to decode filtered frame");
        assert_eq!(filtered.width, 160);
        assert_eq!(filtered.height, 120);
    }

    #[test]
    fn preferred_video_encoders_includes_amd_amf_and_verified_hw_only() {
        let mut caps = MediaCapabilities::default();
        for (name, desc, is_hardware) in [
            ("libx264", "libx264 H.264", false),
            ("h264_amf", "AMD AMF H.264 Encoder", true),
            ("libx265", "libx265 H.265", false),
            ("hevc_amf", "AMD AMF HEVC encoder", true),
            ("av1_amf", "AMD AMF AV1 encoder", true),
        ] {
            caps.encoder_names.insert(name.to_string());
            caps.video_encoders.push(EncoderInfo {
                name: name.to_string(),
                description: desc.to_string(),
                is_video: true,
                is_hardware,
            });
        }

        let preferred: Vec<String> = caps
            .preferred_video_encoders()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(preferred.contains(&"h264_amf".to_string()));
        assert!(preferred.contains(&"hevc_amf".to_string()));
        assert!(preferred.contains(&"av1_amf".to_string()));
        assert!(!preferred.contains(&"h264_nvenc".to_string()));
        assert!(!preferred.contains(&"h264_qsv".to_string()));

        let bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
        // Non-existent / unsupported hardware encoder must return false on runtime probe
        assert!(!probe_hardware_video_encoder(
            &bins.ffmpeg_path,
            "nonexistent_hw_encoder"
        ));
    }
}
