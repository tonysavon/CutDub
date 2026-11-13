//! Asynchronous video export, remuxing (Direct Stream Copy), and full processing
//! re-encoding engine for CutDub with real-time progress reporting and cancellation.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use redub_core::{AudioExportMode, EditSegment, Project, TimestampUs};
use redub_filters::FilterPipeline;
use redub_media::{probe_media_file, MediaCapabilities};
use redub_platform::{configure_background_command, FfmpegBinaries};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("No video segments to export")]
    EmptyTimeline,
    #[error("I/O error during export: {0}")]
    Io(#[from] std::io::Error),
    #[error("Export cancelled by user")]
    Cancelled,
    #[error("FFmpeg export failed: {0}")]
    FfmpegFailed(String),
}

/// Supported target container formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerFormat {
    Mp4,
    Mkv,
    Mov,
    Avi,
    Webm,
}

impl ContainerFormat {
    pub const ALL: [Self; 5] = [Self::Mp4, Self::Mkv, Self::Mov, Self::Avi, Self::Webm];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mov => "mov",
            Self::Avi => "avi",
            Self::Webm => "webm",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mp4 => "MP4 (MPEG-4 Part 14)",
            Self::Mkv => "MKV (Matroska)",
            Self::Mov => "MOV (QuickTime)",
            Self::Avi => "AVI (Audio Video Interleave)",
            Self::Webm => "WebM",
        }
    }
}

/// Video processing mode (VirtualDub2 style).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoProcessingMode {
    /// Lossless packet remuxing without decoding/re-encoding (`-c:v copy`).
    DirectStreamCopy,
    /// Frame-accurate hybrid Smart Rendering: re-encodes only the partial GOP at non-keyframe
    /// cut boundaries and stream-copies the remainder of the video untouched.
    SmartCopy,
    /// Full decoding, filtering, frame-exact cutting, and re-encoding.
    FullProcessing,
}

/// How Direct Stream Copy handles non-keyframe start points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyframeCutPolicy {
    /// Starts at the nearest preceding keyframe so all frames decode cleanly without corruption.
    SnapToPrecedingKeyframe,
    /// Requests the exact start timestamp from the muxer (may drop or corrupt initial inter-frames depending on player).
    ExactTimestamp,
}

/// Global audio processing mode in ExportSettings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioProcessingMode {
    /// Losslessly stream-copy enabled audio tracks (or follow per-track overrides).
    #[default]
    DirectStreamCopy,
    /// Re-encode enabled audio tracks using the configured audio encoder, bitrate, sample rate, channels, and filters.
    FullProcessing,
    /// Strip all audio tracks from the exported file (`-an`).
    NoAudio,
}

/// Target audio channel layout when re-encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioChannelMode {
    #[default]
    Original,
    Mono,
    Stereo,
    Surround51,
}

impl AudioChannelMode {
    pub const ALL: [Self; 4] = [Self::Original, Self::Mono, Self::Stereo, Self::Surround51];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Keep Source Channels",
            Self::Mono => "Mono (1.0)",
            Self::Stereo => "Stereo (2.0)",
            Self::Surround51 => "5.1 Surround (6ch)",
        }
    }

    pub fn channel_count(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::Mono => Some(1),
            Self::Stereo => Some(2),
            Self::Surround51 => Some(6),
        }
    }
}

/// Rate control mode when re-encoding video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VideoRateControlMode {
    /// Constant Quality / Rate Factor (CRF / QP).
    #[default]
    Crf,
    /// Target Average Video Bitrate in kbps.
    TargetBitrateKbps,
    /// Target Total Output File Size in MB (automatically calculates video bitrate from duration & audio tracks).
    TargetFileSizeMb,
}

fn default_target_video_bitrate_kbps() -> u32 {
    4000
}

fn default_target_file_size_mb() -> f32 {
    25.0
}

fn default_audio_encoder() -> String {
    "aac".to_string()
}

fn default_audio_bitrate_kbps() -> u32 {
    192
}

/// Returns true if the audio encoder is lossless or uncompressed PCM (does not use a target CBR/VBR bitrate).
pub fn is_lossless_or_pcm_audio_encoder(codec: &str) -> bool {
    matches!(
        codec,
        "flac" | "alac" | "alac_at" | "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le"
    )
}

/// Detailed audio bitrate profile tailored specifically to the chosen audio encoder,
/// sample rate, and channel configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBitrateProfile {
    /// True if the codec is lossless or uncompressed PCM (bitrate not applicable).
    pub is_lossless: bool,
    /// Absolute minimum valid/allowed bitrate in kbps for this configuration.
    pub min_kbps: u32,
    /// Absolute maximum valid/allowed bitrate in kbps for this configuration.
    pub max_kbps: u32,
    /// Recommended default bitrate in kbps.
    pub default_kbps: u32,
    /// Sensible discrete preset bitrates to display as quick-select buttons in the UI.
    pub presets: Vec<u32>,
    /// Optional advisory warning or notice regarding standard limitations.
    pub notice: Option<&'static str>,
}

/// Computes the valid bitrate range, recommended default, presets, and specification notices
/// for a specific audio codec, target/source sample rate, and channel count.
pub fn audio_bitrate_profile(
    codec: &str,
    sample_rate_hz: Option<u32>,
    channel_count: u32,
) -> AudioBitrateProfile {
    if is_lossless_or_pcm_audio_encoder(codec) {
        return AudioBitrateProfile {
            is_lossless: true,
            min_kbps: 0,
            max_kbps: 0,
            default_kbps: 0,
            presets: Vec::new(),
            notice: None,
        };
    }

    let channels = channel_count.max(1);
    let sr = sample_rate_hz.unwrap_or(48_000);

    match codec {
        "libmp3lame" | "mp3" => {
            let mut notice = None;
            if channels > 2 {
                notice = Some("MP3 standard only supports Mono or Stereo (1 or 2 channels). Audio will be downmixed.");
            }
            if sr >= 96_000 {
                notice = Some("MP3 maximum standard sample rate is 48 kHz. Audio will be resampled.");
            }

            // MPEG-2 Layer 3 applies for low sample rates (<= 24000 Hz)
            if sr <= 24_000 {
                if channels == 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 16,
                        max_kbps: 96,
                        default_kbps: 48,
                        presets: vec![24, 32, 48, 64, 80, 96],
                        notice,
                    }
                } else {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 32,
                        max_kbps: 160,
                        default_kbps: 96,
                        presets: vec![48, 64, 96, 128, 160],
                        notice,
                    }
                }
            } else {
                // MPEG-1 Layer 3 (32000, 44100, 48000 Hz)
                if channels == 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 32,
                        max_kbps: 192,
                        default_kbps: 96,
                        presets: vec![48, 64, 96, 128, 160, 192],
                        notice,
                    }
                } else {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 64,
                        max_kbps: 320,
                        default_kbps: 192,
                        presets: vec![96, 128, 160, 192, 224, 256, 320],
                        notice,
                    }
                }
            }
        }
        "ac3" => {
            let mut notice = None;
            if sr != 32_000 && sr != 44_100 && sr != 48_000 {
                notice = Some("ATSC A/52 standard for AC-3 strictly requires 32 kHz, 44.1 kHz, or 48 kHz.");
            }
            if channels > 6 {
                notice = Some("Standard AC-3 supports up to 5.1 channels. For 7.1, choose E-AC-3 (eac3).");
            }

            if channels <= 1 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 48,
                    max_kbps: 192,
                    default_kbps: 96,
                    presets: vec![64, 96, 128, 160, 192],
                    notice,
                }
            } else if channels <= 2 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 96,
                    max_kbps: 448,
                    default_kbps: 192,
                    presets: vec![128, 160, 192, 224, 256, 320, 384, 448],
                    notice,
                }
            } else if channels <= 5 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 192,
                    max_kbps: 448,
                    default_kbps: 320,
                    presets: vec![192, 256, 320, 384, 448],
                    notice,
                }
            } else {
                // 5.1 Surround (6ch) or clamped
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 256,
                    max_kbps: 640,
                    default_kbps: 448,
                    presets: vec![320, 384, 448, 512, 576, 640],
                    notice,
                }
            }
        }
        "eac3" => {
            let mut notice = None;
            if sr != 32_000 && sr != 44_100 && sr != 48_000 {
                notice = Some("Dolby Digital Plus (E-AC-3) standard recommends 32 kHz, 44.1 kHz, or 48 kHz.");
            }

            if channels <= 1 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 32,
                    max_kbps: 256,
                    default_kbps: 96,
                    presets: vec![48, 64, 96, 128, 192],
                    notice,
                }
            } else if channels <= 2 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 96,
                    max_kbps: 512,
                    default_kbps: 224,
                    presets: vec![128, 160, 192, 224, 256, 320, 384],
                    notice,
                }
            } else if channels <= 6 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 256,
                    max_kbps: 1024,
                    default_kbps: 448,
                    presets: vec![320, 384, 448, 512, 640, 768],
                    notice,
                }
            } else {
                // 7.1 Surround (8ch)
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 384,
                    max_kbps: 1536,
                    default_kbps: 640,
                    presets: vec![448, 512, 640, 768, 1024, 1536],
                    notice,
                }
            }
        }
        "libopus" | "opus" => {
            if channels <= 1 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 16,
                    max_kbps: 160,
                    default_kbps: 64,
                    presets: vec![24, 32, 48, 64, 80, 96, 128],
                    notice: None,
                }
            } else if channels <= 2 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 32,
                    max_kbps: 320,
                    default_kbps: 128,
                    presets: vec![64, 96, 128, 160, 192, 256],
                    notice: None,
                }
            } else if channels <= 6 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 128,
                    max_kbps: 512,
                    default_kbps: 256,
                    presets: vec![160, 192, 224, 256, 320, 384, 448],
                    notice: None,
                }
            } else {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 160,
                    max_kbps: 640,
                    default_kbps: 320,
                    presets: vec![192, 256, 320, 384, 448, 512],
                    notice: None,
                }
            }
        }
        "libvorbis" | "vorbis" => {
            if channels <= 1 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 32,
                    max_kbps: 160,
                    default_kbps: 80,
                    presets: vec![48, 64, 80, 96, 128],
                    notice: None,
                }
            } else if channels <= 2 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 64,
                    max_kbps: 350,
                    default_kbps: 160,
                    presets: vec![96, 128, 160, 192, 224, 256, 320],
                    notice: None,
                }
            } else {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 192,
                    max_kbps: 500,
                    default_kbps: 320,
                    presets: vec![224, 256, 320, 384, 448, 500],
                    notice: None,
                }
            }
        }
        "aac" | "aac_at" => {
            if sr <= 24_000 {
                if channels <= 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 16,
                        max_kbps: 80,
                        default_kbps: 48,
                        presets: vec![24, 32, 48, 64],
                        notice: None,
                    }
                } else if channels <= 2 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 32,
                        max_kbps: 128,
                        default_kbps: 64,
                        presets: vec![32, 48, 64, 96, 128],
                        notice: None,
                    }
                } else {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 96,
                        max_kbps: 256,
                        default_kbps: 160,
                        presets: vec![96, 128, 160, 192, 256],
                        notice: None,
                    }
                }
            } else if sr == 32_000 {
                if channels <= 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 24,
                        max_kbps: 128,
                        default_kbps: 64,
                        presets: vec![32, 48, 64, 96, 128],
                        notice: None,
                    }
                } else if channels <= 2 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 48,
                        max_kbps: 224,
                        default_kbps: 128,
                        presets: vec![64, 96, 128, 160, 192],
                        notice: None,
                    }
                } else {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 160,
                        max_kbps: 448,
                        default_kbps: 256,
                        presets: vec![192, 256, 320, 384, 448],
                        notice: None,
                    }
                }
            } else if sr >= 96_000 {
                if channels <= 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 64,
                        max_kbps: 256,
                        default_kbps: 128,
                        presets: vec![96, 128, 160, 192, 256],
                        notice: None,
                    }
                } else if channels <= 2 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 128,
                        max_kbps: 512,
                        default_kbps: 256,
                        presets: vec![160, 192, 256, 320, 384, 512],
                        notice: None,
                    }
                } else if channels <= 6 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 320,
                        max_kbps: 896,
                        default_kbps: 512,
                        presets: vec![384, 448, 512, 640, 768, 896],
                        notice: None,
                    }
                } else {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 384,
                        max_kbps: 1024,
                        default_kbps: 640,
                        presets: vec![448, 512, 640, 768, 896, 1024],
                        notice: None,
                    }
                }
            } else {
                // 44.1 kHz & 48 kHz standard AAC
                if channels <= 1 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 32,
                        max_kbps: 192,
                        default_kbps: 96,
                        presets: vec![48, 64, 96, 128, 160],
                        notice: None,
                    }
                } else if channels <= 2 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 64,
                        max_kbps: 384,
                        default_kbps: 192,
                        presets: vec![96, 128, 160, 192, 256, 320],
                        notice: None,
                    }
                } else if channels <= 6 {
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 192,
                        max_kbps: 640,
                        default_kbps: 384,
                        presets: vec![256, 320, 384, 448, 512, 640],
                        notice: None,
                    }
                } else {
                    // 7.1 Surround (8ch)
                    AudioBitrateProfile {
                        is_lossless: false,
                        min_kbps: 256,
                        max_kbps: 768,
                        default_kbps: 512,
                        presets: vec![384, 448, 512, 640, 768],
                        notice: None,
                    }
                }
            }
        }
        _ => {
            // General fallback
            if channels <= 1 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 32,
                    max_kbps: 192,
                    default_kbps: 96,
                    presets: vec![48, 64, 96, 128, 160],
                    notice: None,
                }
            } else if channels <= 2 {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 64,
                    max_kbps: 384,
                    default_kbps: 192,
                    presets: vec![96, 128, 160, 192, 256, 320],
                    notice: None,
                }
            } else {
                AudioBitrateProfile {
                    is_lossless: false,
                    min_kbps: 192,
                    max_kbps: 640,
                    default_kbps: 384,
                    presets: vec![256, 320, 384, 448, 512, 640],
                    notice: None,
                }
            }
        }
    }
}

/// Complete configuration for an export job or saved preset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportSettings {
    pub processing_mode: VideoProcessingMode,
    pub export_selection_only: bool,
    pub keyframe_policy: KeyframeCutPolicy,
    pub container: ContainerFormat,
    pub video_encoder: String,
    pub crf: u8,
    #[serde(default)]
    pub rate_control_mode: VideoRateControlMode,
    #[serde(default = "default_target_video_bitrate_kbps")]
    pub target_video_bitrate_kbps: u32,
    #[serde(default = "default_target_file_size_mb")]
    pub target_file_size_mb: f32,
    pub encoder_preset: String,
    pub custom_resolution: Option<(u32, u32)>,
    pub custom_fps: Option<f64>,
    pub use_hwaccel_decode: bool,
    pub preserve_metadata: bool,
    #[serde(default)]
    pub audio_processing_mode: AudioProcessingMode,
    #[serde(default = "default_audio_encoder")]
    pub audio_encoder: String,
    #[serde(default = "default_audio_bitrate_kbps")]
    pub audio_bitrate_kbps: u32,
    #[serde(default)]
    pub audio_sample_rate: Option<u32>,
    #[serde(default)]
    pub audio_channels: AudioChannelMode,
    #[serde(default)]
    pub audio_volume_db: f32,
    #[serde(default)]
    pub audio_normalize_loudness: bool,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            processing_mode: VideoProcessingMode::DirectStreamCopy,
            export_selection_only: true,
            keyframe_policy: KeyframeCutPolicy::SnapToPrecedingKeyframe,
            container: ContainerFormat::Mp4,
            video_encoder: "libx264".to_string(),
            crf: 18,
            rate_control_mode: VideoRateControlMode::Crf,
            target_video_bitrate_kbps: 4000,
            target_file_size_mb: 25.0,
            encoder_preset: "veryfast".to_string(),
            custom_resolution: None,
            custom_fps: None,
            use_hwaccel_decode: false,
            preserve_metadata: true,
            audio_processing_mode: AudioProcessingMode::DirectStreamCopy,
            audio_encoder: "aac".to_string(),
            audio_bitrate_kbps: 192,
            audio_sample_rate: None,
            audio_channels: AudioChannelMode::Original,
            audio_volume_db: 0.0,
            audio_normalize_loudness: false,
        }
    }
}

impl ExportSettings {
    pub fn encoders_available(&self, caps: &MediaCapabilities) -> bool {
        (self.processing_mode == VideoProcessingMode::DirectStreamCopy
            || caps.has_encoder(&self.video_encoder))
            && (self.audio_processing_mode != AudioProcessingMode::FullProcessing
                || caps.has_encoder(&self.audio_encoder))
    }

    /// Computes the effective target video bitrate in kbps when `rate_control_mode` is
    /// `TargetBitrateKbps` or `TargetFileSizeMb`. Returns `None` in `Crf` mode.
    pub fn effective_video_bitrate_kbps(
        &self,
        duration_secs: f64,
        active_audio_tracks: usize,
    ) -> Option<u32> {
        match self.rate_control_mode {
            VideoRateControlMode::Crf => None,
            VideoRateControlMode::TargetBitrateKbps => {
                Some(self.target_video_bitrate_kbps.max(100))
            }
            VideoRateControlMode::TargetFileSizeMb => {
                let dur = duration_secs.max(0.5);
                let total_bits =
                    (self.target_file_size_mb.max(0.5) as f64) * 8.0 * 1024.0 * 1024.0 * 0.96;
                let total_kbps = total_bits / dur / 1000.0;
                let audio_kbps = if self.audio_processing_mode == AudioProcessingMode::NoAudio {
                    0.0
                } else {
                    (active_audio_tracks.max(1) as f64) * (self.audio_bitrate_kbps as f64)
                };
                let video_kbps = (total_kbps - audio_kbps).round() as i64;
                Some(video_kbps.clamp(100, 200_000) as u32)
            }
        }
    }
    /// Computes the tailored audio bitrate profile for the current encoder, sample rate, and channels.
    pub fn audio_bitrate_profile(
        &self,
        source_channels: Option<u32>,
        source_sample_rate: Option<u32>,
    ) -> AudioBitrateProfile {
        let ch = self.audio_channels.channel_count().or(source_channels).unwrap_or(2);
        let sr = self.audio_sample_rate.or(source_sample_rate);
        audio_bitrate_profile(&self.audio_encoder, sr, ch)
    }

    /// Automatically validates and adjusts `audio_bitrate_kbps` to fit within the valid range
    /// for the current compression format, sample rate, and channel count.
    pub fn clamp_audio_bitrate(
        &mut self,
        source_channels: Option<u32>,
        source_sample_rate: Option<u32>,
    ) {
        let profile = self.audio_bitrate_profile(source_channels, source_sample_rate);
        if !profile.is_lossless {
            self.audio_bitrate_kbps = self.audio_bitrate_kbps.clamp(profile.min_kbps, profile.max_kbps);
        }
    }

    /// Updates the audio encoder and automatically applies the recommended default bitrate
    /// tailored to the compression format, sample rate, and channels.
    pub fn apply_recommended_defaults_for_audio_encoder_with_format(
        &mut self,
        new_audio_encoder: &str,
        source_channels: Option<u32>,
        source_sample_rate: Option<u32>,
    ) {
        self.audio_encoder = new_audio_encoder.to_string();
        let profile = self.audio_bitrate_profile(source_channels, source_sample_rate);
        if !profile.is_lossless {
            self.audio_bitrate_kbps = profile.default_kbps;
        }
    }

    /// Applies sensible audio bitrate defaults when switching audio encoders (fallback).
    pub fn apply_recommended_defaults_for_audio_encoder(&mut self, new_audio_encoder: &str) {
        self.apply_recommended_defaults_for_audio_encoder_with_format(new_audio_encoder, None, None);
    }
    /// Applies sensible codec-specific CRF and speed preset defaults when the user switches encoders.
    pub fn apply_recommended_defaults_for_encoder(&mut self, new_encoder: &str) {
        self.video_encoder = new_encoder.to_string();
        if new_encoder == "libx265" {
            self.crf = 22;
            self.encoder_preset = "veryfast".to_string();
        } else if new_encoder == "libvvenc" || new_encoder == "vvenc" || new_encoder == "vvencapp" {
            self.crf = 26;
            self.encoder_preset = "fast".to_string();
            if self.container == ContainerFormat::Avi || self.container == ContainerFormat::Webm {
                self.container = ContainerFormat::Mp4;
            }
        } else if new_encoder == "libx264" {
            self.crf = 18;
            self.encoder_preset = "veryfast".to_string();
        } else if new_encoder == "libvpx-vp9" || new_encoder == "libsvtav1" || new_encoder == "libaom-av1" {
            self.crf = 28;
            self.encoder_preset = "fast".to_string();
        } else if new_encoder.contains("videotoolbox")
            || new_encoder.contains("nvenc")
            || new_encoder.contains("qsv")
            || new_encoder.contains("amf")
            || new_encoder.contains("vaapi")
        {
            self.crf = if new_encoder.starts_with("av1_") {
                26
            } else if new_encoder.starts_with("hevc_") {
                22
            } else {
                20
            };
        }
    }
}

/// User-saved encoding preset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodingPreset {
    pub name: String,
    pub settings: ExportSettings,
}

impl EncodingPreset {
    pub fn built_in_presets() -> Vec<Self> {
        vec![
            Self {
                name: "Lossless Direct Stream Copy (MP4)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::DirectStreamCopy,
                    container: ContainerFormat::Mp4,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Lossless Direct Stream Copy (MKV)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::DirectStreamCopy,
                    container: ContainerFormat::Mkv,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Smart Cut / Smart Rendering (MP4)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::SmartCopy,
                    container: ContainerFormat::Mp4,
                    video_encoder: "libx264".to_string(),
                    crf: 16,
                    encoder_preset: "fast".to_string(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "High Quality H.264 (CRF 18, MP4)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mp4,
                    video_encoder: "libx264".to_string(),
                    crf: 18,
                    encoder_preset: "medium".to_string(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Fast H.264 (CRF 23, MP4)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    video_encoder: "libx264".into(), crf: 23, encoder_preset: "veryfast".into(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "H.264 Sharing (4 Mbps + AAC, MP4)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    video_encoder: "libx264".into(), encoder_preset: "fast".into(),
                    rate_control_mode: VideoRateControlMode::TargetBitrateKbps,
                    target_video_bitrate_kbps: 4000,
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    audio_bitrate_kbps: 128,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "High Quality H.265 / HEVC (CRF 18, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv,
                    video_encoder: "libx265".into(),
                    crf: 18,
                    encoder_preset: "medium".into(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Compact H.265 / HEVC (CRF 22, MKV)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv,
                    video_encoder: "libx265".to_string(),
                    crf: 22,
                    encoder_preset: "medium".to_string(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "High Quality AV1 (CRF 24, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv, video_encoder: "libsvtav1".into(),
                    crf: 24, encoder_preset: "slow".into(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Compact AV1 (CRF 32, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv, video_encoder: "libsvtav1".into(),
                    crf: 32, encoder_preset: "fast".into(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "WebM AV1 (CRF 28 + Opus)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Webm,
                    video_encoder: "libsvtav1".to_string(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    audio_encoder: "libopus".to_string(),
                    audio_bitrate_kbps: 128,
                    crf: 28,
                    encoder_preset: "medium".to_string(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Next-Gen H.266 / VVC (CRF 26, MP4)".to_string(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mp4,
                    video_encoder: "libvvenc".to_string(),
                    crf: 26,
                    encoder_preset: "fast".to_string(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Lossless Archive (FFV1 + FLAC, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv, video_encoder: "ffv1".into(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    audio_encoder: "flac".into(),
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Hardware H.264 (Fast GPU, MP4)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mp4,
                    video_encoder: "h264_hw".into(),
                    crf: 20,
                    encoder_preset: "fast".into(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Hardware HEVC / H.265 (GPU, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv,
                    video_encoder: "hevc_hw".into(),
                    crf: 22,
                    encoder_preset: "fast".into(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    ..ExportSettings::default()
                },
            },
            Self {
                name: "Hardware AV1 (GPU, MKV)".into(),
                settings: ExportSettings {
                    processing_mode: VideoProcessingMode::FullProcessing,
                    container: ContainerFormat::Mkv,
                    video_encoder: "av1_hw".into(),
                    crf: 26,
                    encoder_preset: "fast".into(),
                    audio_processing_mode: AudioProcessingMode::FullProcessing,
                    ..ExportSettings::default()
                },
            },
        ]
    }

    /// Resolve built-in AV1 and hardware GPU templates to an installed/verified implementation,
    /// and hide presets whose required video/audio encoder is unavailable.
    pub fn available_with(&self, caps: &MediaCapabilities) -> Option<Self> {
        let mut preset = self.clone();
        if preset.settings.video_encoder == "h264_hw" {
            if let Some(hw) = caps
                .preferred_video_encoders()
                .into_iter()
                .find(|e| e.is_hardware && e.name.starts_with("h264_"))
            {
                preset.settings.video_encoder = hw.name;
            }
        } else if preset.settings.video_encoder == "hevc_hw" {
            if let Some(hw) = caps
                .preferred_video_encoders()
                .into_iter()
                .find(|e| e.is_hardware && e.name.starts_with("hevc_"))
            {
                preset.settings.video_encoder = hw.name;
            }
        } else if preset.settings.video_encoder == "av1_hw" {
            if let Some(hw) = caps
                .preferred_video_encoders()
                .into_iter()
                .find(|e| e.is_hardware && e.name.starts_with("av1_"))
            {
                preset.settings.video_encoder = hw.name;
            }
        } else if matches!(preset.name.as_str(), "WebM AV1 (CRF 28 + Opus)"
            | "High Quality AV1 (CRF 24, MKV)" | "Compact AV1 (CRF 32, MKV)")
            && preset.settings.video_encoder == "libsvtav1"
            && !caps.has_encoder("libsvtav1") && caps.has_encoder("libaom-av1") {
            preset.settings.video_encoder = "libaom-av1".into();
        }
        preset.settings.encoders_available(caps).then_some(preset)
    }

    pub fn load_all(presets_file: &Path) -> Vec<Self> {
        let mut presets = Self::built_in_presets();
        if let Ok(bytes) = fs::read(presets_file) {
            if let Ok(saved) = serde_json::from_slice::<Vec<Self>>(&bytes) {
                for preset in saved {
                    // Retire the old bundled VP9 template, preserving custom VP9 presets.
                    if preset.name == "WebM VP9 (CRF 28)"
                        && preset.settings.video_encoder == "libvpx-vp9" { continue; }
                    if let Some(existing) = presets.iter_mut().find(|p| p.name == preset.name) {
                        *existing = preset;
                    } else {
                        presets.push(preset);
                    }
                }
            }
        }
        presets
    }

    pub fn save_all(presets_file: &Path, presets: &[Self]) -> Result<(), std::io::Error> {
        if let Some(parent) = presets_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(presets)?;
        fs::write(presets_file, json)
    }
}

/// Real-time progress update emitted by an active export job.
#[derive(Debug, Clone)]
pub enum ExportEvent {
    Progress {
        fraction: f32,
        encoded_time: TimestampUs,
        total_duration: TimestampUs,
        fps: f32,
        speed: String,
        size_bytes: u64,
    },
    Completed {
        output_path: PathBuf,
        duration: TimestampUs,
    },
    Cancelled,
    Failed(String),
}

/// Handle to a running background export job.
pub struct ExportJobHandle {
    pub output_path: PathBuf,
    pub receiver: Receiver<ExportEvent>,
    cancel_flag: Arc<AtomicBool>,
}

impl ExportJobHandle {
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }
}

/// Starts an asynchronous export job on a background worker thread.
pub fn start_export_job(
    binaries: FfmpegBinaries,
    project: Project,
    filters: FilterPipeline,
    settings: ExportSettings,
    output_path: PathBuf,
) -> Result<ExportJobHandle, ExportError> {
    let segments = project.effective_export_segments(settings.export_selection_only);
    if segments.is_empty() {
        return Err(ExportError::EmptyTimeline);
    }

    let (tx, rx) = mpsc::channel::<ExportEvent>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel_flag);
    let out_clone = output_path.clone();

    thread::Builder::new()
        .name("cutdub-export-worker".into())
        .spawn(move || {
            let result = execute_export(
                &binaries,
                &project,
                &segments,
                &filters,
                &settings,
                &out_clone,
                &cancel_clone,
                &tx,
            );
            match result {
                Ok(total_dur) => {
                    let _ = tx.send(ExportEvent::Completed {
                        output_path: out_clone,
                        duration: total_dur,
                    });
                }
                Err(ExportError::Cancelled) => {
                    let _ = fs::remove_file(&out_clone);
                    let _ = tx.send(ExportEvent::Cancelled);
                }
                Err(e) => {
                    let _ = fs::remove_file(&out_clone);
                    let _ = tx.send(ExportEvent::Failed(e.to_string()));
                }
            }
        })
        .map_err(ExportError::Io)?;

    Ok(ExportJobHandle {
        output_path,
        receiver: rx,
        cancel_flag,
    })
}

/// Starts an asynchronous Audio-Only export job (`-vn`) extracting or transcoding audio from the
/// active selection/timeline to `.wav`, `.flac`, `.mp3`, `.m4a`, `.opus`, or `.ogg`.
pub fn start_audio_only_export_job(
    binaries: FfmpegBinaries,
    project: Project,
    filters: FilterPipeline,
    settings: ExportSettings,
    output_path: PathBuf,
) -> Result<ExportJobHandle, ExportError> {
    let segments = project.effective_export_segments(settings.export_selection_only);
    let Some(first_seg) = segments.first().cloned() else {
        return Err(ExportError::EmptyTimeline);
    };
    let last_end_frame = segments.last().map(|s| s.end_frame).unwrap_or(first_seg.end_frame);
    let src = project
        .source_by_id(first_seg.source_id)
        .ok_or(ExportError::EmptyTimeline)?
        .clone();

    let start_pts = src.timing.frame_to_timestamp(first_seg.start_frame);
    let end_pts = src.timing.frame_to_timestamp(last_end_frame);
    let duration_us = (end_pts.0 - start_pts.0).max(1_000);
    let duration_secs = (duration_us as f64) / 1_000_000.0;
    let total_duration = TimestampUs(duration_us);

    let (tx, rx) = mpsc::channel::<ExportEvent>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel_flag);
    let out_clone = output_path.clone();

    thread::Builder::new()
        .name("cutdub-audio-export-worker".into())
        .spawn(move || {
            let mut cmd = Command::new(&binaries.ffmpeg_path);
            configure_background_command(&mut cmd);
            cmd.args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-progress",
                "pipe:1",
                "-nostats",
            ]);
            if start_pts.0 > 0 {
                cmd.arg("-ss")
                    .arg(format!("{:.6}", start_pts.as_seconds_f64()));
            }
            if let Some(ext_audio) = &project.external_audio_path {
                if ext_audio.exists() && project.replace_source_audio_with_external {
                    cmd.arg("-i").arg(ext_audio);
                } else {
                    cmd.arg("-i").arg(&src.path);
                }
            } else {
                cmd.arg("-i").arg(&src.path);
            }
            cmd.arg("-t").arg(format!("{duration_secs:.6}"));
            cmd.arg("-vn");

            let ext = out_clone
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("wav")
                .to_lowercase();
            let codec = match ext.as_str() {
                "wav" => "pcm_s16le".to_string(),
                "flac" => "flac".to_string(),
                "mp3" => "libmp3lame".to_string(),
                "opus" | "ogg" => "libopus".to_string(),
                "m4a" | "aac" => "aac".to_string(),
                _ => settings.audio_encoder.clone(),
            };
            cmd.args(["-c:a", &codec]);
            if !is_lossless_or_pcm_audio_encoder(&codec) {
                cmd.args(["-b:a", &format!("{}k", settings.audio_bitrate_kbps)]);
            }
            if let Some(sr) = settings.audio_sample_rate {
                cmd.args(["-ar", &sr.to_string()]);
            }
            if let Some(ch) = settings.audio_channels.channel_count() {
                cmd.args(["-ac", &ch.to_string()]);
            }

            let mut af_parts = Vec::new();
            if settings.audio_volume_db.abs() > 0.05 {
                af_parts.push(format!("volume={:.1}dB", settings.audio_volume_db));
            }
            if settings.audio_normalize_loudness {
                af_parts.push("loudnorm=I=-16:TP=-1.5:LRA=11".to_string());
            }
            let speed_factor = filters.effective_speed_factor();
            if (speed_factor - 1.0).abs() > 0.005 {
                let mut rem = speed_factor.clamp(0.125, 8.0);
                while rem > 2.0 + 1e-4 {
                    af_parts.push("atempo=2.0".to_string());
                    rem /= 2.0;
                }
                while rem < 0.5 - 1e-4 {
                    af_parts.push("atempo=0.5".to_string());
                    rem /= 0.5;
                }
                if (rem - 1.0).abs() > 0.005 {
                    af_parts.push(format!("atempo={rem:.4}"));
                }
            }
            if !af_parts.is_empty() {
                cmd.args(["-af", &af_parts.join(",")]);
            }

            cmd.arg(&out_clone);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            match run_ffmpeg_with_progress(cmd, total_duration, TimestampUs(0), &cancel_clone, &tx) {
                Ok(()) => {
                    let _ = tx.send(ExportEvent::Completed {
                        output_path: out_clone,
                        duration: total_duration,
                    });
                }
                Err(ExportError::Cancelled) => {
                    let _ = fs::remove_file(&out_clone);
                    let _ = tx.send(ExportEvent::Cancelled);
                }
                Err(e) => {
                    let _ = fs::remove_file(&out_clone);
                    let _ = tx.send(ExportEvent::Failed(e.to_string()));
                }
            }
        })
        .map_err(ExportError::Io)?;

    Ok(ExportJobHandle {
        output_path,
        receiver: rx,
        cancel_flag,
    })
}

/// Splits the timeline at every Timeline Marker (`project.split_clip_ranges()`) and exports each
/// sub-clip into `<parent>/<stem>_01_<label>.<ext>` asynchronously.
pub fn start_split_by_markers_export_job(
    binaries: FfmpegBinaries,
    project: Project,
    filters: FilterPipeline,
    settings: ExportSettings,
    base_output_path: PathBuf,
) -> Result<ExportJobHandle, ExportError> {
    let ranges = project.split_clip_ranges();
    if ranges.is_empty() {
        return Err(ExportError::EmptyTimeline);
    }

    let (tx, rx) = mpsc::channel::<ExportEvent>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel_flag);
    let out_clone = base_output_path.clone();

    thread::Builder::new()
        .name("cutdub-split-export-worker".into())
        .spawn(move || {
            let parent = out_clone
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let stem = out_clone
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("clip");
            let ext = settings.container.extension();

            let mut total_dur = TimestampUs(0);
            let mut last_path = out_clone.clone();

            for (idx, (s_frame, e_frame, label)) in ranges.iter().enumerate() {
                if cancel_clone.load(Ordering::SeqCst) {
                    let _ = tx.send(ExportEvent::Cancelled);
                    return;
                }
                let segs = project.segments_for_virtual_range(*s_frame, *e_frame);
                if segs.is_empty() {
                    continue;
                }
                let sanitized_label: String = label
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
                    .collect();
                let clip_path = parent.join(format!(
                    "{}_{:02}_{}.{}",
                    stem,
                    idx + 1,
                    sanitized_label,
                    ext
                ));
                match execute_export(
                    &binaries,
                    &project,
                    &segs,
                    &filters,
                    &settings,
                    &clip_path,
                    &cancel_clone,
                    &tx,
                ) {
                    Ok(dur) => {
                        total_dur.0 += dur.0;
                        last_path = clip_path;
                    }
                    Err(ExportError::Cancelled) => {
                        let _ = fs::remove_file(&clip_path);
                        let _ = tx.send(ExportEvent::Cancelled);
                        return;
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&clip_path);
                        let _ = tx.send(ExportEvent::Failed(e.to_string()));
                        return;
                    }
                }
            }

            let _ = tx.send(ExportEvent::Completed {
                output_path: last_path,
                duration: total_dur,
            });
        })
        .map_err(ExportError::Io)?;

    Ok(ExportJobHandle {
        output_path: base_output_path,
        receiver: rx,
        cancel_flag,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_export(
    binaries: &FfmpegBinaries,
    project: &Project,
    segments: &[EditSegment],
    filters: &FilterPipeline,
    settings: &ExportSettings,
    output_path: &Path,
    cancel_flag: &Arc<AtomicBool>,
    tx: &mpsc::Sender<ExportEvent>,
) -> Result<TimestampUs, ExportError> {
    // Compute total expected duration across all export segments
    let mut total_duration_us = 0i64;
    for seg in segments {
        if let Some(src) = project.source_by_id(seg.source_id) {
            let s = src.timing.frame_to_timestamp(seg.start_frame);
            let e = src.timing.frame_to_timestamp(seg.end_frame);
            total_duration_us += (e.0 - s.0).max(1);
        }
    }
    let speed_factor = if settings.processing_mode == VideoProcessingMode::FullProcessing {
        filters.effective_speed_factor()
    } else {
        1.0
    };
    let scaled_duration_us = ((total_duration_us as f64) / speed_factor).round() as i64;
    let total_duration = TimestampUs(scaled_duration_us.max(1));

    // Expand segments into concrete (EditSegment, ExportSettings) tasks.
    // In SmartCopy mode, any segment starting on a non-keyframe is split into:
    //   1) [start_frame, next_keyframe) -> FullProcessing (re-encode partial lead-in GOP)
    //   2) [next_keyframe, end_frame)   -> DirectStreamCopy (lossless stream copy from keyframe)
    let mut sub_tasks: Vec<(EditSegment, ExportSettings, FilterPipeline)> = Vec::new();
    for seg in segments {
        if settings.processing_mode == VideoProcessingMode::SmartCopy {
            if let Some(src) = project.source_by_id(seg.source_id) {
                let kf_before = src.timing.keyframe_at_or_before(seg.start_frame);
                if kf_before == seg.start_frame {
                    let mut copy_set = settings.clone();
                    copy_set.processing_mode = VideoProcessingMode::DirectStreamCopy;
                    copy_set.keyframe_policy = KeyframeCutPolicy::ExactTimestamp;
                    sub_tasks.push((*seg, copy_set, FilterPipeline::new()));
                    continue;
                }
                let next_kf = src.timing.next_keyframe(seg.start_frame).min(seg.end_frame);
                if next_kf > seg.start_frame {
                    let mut gop_set = settings.clone();
                    gop_set.processing_mode = VideoProcessingMode::FullProcessing;
                    gop_set.video_encoder =
                        matching_smart_encoder(&src.video_codec, &settings.video_encoder);
                    gop_set.custom_resolution = None;
                    gop_set.custom_fps = None;
                    sub_tasks.push((
                        EditSegment {
                            source_id: seg.source_id,
                            start_frame: seg.start_frame,
                            end_frame: next_kf,
                        },
                        gop_set,
                        FilterPipeline::new(),
                    ));
                }
                if next_kf < seg.end_frame {
                    let mut copy_set = settings.clone();
                    copy_set.processing_mode = VideoProcessingMode::DirectStreamCopy;
                    copy_set.keyframe_policy = KeyframeCutPolicy::ExactTimestamp;
                    sub_tasks.push((
                        EditSegment {
                            source_id: seg.source_id,
                            start_frame: next_kf,
                            end_frame: seg.end_frame,
                        },
                        copy_set,
                        FilterPipeline::new(),
                    ));
                }
                continue;
            }
        }
        sub_tasks.push((*seg, settings.clone(), filters.clone()));
    }

    if sub_tasks.len() == 1 {
        let (ref seg, ref task_settings, ref task_filters) = sub_tasks[0];
        execute_single_segment_task(
            binaries,
            project,
            seg,
            task_filters,
            task_settings,
            output_path,
            total_duration,
            TimestampUs::ZERO,
            cancel_flag,
            tx,
        )?;
        Ok(total_duration)
    } else {
        let temp_dir = tempfile::TempDir::new()?;
        let mut part_paths = Vec::with_capacity(sub_tasks.len());
        let mut completed_us = 0i64;

        for (idx, (seg, task_settings, task_filters)) in sub_tasks.iter().enumerate() {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err(ExportError::Cancelled);
            }
            let part_path = temp_dir
                .path()
                .join(format!("part_{idx:04}.{}", settings.container.extension()));
            execute_single_segment_task(
                binaries,
                project,
                seg,
                task_filters,
                task_settings,
                &part_path,
                total_duration,
                TimestampUs(completed_us),
                cancel_flag,
                tx,
            )?;
            if let Some(src) = project.source_by_id(seg.source_id) {
                let s = src.timing.frame_to_timestamp(seg.start_frame);
                let e = src.timing.frame_to_timestamp(seg.end_frame);
                completed_us += (e.0 - s.0).max(0);
            }
            part_paths.push(part_path);
        }

        // Write concat list
        let concat_list_path = temp_dir.path().join("concat_list.txt");
        let mut list_content = String::new();
        for part in &part_paths {
            let escaped = part.to_string_lossy().replace('\'', "'\\''");
            list_content.push_str(&format!("file '{escaped}'\n"));
        }
        fs::write(&concat_list_path, list_content)?;

        let mut concat_cmd = Command::new(&binaries.ffmpeg_path);
        configure_background_command(&mut concat_cmd);
        concat_cmd
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&concat_list_path)
            .args(["-c", "copy"])
            .arg(output_path);

        let out = concat_cmd.output()?;
        if !out.status.success() {
            return Err(ExportError::FfmpegFailed(
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }

        Ok(total_duration)
    }
}

fn build_single_segment_command(
    binaries: &FfmpegBinaries,
    project: &Project,
    seg: &EditSegment,
    filters: &FilterPipeline,
    settings: &ExportSettings,
    output_path: &Path,
) -> Result<Command, ExportError> {
    let src = project
        .source_by_id(seg.source_id)
        .ok_or(ExportError::EmptyTimeline)?;

    let effective_start_frame = match (settings.processing_mode, settings.keyframe_policy) {
        (VideoProcessingMode::DirectStreamCopy, KeyframeCutPolicy::SnapToPrecedingKeyframe) => {
            src.timing.keyframe_at_or_before(seg.start_frame)
        }
        _ => seg.start_frame,
    };

    let start_pts = src.timing.frame_to_timestamp(effective_start_frame);
    let end_pts = src.timing.frame_to_timestamp(seg.end_frame);
    let duration_secs = ((end_pts.0 - start_pts.0).max(1_000)) as f64 / 1_000_000.0;

    let mut cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut cmd);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-progress",
        "pipe:1",
        "-nostats",
    ]);

    if settings.use_hwaccel_decode && settings.processing_mode == VideoProcessingMode::FullProcessing
    {
        cmd.args(["-hwaccel", "auto"]);
    }

    let mut ext_audio_input_idx: Option<usize> = None;

    match settings.processing_mode {
        VideoProcessingMode::DirectStreamCopy => {
            if start_pts.0 > 0 {
                cmd.arg("-ss")
                    .arg(format!("{:.6}", start_pts.as_seconds_f64()));
            }
            cmd.arg("-i").arg(&src.path);
            if let Some(ext_audio) = &project.external_audio_path {
                if ext_audio.exists() {
                    cmd.arg("-i").arg(ext_audio);
                    ext_audio_input_idx = Some(1);
                }
            }
            cmd.arg("-t").arg(format!("{duration_secs:.6}"));
            cmd.args(["-map", &format!("0:{}", src.video_stream_index)]);
            cmd.args(["-c:v", "copy"]);
        }
        VideoProcessingMode::FullProcessing | VideoProcessingMode::SmartCopy => {
            // Enable multi-threaded input decoding and filtergraph execution across all CPU cores
            let logical_cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8)
                .clamp(2, 64);

            if !settings.use_hwaccel_decode {
                cmd.args(["-threads", "0", "-thread_type", "frame+slice"]);
            }
            cmd.args(["-filter_threads", "0", "-filter_complex_threads", "0"]);

            // Single-stage frame-accurate input seek
            if start_pts.0 > 0 {
                cmd.arg("-ss")
                    .arg(format!("{:.6}", start_pts.as_seconds_f64()));
            }
            cmd.arg("-i").arg(&src.path);
            if let Some(ext_audio) = &project.external_audio_path {
                if ext_audio.exists() {
                    cmd.arg("-i").arg(ext_audio);
                    ext_audio_input_idx = Some(1);
                }
            }
            cmd.arg("-t").arg(format!("{duration_secs:.6}"));
            cmd.args(["-map", &format!("0:{}", src.video_stream_index)]);

            // Construct video filtergraph from FilterPipeline + optional custom export resolution
            let (mut vf_chain, out_w, out_h) = filters.to_ffmpeg_filtergraph(src.width, src.height);
            if let Some((req_w, req_h)) = settings.custom_resolution {
                let w = req_w.max(2) & !1;
                let h = req_h.max(2) & !1;
                if w != out_w || h != out_h {
                    let scale_expr = format!("scale={w}:{h}");
                    vf_chain = Some(match vf_chain {
                        Some(existing) => format!("{existing},{scale_expr}"),
                        None => scale_expr,
                    });
                }
            }
            if let Some(vf) = vf_chain {
                cmd.args(["-vf", &vf]);
            }

            if let Some(fps) = settings.custom_fps {
                if fps > 0.0 {
                    cmd.args(["-r", &format!("{fps:.3}")]);
                }
            }

            cmd.args(["-c:v", &settings.video_encoder]);

            let active_audio_tracks = project
                .audio_configs
                .iter()
                .filter(|a| a.mode != AudioExportMode::Disabled)
                .count();
            let target_v_kbps =
                settings.effective_video_bitrate_kbps(duration_secs, active_audio_tracks);

            // Apply quality / CRF or target bitrate, preset, and multi-core threading flags according to encoder family
            let enc = settings.video_encoder.as_str();
            if enc == "libx264" {
                cmd.args(["-threads", "0"]);
                if let Some(kbps) = target_v_kbps {
                    cmd.args([
                        "-b:v",
                        &format!("{kbps}k"),
                        "-maxrate",
                        &format!("{}k", (kbps * 3) / 2),
                        "-bufsize",
                        &format!("{}k", kbps * 2),
                    ]);
                } else {
                    cmd.args(["-crf", &settings.crf.to_string()]);
                }
                cmd.args(["-preset", &settings.encoder_preset, "-pix_fmt", "yuv420p"]);
            } else if enc == "libx265" {
                cmd.args(["-threads", "0"]);
                if let Some(kbps) = target_v_kbps {
                    cmd.args([
                        "-b:v",
                        &format!("{kbps}k"),
                        "-maxrate",
                        &format!("{}k", (kbps * 3) / 2),
                        "-bufsize",
                        &format!("{}k", kbps * 2),
                    ]);
                } else {
                    cmd.args(["-crf", &settings.crf.to_string()]);
                }
                cmd.args([
                    "-preset",
                    &settings.encoder_preset,
                    "-x265-params",
                    "pools=+:frame-threads=0:wpp=1:no-sao=1:ctu=32:b-adapt=1",
                    "-pix_fmt",
                    "yuv420p",
                ]);
                if settings.container == ContainerFormat::Mp4
                    || settings.container == ContainerFormat::Mov
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else if enc == "libsvtav1" {
                let svt_preset = match settings.encoder_preset.as_str() {
                    "ultrafast" => "10",
                    "superfast" | "veryfast" => "9",
                    "faster" | "fast" => "8",
                    "medium" => "7",
                    "slow" => "5",
                    "slower" | "veryslow" => "4",
                    _ => "8",
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    cmd.args(["-crf", &settings.crf.to_string()]);
                }
                cmd.args([
                    "-preset",
                    svt_preset,
                    "-svtav1-params",
                    "lp=0",
                    "-pix_fmt",
                    "yuv420p",
                ]);
            } else if enc == "libaom-av1" {
                let cpu_used = match settings.encoder_preset.as_str() {
                    "ultrafast" | "superfast" => "8",
                    "veryfast" | "faster" => "7",
                    "fast" | "medium" => "6",
                    _ => "4",
                };
                let tiles = if logical_cpus >= 16 {
                    "3x2"
                } else if logical_cpus >= 8 {
                    "2x2"
                } else {
                    "2x1"
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    cmd.args(["-crf", &settings.crf.to_string(), "-b:v", "0"]);
                }
                cmd.args([
                    "-cpu-used",
                    cpu_used,
                    "-row-mt",
                    "1",
                    "-tiles",
                    tiles,
                    "-threads",
                    &logical_cpus.to_string(),
                    "-pix_fmt",
                    "yuv420p",
                ]);
            } else if enc == "libvpx-vp9" {
                let cpu_used = match settings.encoder_preset.as_str() {
                    "ultrafast" | "superfast" => "5",
                    "veryfast" | "faster" | "fast" => "4",
                    "medium" => "3",
                    "slow" => "2",
                    _ => "1",
                };
                let tile_cols_log2 = if logical_cpus >= 16 {
                    "3"
                } else if logical_cpus >= 6 {
                    "2"
                } else {
                    "1"
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    cmd.args(["-crf", &settings.crf.to_string(), "-b:v", "0"]);
                }
                cmd.args([
                    "-row-mt",
                    "1",
                    "-tile-columns",
                    tile_cols_log2,
                    "-tile-rows",
                    "1",
                    "-threads",
                    &logical_cpus.to_string(),
                    "-cpu-used",
                    cpu_used,
                    "-pix_fmt",
                    "yuv420p",
                ]);
            } else if enc == "libvvenc" {
                let vvenc_preset = map_vvenc_preset(&settings.encoder_preset);
                cmd.args(["-threads", &logical_cpus.to_string()]);
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    cmd.args(["-qp", &settings.crf.clamp(1, 63).to_string()]);
                }
                cmd.args(["-preset", vvenc_preset, "-pix_fmt", "yuv420p10le"]);
            } else if enc.contains("videotoolbox") {
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    let q = (100u32.saturating_sub(settings.crf as u32 * 2)).clamp(20, 95);
                    cmd.args(["-q:v", &q.to_string()]);
                }
                if !enc.starts_with("prores_") {
                    cmd.args(["-pix_fmt", "yuv420p"]);
                }
                if enc.contains("hevc")
                    && (settings.container == ContainerFormat::Mp4
                        || settings.container == ContainerFormat::Mov)
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else if enc.ends_with("_amf") {
                let amf_quality = match settings.encoder_preset.as_str() {
                    "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" => "speed",
                    "medium" => "balanced",
                    _ => "quality",
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args([
                        "-b:v",
                        &format!("{kbps}k"),
                        "-maxrate",
                        &format!("{}k", (kbps * 3) / 2),
                        "-bufsize",
                        &format!("{}k", kbps * 2),
                    ]);
                } else {
                    let qp = settings.crf.clamp(0, 51).to_string();
                    cmd.args(["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp]);
                }
                cmd.args(["-quality", amf_quality, "-pix_fmt", "yuv420p"]);
                if enc.contains("hevc")
                    && (settings.container == ContainerFormat::Mp4
                        || settings.container == ContainerFormat::Mov)
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else if enc.ends_with("_nvenc") {
                let nvenc_preset = match settings.encoder_preset.as_str() {
                    "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" => "fast",
                    "medium" => "medium",
                    _ => "slow",
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args([
                        "-b:v",
                        &format!("{kbps}k"),
                        "-maxrate",
                        &format!("{}k", (kbps * 3) / 2),
                        "-bufsize",
                        &format!("{}k", kbps * 2),
                    ]);
                } else {
                    let cq = settings.crf.clamp(0, 51).to_string();
                    cmd.args(["-rc", "vbr", "-cq", &cq, "-b:v", "0"]);
                }
                cmd.args(["-preset", nvenc_preset, "-pix_fmt", "yuv420p"]);
                if enc.contains("hevc")
                    && (settings.container == ContainerFormat::Mp4
                        || settings.container == ContainerFormat::Mov)
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else if enc.ends_with("_qsv") {
                let qsv_preset = match settings.encoder_preset.as_str() {
                    "ultrafast" | "superfast" | "veryfast" => "veryfast",
                    "faster" => "faster",
                    "fast" => "fast",
                    "medium" => "medium",
                    "slow" => "slow",
                    "slower" => "slower",
                    _ => "veryslow",
                };
                if let Some(kbps) = target_v_kbps {
                    cmd.args([
                        "-b:v",
                        &format!("{kbps}k"),
                        "-maxrate",
                        &format!("{}k", (kbps * 3) / 2),
                        "-bufsize",
                        &format!("{}k", kbps * 2),
                    ]);
                } else {
                    let gq = settings.crf.clamp(1, 51).to_string();
                    cmd.args(["-global_quality", &gq]);
                }
                cmd.args(["-preset", qsv_preset, "-pix_fmt", "nv12"]);
                if enc.contains("hevc")
                    && (settings.container == ContainerFormat::Mp4
                        || settings.container == ContainerFormat::Mov)
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else if enc.ends_with("_vaapi") {
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                } else {
                    let qp = settings.crf.clamp(1, 51).to_string();
                    cmd.args(["-qp", &qp]);
                }
                if enc.contains("hevc")
                    && (settings.container == ContainerFormat::Mp4
                        || settings.container == ContainerFormat::Mov)
                {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            } else {
                cmd.args(["-threads", "0"]);
                if let Some(kbps) = target_v_kbps {
                    cmd.args(["-b:v", &format!("{kbps}k")]);
                }
            }
        }
    }

    append_audio_and_subtitle_args(&mut cmd, 0, ext_audio_input_idx, project, filters, settings);

    if settings.container == ContainerFormat::Mp4 || settings.container == ContainerFormat::Mov {
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.arg(output_path);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    info!(output = %output_path.display(), mode = ?settings.processing_mode, "Built FFmpeg export command");
    Ok(cmd)
}

/// Appends audio mapping, encoding (`-c:a`, `-b:a`, `-ar`, `-ac`), audio filtergraph (`-af`
/// for `adelay`, `atrim`, `volume`, `loudnorm`, and `atempo`), subtitle mapping, and metadata
/// flags for the source media input at `source_input_index` (and optional external audio at `ext_audio_input_idx`).
fn append_audio_and_subtitle_args(
    cmd: &mut Command,
    source_input_index: usize,
    ext_audio_input_idx: Option<usize>,
    project: &Project,
    filters: &FilterPipeline,
    settings: &ExportSettings,
) {
    if settings.audio_processing_mode == AudioProcessingMode::NoAudio {
        cmd.arg("-an");
    } else {
        let speed_factor = if settings.processing_mode == VideoProcessingMode::FullProcessing {
            filters.effective_speed_factor()
        } else {
            1.0
        };
        let has_speed_change = (speed_factor - 1.0).abs() > 0.005;
        let has_global_audio_filter =
            settings.audio_volume_db.abs() > 0.05 || settings.audio_normalize_loudness;
        let has_resample_or_remix = settings.audio_sample_rate.is_some()
            || settings.audio_channels.channel_count().is_some();

        let mut out_audio_idx = 0usize;
        let mut audio_filter_parts = Vec::new();
        let skip_source_audio =
            ext_audio_input_idx.is_some() && project.replace_source_audio_with_external;

        if !skip_source_audio {
            for audio_cfg in &project.audio_configs {
                if audio_cfg.mode == AudioExportMode::Disabled {
                    continue;
                }
                cmd.args([
                    "-map",
                    &format!("{source_input_index}:{}?", audio_cfg.info.stream_index),
                ]);

                let effective_mode = match settings.audio_processing_mode {
                    AudioProcessingMode::FullProcessing => AudioExportMode::ReEncode {
                        codec: settings.audio_encoder.clone(),
                        bitrate_kbps: settings.audio_bitrate_kbps,
                    },
                    AudioProcessingMode::DirectStreamCopy => audio_cfg.mode.clone(),
                    AudioProcessingMode::NoAudio => unreachable!(),
                };

                let requires_reencode = audio_cfg.delay_ms != 0
                    || has_speed_change
                    || has_global_audio_filter
                    || has_resample_or_remix;

                match effective_mode {
                    AudioExportMode::DirectStreamCopy if !requires_reencode => {
                        cmd.args([&format!("-c:a:{out_audio_idx}"), "copy"]);
                    }
                    AudioExportMode::DirectStreamCopy => {
                        let enc = &settings.audio_encoder;
                        cmd.args([&format!("-c:a:{out_audio_idx}"), enc]);
                        if !is_lossless_or_pcm_audio_encoder(enc) {
                            cmd.args([
                                &format!("-b:a:{out_audio_idx}"),
                                &format!("{}k", settings.audio_bitrate_kbps),
                            ]);
                        }
                        if let Some(sr) = settings.audio_sample_rate {
                            cmd.args([&format!("-ar:a:{out_audio_idx}"), &sr.to_string()]);
                        }
                        if let Some(ch) = settings.audio_channels.channel_count() {
                            cmd.args([&format!("-ac:a:{out_audio_idx}"), &ch.to_string()]);
                        }
                    }
                    AudioExportMode::ReEncode {
                        codec,
                        bitrate_kbps,
                    } => {
                        cmd.args([&format!("-c:a:{out_audio_idx}"), &codec]);
                        if !is_lossless_or_pcm_audio_encoder(&codec) {
                            cmd.args([
                                &format!("-b:a:{out_audio_idx}"),
                                &format!("{bitrate_kbps}k"),
                            ]);
                        }
                        if let Some(sr) = settings.audio_sample_rate {
                            cmd.args([&format!("-ar:a:{out_audio_idx}"), &sr.to_string()]);
                        }
                        if let Some(ch) = settings.audio_channels.channel_count() {
                            cmd.args([&format!("-ac:a:{out_audio_idx}"), &ch.to_string()]);
                        }
                    }
                    AudioExportMode::Disabled => unreachable!(),
                }

                if audio_cfg.delay_ms > 0 {
                    audio_filter_parts.push(format!("adelay={}:all=1", audio_cfg.delay_ms));
                } else if audio_cfg.delay_ms < 0 {
                    let trim_secs = (audio_cfg.delay_ms.unsigned_abs() as f64) / 1000.0;
                    audio_filter_parts
                        .push(format!("atrim=start={trim_secs:.3},asetpts=PTS-STARTPTS"));
                }

                out_audio_idx += 1;
            }
        }

        if let Some(ext_idx) = ext_audio_input_idx {
            cmd.args(["-map", &format!("{ext_idx}:a:0?")]);
            let requires_reencode = settings.audio_processing_mode
                == AudioProcessingMode::FullProcessing
                || has_speed_change
                || has_global_audio_filter
                || has_resample_or_remix;
            if requires_reencode {
                let enc = &settings.audio_encoder;
                cmd.args([&format!("-c:a:{out_audio_idx}"), enc]);
                if !is_lossless_or_pcm_audio_encoder(enc) {
                    cmd.args([
                        &format!("-b:a:{out_audio_idx}"),
                        &format!("{}k", settings.audio_bitrate_kbps),
                    ]);
                }
                if let Some(sr) = settings.audio_sample_rate {
                    cmd.args([&format!("-ar:a:{out_audio_idx}"), &sr.to_string()]);
                }
                if let Some(ch) = settings.audio_channels.channel_count() {
                    cmd.args([&format!("-ac:a:{out_audio_idx}"), &ch.to_string()]);
                }
            } else {
                cmd.args([&format!("-c:a:{out_audio_idx}"), "copy"]);
            }
            out_audio_idx += 1;
        }

        if out_audio_idx > 0 {
            if settings.audio_volume_db.abs() > 0.05 {
                audio_filter_parts.push(format!("volume={:.1}dB", settings.audio_volume_db));
            }
            if settings.audio_normalize_loudness {
                audio_filter_parts.push("loudnorm=I=-16:TP=-1.5:LRA=11".to_string());
            }
            if has_speed_change {
                let mut rem = speed_factor.clamp(0.125, 8.0);
                while rem > 2.0 + 1e-4 {
                    audio_filter_parts.push("atempo=2.0".to_string());
                    rem /= 2.0;
                }
                while rem < 0.5 - 1e-4 {
                    audio_filter_parts.push("atempo=0.5".to_string());
                    rem /= 0.5;
                }
                if (rem - 1.0).abs() > 0.005 {
                    audio_filter_parts.push(format!("atempo={rem:.4}"));
                }
            }
        }

        if !audio_filter_parts.is_empty() {
            cmd.args(["-af", &audio_filter_parts.join(",")]);
        }
    }

    // Map enabled subtitle streams (when container supports them)
    if settings.container != ContainerFormat::Avi {
        let mut out_sub_idx = 0usize;
        for sub_cfg in &project.subtitle_configs {
            if !sub_cfg.enabled {
                continue;
            }
            cmd.args([
                "-map",
                &format!("{source_input_index}:{}?", sub_cfg.info.stream_index),
            ]);
            let sub_codec = if settings.container == ContainerFormat::Mp4
                || settings.container == ContainerFormat::Mov
            {
                "mov_text"
            } else {
                "copy"
            };
            cmd.args([&format!("-c:s:{out_sub_idx}"), sub_codec]);
            out_sub_idx += 1;
        }
    }

    if settings.preserve_metadata {
        cmd.args(["-map_metadata", &source_input_index.to_string()]);
    }
}

fn map_vvenc_preset(preset: &str) -> &'static str {
    match preset {
        "ultrafast" | "superfast" | "veryfast" | "faster" => "faster",
        "fast" => "fast",
        "medium" => "medium",
        "slow" => "slow",
        "slower" | "veryslow" => "slower",
        _ => "fast",
    }
}

fn matching_smart_encoder(source_codec: &str, fallback_encoder: &str) -> String {
    let codec = source_codec.to_lowercase();
    let fb = fallback_encoder.to_lowercase();
    match codec.as_str() {
        "h264" | "avc" => {
            if fb.starts_with("h264_") {
                fallback_encoder.to_string()
            } else {
                "libx264".to_string()
            }
        }
        "hevc" | "h265" => {
            if fb.starts_with("hevc_") {
                fallback_encoder.to_string()
            } else {
                "libx265".to_string()
            }
        }
        "vvc" | "h266" => "libvvenc".to_string(),
        "vp9" => {
            if fb.starts_with("vp9_") {
                fallback_encoder.to_string()
            } else {
                "libvpx-vp9".to_string()
            }
        }
        "av1" => {
            if fb.starts_with("av1_") {
                fallback_encoder.to_string()
            } else {
                "libsvtav1".to_string()
            }
        }
        "mpeg4" => "mpeg4".to_string(),
        _ => fallback_encoder.to_string(),
    }
}

fn ffmpeg_has_native_encoder(ffmpeg_path: &Path, encoder_name: &str) -> bool {
    let mut cmd = Command::new(ffmpeg_path);
    cmd.args(["-hide_banner", "-encoders"]);
    configure_background_command(&mut cmd);
    if let Ok(output) = cmd.output() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let mut parts = line.trim().split_whitespace();
            let _flags = parts.next();
            if let Some(name) = parts.next() {
                if name == encoder_name {
                    return true;
                }
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn execute_single_segment_task(
    binaries: &FfmpegBinaries,
    project: &Project,
    seg: &EditSegment,
    filters: &FilterPipeline,
    settings: &ExportSettings,
    output_path: &Path,
    total_duration: TimestampUs,
    base_completed: TimestampUs,
    cancel_flag: &Arc<AtomicBool>,
    tx: &mpsc::Sender<ExportEvent>,
) -> Result<(), ExportError> {
    let is_vvenc = matches!(
        settings.video_encoder.as_str(),
        "libvvenc" | "vvenc" | "vvencapp"
    );
    if settings.processing_mode != VideoProcessingMode::DirectStreamCopy
        && is_vvenc
        && !ffmpeg_has_native_encoder(&binaries.ffmpeg_path, "libvvenc")
    {
        if let Some(vvencapp_path) = &binaries.vvencapp_path {
            return run_external_vvenc_segment_export(
                binaries,
                vvencapp_path,
                project,
                seg,
                filters,
                settings,
                output_path,
                total_duration,
                base_completed,
                cancel_flag,
                tx,
            );
        }
    }

    let cmd = build_single_segment_command(
        binaries,
        project,
        seg,
        filters,
        settings,
        output_path,
    )?;
    run_ffmpeg_with_progress(cmd, total_duration, base_completed, cancel_flag, tx)
}

/// Executes an H.266 / VVC encode using the system `vvencapp` (`libvvenc`) binary when
/// `ffmpeg` was compiled without `--enable-libvvenc`:
/// 1. Decodes and filters the segment in `ffmpeg`, piping raw `yuv420p` frames into `vvencapp`.
/// 2. Tracks per-frame `POC` emitted by `vvencapp` and streams live progress updates to the UI.
/// 3. Wraps the Annex-B `.266` stream into a temporary MP4 and patches ISO-BMFF `ctts` v1
///    signed composition offsets `(POC - dts_index) * sample_delta` for 100% B-frame PTS accuracy.
/// 4. Muxes the timestamp-exact VVC video stream with audio and subtitle streams into `output_path`.
#[allow(clippy::too_many_arguments)]
fn run_external_vvenc_segment_export(
    binaries: &FfmpegBinaries,
    vvencapp_path: &Path,
    project: &Project,
    seg: &EditSegment,
    filters: &FilterPipeline,
    settings: &ExportSettings,
    output_path: &Path,
    total_duration: TimestampUs,
    base_completed: TimestampUs,
    cancel_flag: &Arc<AtomicBool>,
    tx: &mpsc::Sender<ExportEvent>,
) -> Result<(), ExportError> {
    let src = project
        .source_by_id(seg.source_id)
        .ok_or(ExportError::EmptyTimeline)?;

    let start_pts = src.timing.frame_to_timestamp(seg.start_frame);
    let end_pts = src.timing.frame_to_timestamp(seg.end_frame);
    let duration_secs = ((end_pts.0 - start_pts.0).max(1_000)) as f64 / 1_000_000.0;

    let (mut vf_chain, out_w, out_h) = filters.to_ffmpeg_filtergraph(src.width, src.height);
    let (mut final_w, mut final_h) = (out_w, out_h);
    if let Some((req_w, req_h)) = settings.custom_resolution {
        let w = req_w.max(2) & !1;
        let h = req_h.max(2) & !1;
        final_w = w;
        final_h = h;
        if w != out_w || h != out_h {
            let scale_expr = format!("scale={w}:{h}");
            vf_chain = Some(match vf_chain {
                Some(existing) => format!("{existing},{scale_expr}"),
                None => scale_expr,
            });
        }
    }
    final_w = final_w.max(2) & !1;
    final_h = final_h.max(2) & !1;

    // Determine exact output framerate rational (fps_num / fps_den)
    let (fps_num, fps_den) = if let Some(cfps) = settings.custom_fps.filter(|&f| f > 0.0) {
        ((cfps * 1000.0).round() as i64, 1000i64)
    } else {
        (
            src.timing.nominal_fps.num.max(1),
            src.timing.nominal_fps.den.max(1),
        )
    };
    let fps_f64 = (fps_num as f64) / (fps_den as f64);

    let temp_dir = tempfile::TempDir::new()?;
    let raw_vvc_path = temp_dir.path().join("encoded.266");
    let temp_vvc_mp4 = temp_dir.path().join("video_pts.mp4");

    // 1. Spawn FFmpeg decoder + filtergraph piping raw yuv420p to stdout
    let mut dec_cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut dec_cmd);
    dec_cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if settings.use_hwaccel_decode {
        dec_cmd.args(["-hwaccel", "auto"]);
    } else {
        dec_cmd.args(["-threads", "0", "-thread_type", "frame+slice"]);
    }
    dec_cmd.args(["-filter_threads", "0", "-filter_complex_threads", "0"]);
    if start_pts.0 > 0 {
        dec_cmd
            .arg("-ss")
            .arg(format!("{:.6}", start_pts.as_seconds_f64()));
    }
    dec_cmd.arg("-i").arg(&src.path);
    dec_cmd.arg("-t").arg(format!("{duration_secs:.6}"));
    dec_cmd.args(["-map", &format!("0:{}", src.video_stream_index)]);
    if let Some(ref vf) = vf_chain {
        dec_cmd.args(["-vf", vf]);
    }
    dec_cmd.args([
        "-r",
        &format!("{fps_num}/{fps_den}"),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "yuv420p",
        "pipe:1",
    ]);
    dec_cmd.stdout(Stdio::piped());
    dec_cmd.stderr(Stdio::piped());

    let mut dec_child = dec_cmd.spawn()?;
    let dec_stdout = dec_child
        .stdout
        .take()
        .ok_or_else(|| ExportError::FfmpegFailed("Missing decoder stdout pipe".into()))?;

    // 2. Spawn vvencapp reading from dec_stdout
    let logical_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .clamp(2, 16);
    let vvenc_preset = map_vvenc_preset(&settings.encoder_preset);
    let qp = settings.crf.clamp(1, 63);
    let active_audio_tracks = project
        .audio_configs
        .iter()
        .filter(|a| a.mode != AudioExportMode::Disabled)
        .count();
    let target_v_kbps = settings.effective_video_bitrate_kbps(duration_secs, active_audio_tracks);

    let mut enc_cmd = Command::new(vvencapp_path);
    configure_background_command(&mut enc_cmd);
    enc_cmd.args([
        "-i",
        "-",
        "-s",
        &format!("{final_w}x{final_h}"),
        "-c",
        "yuv420",
        "--fps",
        &format!("{fps_num}/{fps_den}"),
        "--preset",
        vvenc_preset,
    ]);
    if let Some(kbps) = target_v_kbps {
        enc_cmd.args(["-b", &format!("{kbps}k")]);
    } else {
        enc_cmd.args(["-q", &qp.to_string(), "--qpa", "1"]);
    }
    enc_cmd
        .args([
            "-t",
            &logical_cpus.to_string(),
            "--additional",
            "POC0IDR=1",
            "-v",
            "5",
            "-o",
        ])
        .arg(&raw_vvc_path)
        .stdin(Stdio::from(dec_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut enc_child = enc_cmd.spawn()?;
    let enc_stdout = enc_child
        .stdout
        .take()
        .ok_or_else(|| ExportError::FfmpegFailed("Missing vvencapp stdout pipe".into()))?;

    let start_instant = std::time::Instant::now();
    let reader = BufReader::new(enc_stdout);
    let mut pocs: Vec<i64> = Vec::new();
    let mut last_error_line = String::new();

    for line_res in reader.lines() {
        if cancel_flag.load(Ordering::SeqCst) {
            let _ = enc_child.kill();
            let _ = dec_child.kill();
            let _ = enc_child.wait();
            let _ = dec_child.wait();
            return Err(ExportError::Cancelled);
        }
        let Ok(line) = line_res else { break };
        let trimmed = line.trim();
        if trimmed.to_lowercase().contains("error") {
            last_error_line = trimmed.to_string();
        }
        if let Some(rest) = trimmed.strip_prefix("POC") {
            if let Some(poc_str) = rest.split_whitespace().next() {
                if let Ok(poc) = poc_str.parse::<i64>() {
                    pocs.push(poc);
                    let encoded_frames = pocs.len() as f64;
                    let cur_time_us = ((encoded_frames / fps_f64) * 1_000_000.0).round() as i64;
                    let elapsed = start_instant.elapsed().as_secs_f32().max(0.001);
                    let cur_fps = (encoded_frames as f32) / elapsed;
                    let speed_x = (cur_fps as f64) / fps_f64;
                    let cur_size = fs::metadata(&raw_vvc_path).map(|m| m.len()).unwrap_or(0);
                    let encoded = TimestampUs(base_completed.0 + cur_time_us);
                    let fraction =
                        (encoded.0 as f32 / total_duration.0.max(1) as f32).clamp(0.0, 0.98);
                    let _ = tx.send(ExportEvent::Progress {
                        fraction,
                        encoded_time: encoded,
                        total_duration,
                        fps: cur_fps,
                        speed: format!("{speed_x:.2}x"),
                        size_bytes: cur_size,
                    });
                }
            }
        }
    }

    let enc_status = enc_child.wait()?;
    let _ = dec_child.wait();
    if !enc_status.success() {
        return Err(ExportError::FfmpegFailed(format!(
            "vvencapp H.266/VVC encode failed: {}",
            if last_error_line.is_empty() {
                format!("exit status {enc_status}")
            } else {
                last_error_line
            }
        )));
    }

    // 3. Wrap raw .266 bitstream into a temporary MP4 and patch ctts v1 signed composition offsets
    let bsf_expr = format!(
        "setts=dts=N/(({fps_num}/{fps_den})*TB):pts=(N+mod(N\\,2))/(({fps_num}/{fps_den})*TB)"
    );
    let mut wrap_cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut wrap_cmd);
    wrap_cmd
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-r",
            &format!("{fps_num}/{fps_den}"),
            "-i",
        ])
        .arg(&raw_vvc_path)
        .args(["-c:v", "copy", "-bsf:v", &bsf_expr])
        .arg(&temp_vvc_mp4);

    let wrap_out = wrap_cmd.output()?;
    if !wrap_out.status.success() {
        return Err(ExportError::FfmpegFailed(
            String::from_utf8_lossy(&wrap_out.stderr).trim().to_string(),
        ));
    }

    patch_mp4_ctts_from_pocs(&temp_vvc_mp4, &pocs)?;

    // 4. Final mux: combine timestamp-accurate VVC video stream with audio & subtitle streams
    let mut mux_cmd = Command::new(&binaries.ffmpeg_path);
    configure_background_command(&mut mux_cmd);
    mux_cmd
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&temp_vvc_mp4);

    if start_pts.0 > 0 {
        mux_cmd
            .arg("-ss")
            .arg(format!("{:.6}", start_pts.as_seconds_f64()));
    }
    mux_cmd.arg("-i").arg(&src.path);
    let mut ext_audio_input_idx = None;
    if let Some(ext_audio) = &project.external_audio_path {
        if ext_audio.exists() {
            mux_cmd.arg("-i").arg(ext_audio);
            ext_audio_input_idx = Some(2);
        }
    }
    mux_cmd.arg("-t").arg(format!("{duration_secs:.6}"));

    // Map video from input 0 (temp_vvc_mp4)
    mux_cmd.args(["-map", "0:v:0", "-c:v", "copy"]);

    // Map enabled audio & subtitle streams from input 1 (src.path) and optional input 2 (ext_audio)
    append_audio_and_subtitle_args(&mut mux_cmd, 1, ext_audio_input_idx, project, filters, settings);

    if settings.container == ContainerFormat::Mp4 || settings.container == ContainerFormat::Mov {
        mux_cmd.args(["-movflags", "+faststart"]);
    }

    mux_cmd.arg(output_path);
    let mux_out = mux_cmd.output()?;
    if !mux_out.status.success() {
        return Err(ExportError::FfmpegFailed(
            String::from_utf8_lossy(&mux_out.stderr).trim().to_string(),
        ));
    }

    Ok(())
}

/// Patches the ISO-BMFF `ctts` (Composition Time to Sample) box in `mp4_path` in-place
/// using the exact Picture Order Counts (`pocs`) emitted by `vvencapp`.
fn patch_mp4_ctts_from_pocs(mp4_path: &Path, pocs: &[i64]) -> Result<(), ExportError> {
    if pocs.len() <= 1 {
        return Ok(());
    }
    let mut data = fs::read(mp4_path)?;
    let Some(stts_idx) = data.windows(4).position(|w| w == b"stts") else {
        return Ok(());
    };
    if stts_idx + 20 > data.len() {
        return Ok(());
    }
    let sample_delta = u32::from_be_bytes([
        data[stts_idx + 16],
        data[stts_idx + 17],
        data[stts_idx + 18],
        data[stts_idx + 19],
    ]) as i64;

    let Some(ctts_idx) = data.windows(4).position(|w| w == b"ctts") else {
        return Ok(());
    };
    if ctts_idx + 12 > data.len() {
        return Ok(());
    }
    let ctts_entries = u32::from_be_bytes([
        data[ctts_idx + 8],
        data[ctts_idx + 9],
        data[ctts_idx + 10],
        data[ctts_idx + 11],
    ]) as usize;

    let count = ctts_entries.min(pocs.len());
    if ctts_idx + 12 + count * 8 > data.len() {
        return Ok(());
    }

    // Set ctts version = 1 (signed composition offsets)
    data[ctts_idx + 4] = 1;
    for (k, &poc) in pocs.iter().take(count).enumerate() {
        let offset = ((poc - k as i64) * sample_delta) as i32;
        let pos = ctts_idx + 12 + k * 8;
        data[pos..pos + 4].copy_from_slice(&1u32.to_be_bytes());
        data[pos + 4..pos + 8].copy_from_slice(&offset.to_be_bytes());
    }

    fs::write(mp4_path, &data)?;
    Ok(())
}

fn run_ffmpeg_with_progress(
    mut cmd: Command,
    total_duration: TimestampUs,
    base_completed: TimestampUs,
    cancel_flag: &Arc<AtomicBool>,
    tx: &mpsc::Sender<ExportEvent>,
) -> Result<(), ExportError> {
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExportError::FfmpegFailed("Missing stdout pipe".into()))?;
    let mut stderr = child.stderr.take();

    let reader = BufReader::new(stdout);
    let mut cur_fps = 0.0f32;
    let mut cur_speed = String::from("1.0x");
    let mut cur_size = 0u64;
    let mut cur_time_us = 0i64;

    for line_res in reader.lines() {
        if cancel_flag.load(Ordering::SeqCst) {
            warn!("Cancelling active FFmpeg export process");
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExportError::Cancelled);
        }
        let Ok(line) = line_res else { break };
        let Some((key, val)) = line.trim().split_once('=') else {
            continue;
        };
        match key {
            "fps" => {
                cur_fps = val.parse().unwrap_or(0.0);
            }
            "speed" => {
                cur_speed = val.trim().to_string();
            }
            "total_size" => {
                cur_size = val.parse().unwrap_or(0);
            }
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = val.parse::<i64>() {
                    cur_time_us = us.max(0);
                }
            }
            "progress" => {
                let encoded = TimestampUs(base_completed.0 + cur_time_us);
                let fraction =
                    (encoded.0 as f32 / total_duration.0.max(1) as f32).clamp(0.0, 1.0);
                let _ = tx.send(ExportEvent::Progress {
                    fraction,
                    encoded_time: encoded,
                    total_duration,
                    fps: cur_fps,
                    speed: cur_speed.clone(),
                    size_bytes: cur_size,
                });
            }
            _ => {}
        }
    }

    if cancel_flag.load(Ordering::SeqCst) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ExportError::Cancelled);
    }

    let status = child.wait()?;
    if !status.success() {
        let mut err_msg = String::new();
        if let Some(mut s) = stderr.take() {
            let _ = s.read_to_string(&mut err_msg);
        }
        return Err(ExportError::FfmpegFailed(
            err_msg.trim().to_string(),
        ));
    }

    // Small settle delay to ensure OS buffers flush
    thread::sleep(Duration::from_millis(10));
    Ok(())
}

// ============================================================================
// Phase 3: Batch Processing Queue
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BatchItemStatus {
    Pending,
    Running { fraction: f32 },
    Completed { duration: TimestampUs },
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchJobItem {
    pub id: usize,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub status: BatchItemStatus,
}

#[derive(Debug, Clone)]
pub enum BatchEvent {
    ItemStarted { index: usize },
    ItemProgress { index: usize, fraction: f32 },
    ItemCompleted { index: usize, duration: TimestampUs },
    ItemFailed { index: usize, error: String },
    AllFinished,
    Cancelled,
}

pub struct BatchJobHandle {
    pub receiver: Receiver<BatchEvent>,
    cancel_flag: Arc<AtomicBool>,
}

impl BatchJobHandle {
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }
}

/// Runs a batch of video files through the given `FilterPipeline` and `ExportSettings` asynchronously.
pub fn start_batch_queue(
    binaries: FfmpegBinaries,
    items: Vec<BatchJobItem>,
    filters: FilterPipeline,
    mut settings: ExportSettings,
) -> Result<BatchJobHandle, ExportError> {
    if items.is_empty() {
        return Err(ExportError::EmptyTimeline);
    }
    // Batch items process full files unless scripted otherwise
    settings.export_selection_only = false;

    let (tx, rx) = mpsc::channel::<BatchEvent>();
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_clone = Arc::clone(&cancel_flag);

    thread::Builder::new()
        .name("cutdub-batch-worker".into())
        .spawn(move || {
            for (idx, item) in items.iter().enumerate() {
                if cancel_clone.load(Ordering::SeqCst) {
                    let _ = tx.send(BatchEvent::Cancelled);
                    return;
                }
                let _ = tx.send(BatchEvent::ItemStarted { index: idx });

                let clip = match probe_media_file(&binaries, &item.input_path, false) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(BatchEvent::ItemFailed {
                            index: idx,
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                let project = Project::from_primary_source(clip);
                let segments = project.effective_export_segments(false);

                let (inner_tx, inner_rx) = mpsc::channel::<ExportEvent>();
                let bins_clone = binaries.clone();
                let proj_clone = project.clone();
                let filt_clone = filters.clone();
                let set_clone = settings.clone();
                let out_path = item.output_path.clone();
                let flag_inner = Arc::clone(&cancel_clone);

                let worker = thread::spawn(move || {
                    execute_export(
                        &bins_clone,
                        &proj_clone,
                        &segments,
                        &filt_clone,
                        &set_clone,
                        &out_path,
                        &flag_inner,
                        &inner_tx,
                    )
                });

                while let Ok(ev) = inner_rx.recv() {
                    if let ExportEvent::Progress { fraction, .. } = ev {
                        let _ = tx.send(BatchEvent::ItemProgress {
                            index: idx,
                            fraction,
                        });
                    }
                }

                match worker.join() {
                    Ok(Ok(dur)) => {
                        let _ = tx.send(BatchEvent::ItemCompleted {
                            index: idx,
                            duration: dur,
                        });
                    }
                    Ok(Err(ExportError::Cancelled)) => {
                        let _ = fs::remove_file(&item.output_path);
                        let _ = tx.send(BatchEvent::Cancelled);
                        return;
                    }
                    Ok(Err(e)) => {
                        let _ = fs::remove_file(&item.output_path);
                        let _ = tx.send(BatchEvent::ItemFailed {
                            index: idx,
                            error: e.to_string(),
                        });
                    }
                    Err(_) => {
                        let _ = tx.send(BatchEvent::ItemFailed {
                            index: idx,
                            error: "Worker thread panicked".into(),
                        });
                    }
                }
            }
            let _ = tx.send(BatchEvent::AllFinished);
        })
        .map_err(ExportError::Io)?;

    Ok(BatchJobHandle {
        receiver: rx,
        cancel_flag,
    })
}

// ============================================================================
// Phase 3: Scripting & Headless Automation
// ============================================================================

/// Non-destructive editing step in a declarative CutDub automation script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScriptEditOp {
    /// Trims timeline to inclusive frame range `start_frame..=end_frame`.
    TrimFrames { start_frame: u64, end_frame: u64 },
    /// Deletes inclusive frame range `start_frame..=end_frame`.
    DeleteFrames { start_frame: u64, end_frame: u64 },
    /// Trims timeline to timestamp range `start_time..=end_time` (`HH:MM:SS.mmm`).
    TrimTimestamps {
        start_time: String,
        end_time: String,
    },
}

/// Declarative automation script (`.cutdub-script.json`) for reproducible CLI or batch workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutomationScript {
    pub name: String,
    pub primary_input: PathBuf,
    #[serde(default)]
    pub append_inputs: Vec<PathBuf>,
    #[serde(default)]
    pub operations: Vec<ScriptEditOp>,
    #[serde(default)]
    pub filters: FilterPipeline,
    pub export_settings: ExportSettings,
    pub output_path: PathBuf,
}

impl AutomationScript {
    /// Captures the current project, filter pipeline, and export settings into a reusable script.
    pub fn from_current_session(
        name: impl Into<String>,
        project: &Project,
        filters: &FilterPipeline,
        export_settings: &ExportSettings,
        output_path: PathBuf,
    ) -> Option<Self> {
        let primary = project.primary_source()?;
        let append_inputs = project
            .sources
            .iter()
            .skip(1)
            .map(|s| s.path.clone())
            .collect();
        let mut operations = Vec::new();
        if let Some((sel_s, sel_e)) = project.selection_range() {
            if export_settings.export_selection_only {
                operations.push(ScriptEditOp::TrimFrames {
                    start_frame: sel_s,
                    end_frame: sel_e.saturating_sub(1),
                });
            }
        }
        let mut settings = export_settings.clone();
        settings.export_selection_only = false;

        Some(Self {
            name: name.into(),
            primary_input: primary.path.clone(),
            append_inputs,
            operations,
            filters: filters.clone(),
            export_settings: settings,
            output_path,
        })
    }

    pub fn save_to_file(&self, path: &Path) -> Result<(), ExportError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Result<Self, ExportError> {
        let bytes = fs::read(path)?;
        let script: Self = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(script)
    }
}

/// Executes an `AutomationScript` synchronously (ideal for CLI `--script` automation).
pub fn execute_automation_script(
    binaries: &FfmpegBinaries,
    script: &AutomationScript,
) -> Result<TimestampUs, ExportError> {
    let primary_clip = probe_media_file(binaries, &script.primary_input, true)
        .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
    let mut project = Project::from_primary_source(primary_clip);

    for extra_path in &script.append_inputs {
        let extra_clip = probe_media_file(binaries, extra_path, true)
            .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
        project
            .append_source(extra_clip)
            .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
    }

    for op in &script.operations {
        match op {
            ScriptEditOp::TrimFrames {
                start_frame,
                end_frame,
            } => {
                project.current_frame = *start_frame;
                project.set_mark_in();
                project.current_frame = *end_frame;
                project.set_mark_out();
                project
                    .trim_to_selection()
                    .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
            }
            ScriptEditOp::DeleteFrames {
                start_frame,
                end_frame,
            } => {
                project.current_frame = *start_frame;
                project.set_mark_in();
                project.current_frame = *end_frame;
                project.set_mark_out();
                project
                    .delete_selection()
                    .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
            }
            ScriptEditOp::TrimTimestamps {
                start_time,
                end_time,
            } => {
                let s_ts = TimestampUs::parse_str(start_time).unwrap_or(TimestampUs::ZERO);
                let e_ts = TimestampUs::parse_str(end_time).unwrap_or(project.total_duration());
                project.current_frame = project.virtual_timestamp_to_frame(s_ts);
                project.set_mark_in();
                project.current_frame = project.virtual_timestamp_to_frame(e_ts);
                project.set_mark_out();
                project
                    .trim_to_selection()
                    .map_err(|e| ExportError::FfmpegFailed(e.to_string()))?;
            }
        }
    }

    let mut settings = script.export_settings.clone();
    settings.export_selection_only = false;
    let segments = project.effective_export_segments(false);
    let (tx, _rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));

    execute_export(
        binaries,
        &project,
        &segments,
        &script.filters,
        &settings,
        &script.output_path,
        &cancel,
        &tx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use redub_filters::{FilterKind, ScalingAlgorithm};
    use redub_media::{generate_synthetic_video, probe_media_file};
    use tempfile::TempDir;

    fn capabilities(names: &[&str]) -> MediaCapabilities {
        MediaCapabilities {
            encoder_names: names.iter().map(|name| name.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn presets_require_installed_encoders_and_av1_can_use_aom() {
        let presets = EncodingPreset::built_in_presets();
        assert!(presets.len() >= 12);
        assert!(!presets.iter().any(|p| p.settings.video_encoder == "libvpx-vp9"));
        let none = MediaCapabilities::default();
        assert_eq!(presets.iter().filter(|p| p.available_with(&none).is_some()).count(), 2);
        let webm = presets.iter().find(|p| p.name.starts_with("WebM AV1")).unwrap();
        assert!(webm.available_with(&capabilities(&["libsvtav1"])).is_none());
        let available = webm.available_with(&capabilities(&["libaom-av1", "libopus"])).unwrap();
        assert_eq!(available.settings.video_encoder, "libaom-av1");
        assert_eq!(available.settings.audio_processing_mode, AudioProcessingMode::FullProcessing);
        let vvc = presets.iter().find(|p| p.settings.video_encoder == "libvvenc").unwrap();
        assert!(vvc.available_with(&none).is_none());
        assert!(vvc.available_with(&capabilities(&["libvvenc"])).is_some());
    }

    #[test]
    fn saved_presets_get_new_templates_without_losing_custom_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("presets.json");
        let legacy = EncodingPreset {
            name: "WebM VP9 (CRF 28)".into(),
            settings: ExportSettings { video_encoder: "libvpx-vp9".into(), ..Default::default() },
        };
        let custom = EncodingPreset { name: "My VP9 Settings".into(), ..legacy.clone() };
        EncodingPreset::save_all(&path, &[legacy, custom]).unwrap();
        let loaded = EncodingPreset::load_all(&path);
        assert!(!loaded.iter().any(|p| p.name == "WebM VP9 (CRF 28)"));
        assert!(loaded.iter().any(|p| p.name == "My VP9 Settings"));
        assert!(loaded.iter().any(|p| p.name.starts_with("WebM AV1")));
        assert_eq!(loaded.len(), EncodingPreset::built_in_presets().len() + 1);
        EncodingPreset::save_all(&path, &loaded).unwrap();
        assert_eq!(EncodingPreset::load_all(&path).len(), loaded.len());
    }

    #[test]
    fn av1_webm_template_exports_with_opus_when_installed() {
        let bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
        let caps = MediaCapabilities::detect(&bins);
        let preset = EncodingPreset::built_in_presets().into_iter()
            .find(|p| p.name.starts_with("WebM AV1")).unwrap();
        let Some(preset) = preset.available_with(&caps) else { return; };
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input.mp4");
        generate_synthetic_video(&bins, &input, 128, 96, 10, 0.5, 5).unwrap();
        let project = Project::from_primary_source(probe_media_file(&bins, &input, true).unwrap());
        let output = tmp.path().join("av1.webm");
        let handle = start_export_job(bins.clone(), project, FilterPipeline::new(),
            preset.settings, output.clone()).unwrap();
        loop {
            match handle.receiver.recv_timeout(Duration::from_secs(30)).unwrap() {
                ExportEvent::Completed { .. } => break,
                ExportEvent::Failed(error) => panic!("AV1 template failed: {error}"),
                _ => {}
            }
        }
        let clip = probe_media_file(&bins, &output, true).unwrap();
        assert_eq!(clip.video_codec, "av1");
        assert_eq!(clip.audio_streams[0].codec_name, "opus");
    }

    #[test]
    fn test_direct_stream_copy_and_full_processing_exports() {
        let bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
        let tmp = TempDir::new().unwrap();
        let src_path = tmp.path().join("source.mp4");
        generate_synthetic_video(&bins, &src_path, 320, 240, 25, 2.0, 10).unwrap();

        let clip = probe_media_file(&bins, &src_path, true).unwrap();
        let mut project = Project::from_primary_source(clip);

        // Select frames 10..=39 (30 frames = 1.2s)
        project.current_frame = 10;
        project.set_mark_in();
        project.current_frame = 39;
        project.set_mark_out();

        // 1. Test Direct Stream Copy export
        let copy_out = tmp.path().join("out_copy.mp4");
        let settings_copy = ExportSettings {
            processing_mode: VideoProcessingMode::DirectStreamCopy,
            export_selection_only: true,
            ..ExportSettings::default()
        };
        let handle = start_export_job(
            bins.clone(),
            project.clone(),
            FilterPipeline::new(),
            settings_copy,
            copy_out.clone(),
        )
        .unwrap();

        let mut finished = false;
        while let Ok(ev) = handle.receiver.recv() {
            match ev {
                ExportEvent::Completed { .. } => {
                    finished = true;
                    break;
                }
                ExportEvent::Failed(e) => panic!("Direct Stream Copy failed: {e}"),
                _ => {}
            }
        }
        assert!(finished);
        let copy_probed = probe_media_file(&bins, &copy_out, true).unwrap();
        assert_eq!(copy_probed.width, 320);
        assert!(copy_probed.timing.total_frames() >= 25);

        // 1b. Test SmartCopy export starting on a non-keyframe (frame 15; keyframe interval is 10)
        project.current_frame = 15;
        project.set_mark_in();
        project.current_frame = 39;
        project.set_mark_out();
        let smart_out = tmp.path().join("out_smart.mp4");
        let settings_smart = ExportSettings {
            processing_mode: VideoProcessingMode::SmartCopy,
            export_selection_only: true,
            encoder_preset: "ultrafast".into(),
            ..ExportSettings::default()
        };
        let handle_smart = start_export_job(
            bins.clone(),
            project.clone(),
            FilterPipeline::new(),
            settings_smart,
            smart_out.clone(),
        )
        .unwrap();
        let mut finished_smart = false;
        while let Ok(ev) = handle_smart.receiver.recv() {
            match ev {
                ExportEvent::Completed { .. } => {
                    finished_smart = true;
                    break;
                }
                ExportEvent::Failed(e) => panic!("SmartCopy export failed: {e}"),
                _ => {}
            }
        }
        assert!(finished_smart);
        let smart_probed = probe_media_file(&bins, &smart_out, true).unwrap();
        assert_eq!(smart_probed.width, 320);
        assert!(smart_probed.timing.total_frames() >= 22);

        // 2. Test Full Processing export with Resize filter
        let full_out = tmp.path().join("out_full.mkv");
        let mut filters = FilterPipeline::new();
        filters.add(FilterKind::new_resize(
            160,
            120,
            ScalingAlgorithm::Bicubic,
        ));
        let settings_full = ExportSettings {
            processing_mode: VideoProcessingMode::FullProcessing,
            export_selection_only: true,
            container: ContainerFormat::Mkv,
            video_encoder: "libx264".into(),
            crf: 20,
            encoder_preset: "ultrafast".into(),
            ..ExportSettings::default()
        };
        let handle2 = start_export_job(
            bins.clone(),
            project,
            filters,
            settings_full,
            full_out.clone(),
        )
        .unwrap();

        let mut finished2 = false;
        while let Ok(ev) = handle2.receiver.recv() {
            match ev {
                ExportEvent::Completed { .. } => {
                    finished2 = true;
                    break;
                }
                ExportEvent::Failed(e) => panic!("Full Processing export failed: {e}"),
                _ => {}
            }
        }
        assert!(finished2);
        let full_probed = probe_media_file(&bins, &full_out, true).unwrap();
        assert_eq!(full_probed.width, 160);
        assert_eq!(full_probed.height, 120);

        // 3. Test AutomationScript save/load and headless execution
        let script_out = tmp.path().join("scripted_out.mp4");
        let mut script_filters = FilterPipeline::new();
        script_filters.add(FilterKind::Grayscale);
        let script = AutomationScript {
            name: "Grayscale Trim Script".into(),
            primary_input: src_path.clone(),
            append_inputs: vec![],
            operations: vec![ScriptEditOp::TrimFrames {
                start_frame: 5,
                end_frame: 24,
            }],
            filters: script_filters,
            export_settings: ExportSettings {
                processing_mode: VideoProcessingMode::FullProcessing,
                container: ContainerFormat::Mp4,
                video_encoder: "libx264".into(),
                crf: 22,
                encoder_preset: "ultrafast".into(),
                ..ExportSettings::default()
            },
            output_path: script_out.clone(),
        };
        let script_json_path = tmp.path().join("job.cutdub-script.json");
        script.save_to_file(&script_json_path).unwrap();
        let loaded_script = AutomationScript::load_from_file(&script_json_path).unwrap();
        let dur = execute_automation_script(&bins, &loaded_script).unwrap();
        assert!(dur.0 > 500_000);
        assert!(script_out.is_file());

        // 4. Test H.266 / VVC (libvvenc / vvencapp) export if available on this machine
        if bins.vvencapp_path.is_some() {
            let vvc_out = tmp.path().join("out_vvc.mp4");
            let vvc_clip = probe_media_file(&bins, &src_path, true).unwrap();
            let vvc_project = Project::from_primary_source(vvc_clip);
            let mut vvc_settings = ExportSettings::default();
            vvc_settings.processing_mode = VideoProcessingMode::FullProcessing;
            vvc_settings.export_selection_only = false;
            vvc_settings.apply_recommended_defaults_for_encoder("libvvenc");
            vvc_settings.encoder_preset = "faster".into();

            let vvc_handle = start_export_job(
                bins.clone(),
                vvc_project,
                FilterPipeline::new(),
                vvc_settings,
                vvc_out.clone(),
            )
            .unwrap();

            let mut vvc_finished = false;
            while let Ok(ev) = vvc_handle.receiver.recv() {
                match ev {
                    ExportEvent::Completed { .. } => {
                        vvc_finished = true;
                        break;
                    }
                    ExportEvent::Failed(e) => panic!("VVC export failed: {e}"),
                    _ => {}
                }
            }
            assert!(vvc_finished);
            let vvc_probed = probe_media_file(&bins, &vvc_out, true).unwrap();
            assert_eq!(vvc_probed.video_codec, "vvc");
            assert_eq!(vvc_probed.width, 320);
            assert_eq!(vvc_probed.height, 240);
            assert_eq!(vvc_probed.timing.total_frames(), 50);
        }
    }

    #[test]
    fn test_audio_bitrate_profiles_by_format_samplerate_and_channels() {
        // 1. Lossless codecs
        let flac_prof = audio_bitrate_profile("flac", Some(48_000), 2);
        assert!(flac_prof.is_lossless);
        assert_eq!(flac_prof.max_kbps, 0);

        // 2. MP3 standard stereo (48 kHz, 2ch) vs low-sample rate (24 kHz) vs mono
        let mp3_stereo = audio_bitrate_profile("libmp3lame", Some(48_000), 2);
        assert!(!mp3_stereo.is_lossless);
        assert_eq!(mp3_stereo.min_kbps, 64);
        assert_eq!(mp3_stereo.max_kbps, 320);
        assert_eq!(mp3_stereo.default_kbps, 192);
        assert!(mp3_stereo.presets.contains(&320));

        let mp3_low_sr = audio_bitrate_profile("libmp3lame", Some(22_050), 2);
        assert_eq!(mp3_low_sr.max_kbps, 160, "MPEG-2 Layer 3 max bitrate must be 160 kbps");

        let mp3_mono = audio_bitrate_profile("libmp3lame", Some(44_100), 1);
        assert_eq!(mp3_mono.default_kbps, 96);
        assert_eq!(mp3_mono.max_kbps, 192);

        let mp3_surround = audio_bitrate_profile("libmp3lame", Some(48_000), 6);
        assert!(mp3_surround.notice.is_some(), "MP3 with >2 channels must provide a notice");

        // 3. AC-3 stereo vs 5.1
        let ac3_stereo = audio_bitrate_profile("ac3", Some(48_000), 2);
        assert_eq!(ac3_stereo.default_kbps, 192);
        assert_eq!(ac3_stereo.max_kbps, 448);

        let ac3_51 = audio_bitrate_profile("ac3", Some(48_000), 6);
        assert_eq!(ac3_51.default_kbps, 448);
        assert_eq!(ac3_51.max_kbps, 640);
        assert!(ac3_51.presets.contains(&640));

        let ac3_invalid_sr = audio_bitrate_profile("ac3", Some(96_000), 2);
        assert!(ac3_invalid_sr.notice.is_some(), "AC-3 at 96 kHz must give an advisory notice");

        // 4. E-AC-3 7.1
        let eac3_71 = audio_bitrate_profile("eac3", Some(48_000), 8);
        assert_eq!(eac3_71.max_kbps, 1536);
        assert_eq!(eac3_71.default_kbps, 640);

        // 5. Opus mono vs stereo vs 5.1
        let opus_mono = audio_bitrate_profile("libopus", Some(48_000), 1);
        assert_eq!(opus_mono.default_kbps, 64);
        assert_eq!(opus_mono.max_kbps, 160);

        let opus_stereo = audio_bitrate_profile("libopus", Some(48_000), 2);
        assert_eq!(opus_stereo.default_kbps, 128);
        assert_eq!(opus_stereo.max_kbps, 320);

        let opus_51 = audio_bitrate_profile("libopus", Some(48_000), 6);
        assert_eq!(opus_51.default_kbps, 256);
        assert_eq!(opus_51.max_kbps, 512);

        // 6. AAC low sample rate vs high sample rate
        let aac_24k_mono = audio_bitrate_profile("aac", Some(24_000), 1);
        assert_eq!(aac_24k_mono.max_kbps, 80);

        let aac_96k_stereo = audio_bitrate_profile("aac", Some(96_000), 2);
        assert_eq!(aac_96k_stereo.max_kbps, 512);
        assert_eq!(aac_96k_stereo.default_kbps, 256);

        // 7. Verify ExportSettings clamping
        let mut settings = ExportSettings::default();
        settings.audio_encoder = "libmp3lame".into();
        settings.audio_sample_rate = Some(24_000);
        settings.audio_channels = AudioChannelMode::Stereo;
        settings.audio_bitrate_kbps = 320; // 320 is invalid for 24kHz MP3!
        settings.clamp_audio_bitrate(None, None);
        assert_eq!(settings.audio_bitrate_kbps, 160, "Must clamp to 160 kbps for 24kHz MP3");
    }
}
