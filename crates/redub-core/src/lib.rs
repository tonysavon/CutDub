//! Core domain model for CutDub: 64-bit timestamps, FFmpeg rational time bases,
//! VFR-aware frame timing maps, non-destructive Edit Decision List (EDL),
//! selection management, stream configuration, and undo/redo history.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("No source media is loaded")]
    NoMediaLoaded,
    #[error("Invalid selection range")]
    InvalidSelection,
    #[error("Selection covers the entire timeline; cannot delete all frames")]
    CannotDeleteEntireTimeline,
    #[error("Frame index {0} is out of bounds (total frames: {1})")]
    FrameOutOfBounds(u64, u64),
    #[error("Incompatible media for append: {0}")]
    IncompatibleAppend(String),
    #[error("Project file I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Project serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Rational number representing FFmpeg `time_base` or `r_frame_rate` / `avg_frame_rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub const fn new(num: i64, den: i64) -> Self {
        Self { num, den }
    }

    /// Parses strings like `"30000/1001"`, `"1/90000"`, or `"25"`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some((n, d)) = s.split_once('/') {
            let num = n.trim().parse::<i64>().ok()?;
            let den = d.trim().parse::<i64>().ok()?;
            if den == 0 {
                return None;
            }
            Some(Self { num, den })
        } else if let Ok(num) = s.parse::<i64>() {
            Some(Self { num, den: 1 })
        } else if let Ok(f) = s.parse::<f64>() {
            if f <= 0.0 || !f.is_finite() {
                return None;
            }
            Some(Self {
                num: (f * 1000.0).round() as i64,
                den: 1000,
            })
        } else {
            None
        }
    }

    pub fn as_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }

    /// Converts a raw FFmpeg timestamp in `self` time_base units into signed microseconds (`i64`).
    pub fn ticks_to_microseconds(self, ticks: i64) -> i64 {
        if self.den == 0 {
            return 0;
        }
        ((ticks as i128 * self.num as i128 * 1_000_000i128) / self.den as i128) as i64
    }

    /// Nominal duration of a single frame in microseconds when `self` represents a frame rate (fps).
    pub fn frame_duration_us(self) -> i64 {
        if self.num <= 0 || self.den <= 0 {
            33_333 // fallback ~30fps
        } else {
            ((self.den as i128 * 1_000_000i128) / self.num as i128) as i64
        }
    }
}

impl Default for Rational {
    fn default() -> Self {
        Self { num: 30, den: 1 }
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{}/{} ({:.3})", self.num, self.den, self.as_f64())
        }
    }
}

/// 64-bit signed timestamp in microseconds (10^-6 seconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct TimestampUs(pub i64);

impl TimestampUs {
    pub const ZERO: Self = Self(0);

    pub fn from_seconds_f64(secs: f64) -> Self {
        Self((secs * 1_000_000.0).round() as i64)
    }

    pub fn as_seconds_f64(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }

    /// Formats as `HH:MM:SS.mmm`.
    pub fn format_hms_millis(self) -> String {
        let sign = if self.0 < 0 { "-" } else { "" };
        let total_ms = (self.0.unsigned_abs() + 500) / 1_000;
        let ms = total_ms % 1_000;
        let total_secs = total_ms / 1_000;
        let secs = total_secs % 60;
        let total_mins = total_secs / 60;
        let mins = total_mins % 60;
        let hours = total_mins / 60;
        format!("{sign}{hours:02}:{mins:02}:{secs:02}.{ms:03}")
    }

    /// Parses `HH:MM:SS.mmm`, `MM:SS.mmm`, or `SS.mmm` into a `TimestampUs`.
    pub fn parse_str(input: &str) -> Option<Self> {
        let s = input.trim();
        if s.is_empty() {
            return None;
        }
        let parts: Vec<&str> = s.split(':').collect();
        let secs_f64 = match parts.as_slice() {
            [sec_str] => sec_str.parse::<f64>().ok()?,
            [min_str, sec_str] => {
                let mins = min_str.parse::<u64>().ok()? as f64;
                let secs = sec_str.parse::<f64>().ok()?;
                mins * 60.0 + secs
            }
            [hr_str, min_str, sec_str] => {
                let hrs = hr_str.parse::<u64>().ok()? as f64;
                let mins = min_str.parse::<u64>().ok()? as f64;
                let secs = sec_str.parse::<f64>().ok()?;
                hrs * 3600.0 + mins * 60.0 + secs
            }
            _ => return None,
        };
        if !secs_f64.is_finite() || secs_f64 < 0.0 {
            return None;
        }
        Some(Self::from_seconds_f64(secs_f64))
    }
}

impl fmt::Display for TimestampUs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format_hms_millis())
    }
}

/// Entry for a single video frame in the source stream's presentation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameEntry {
    pub frame_number: u64,
    pub pts_us: TimestampUs,
    pub duration_us: i64,
    pub is_keyframe: bool,
}

/// Maps frame indices to exact presentation timestamps (PTS) and keyframe flags.
/// Supports both variable frame rate (VFR) and constant frame rate (CFR) streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameTimingMap {
    pub entries: Vec<FrameEntry>,
    pub nominal_fps: Rational,
    pub time_base: Rational,
    pub is_vfr: bool,
}

impl FrameTimingMap {
    /// Creates a timing map from an explicit list of frame entries (e.g. from ffprobe packet/frame scan).
    pub fn from_entries(
        mut entries: Vec<FrameEntry>,
        nominal_fps: Rational,
        time_base: Rational,
    ) -> Self {
        entries.sort_by_key(|e| e.pts_us);
        for (idx, entry) in entries.iter_mut().enumerate() {
            entry.frame_number = idx as u64;
        }
        // Fill missing durations from adjacent PTS deltas
        let default_dur = nominal_fps.frame_duration_us();
        for i in 0..entries.len() {
            if entries[i].duration_us <= 0 {
                if i + 1 < entries.len() {
                    let delta = entries[i + 1].pts_us.0 - entries[i].pts_us.0;
                    entries[i].duration_us = delta.max(1);
                } else {
                    entries[i].duration_us = default_dur;
                }
            }
        }
        // Ensure frame 0 is marked as keyframe if none were flagged at start
        if let Some(first) = entries.first_mut() {
            first.is_keyframe = true;
        }

        // Detect VFR by checking if frame durations vary by more than 2ms
        let is_vfr = if entries.len() > 2 {
            let first_dur = entries[0].duration_us;
            entries
                .iter()
                .take(entries.len().saturating_sub(1))
                .any(|e| (e.duration_us - first_dur).abs() > 2_000)
        } else {
            false
        };

        Self {
            entries,
            nominal_fps,
            time_base,
            is_vfr,
        }
    }

    /// Generates a synthetic CFR timing map when only duration and nominal frame rate are initially known,
    /// optionally marking known keyframe timestamps.
    pub fn from_cfr(
        frame_count: u64,
        nominal_fps: Rational,
        time_base: Rational,
        keyframe_interval: u64,
    ) -> Self {
        let count = frame_count.max(1);
        let dur_us = nominal_fps.frame_duration_us();
        let mut entries = Vec::with_capacity(count as usize);
        let kf_step = keyframe_interval.max(1);
        for i in 0..count {
            let pts_us = if nominal_fps.num > 0 {
                ((i as i128 * nominal_fps.den as i128 * 1_000_000i128)
                    / nominal_fps.num as i128) as i64
            } else {
                i as i64 * dur_us
            };
            entries.push(FrameEntry {
                frame_number: i,
                pts_us: TimestampUs(pts_us),
                duration_us: dur_us,
                is_keyframe: i % kf_step == 0,
            });
        }
        Self {
            entries,
            nominal_fps,
            time_base,
            is_vfr: false,
        }
    }

    pub fn total_frames(&self) -> u64 {
        self.entries.len() as u64
    }

    pub fn total_duration_us(&self) -> TimestampUs {
        if let Some(last) = self.entries.last() {
            TimestampUs(last.pts_us.0 + last.duration_us.max(0))
        } else {
            TimestampUs::ZERO
        }
    }

    pub fn frame_entry(&self, frame: u64) -> Option<FrameEntry> {
        self.entries.get(frame as usize).copied()
    }

    pub fn frame_to_timestamp(&self, frame: u64) -> TimestampUs {
        if let Some(entry) = self.entries.get(frame as usize) {
            entry.pts_us
        } else if let Some(last) = self.entries.last() {
            TimestampUs(last.pts_us.0 + last.duration_us)
        } else {
            TimestampUs::ZERO
        }
    }

    /// Binary search to find the frame active at `timestamp`.
    pub fn timestamp_to_frame(&self, timestamp: TimestampUs) -> u64 {
        if self.entries.is_empty() {
            return 0;
        }
        if timestamp <= self.entries[0].pts_us {
            return 0;
        }
        match self
            .entries
            .binary_search_by_key(&timestamp, |entry| entry.pts_us)
        {
            Ok(idx) => idx as u64,
            Err(insert_idx) => (insert_idx.saturating_sub(1)) as u64,
        }
    }

    /// Finds the strictly previous keyframe before `frame`, or `0` if none.
    pub fn prev_keyframe(&self, frame: u64) -> u64 {
        if self.entries.is_empty() || frame == 0 {
            return 0;
        }
        let max_idx = (frame as usize).min(self.entries.len() - 1);
        for idx in (0..max_idx).rev() {
            if self.entries[idx].is_keyframe {
                return idx as u64;
            }
        }
        0
    }

    /// Finds the strictly next keyframe after `frame`, or the last frame if none.
    pub fn next_keyframe(&self, frame: u64) -> u64 {
        if self.entries.is_empty() {
            return 0;
        }
        let start = (frame as usize + 1).min(self.entries.len());
        for idx in start..self.entries.len() {
            if self.entries[idx].is_keyframe {
                return idx as u64;
            }
        }
        (self.entries.len() - 1) as u64
    }

    /// Finds the keyframe at or immediately preceding `frame` (used for keyframe-accurate seeking and stream copy analysis).
    pub fn keyframe_at_or_before(&self, frame: u64) -> u64 {
        if self.entries.is_empty() {
            return 0;
        }
        let start = (frame as usize).min(self.entries.len() - 1);
        for idx in (0..=start).rev() {
            if self.entries[idx].is_keyframe {
                return idx as u64;
            }
        }
        0
    }

    pub fn keyframe_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_keyframe).count()
    }

    /// Returns the first keyframe index in `[start_frame, end_frame)` if one exists.
    pub fn first_keyframe_in_range(&self, start_frame: u64, end_frame: u64) -> Option<u64> {
        let lo = (start_frame as usize).min(self.entries.len());
        let hi = (end_frame as usize).min(self.entries.len());
        if lo >= hi {
            return None;
        }
        self.entries[lo..hi]
            .iter()
            .find(|e| e.is_keyframe)
            .map(|e| e.frame_number)
    }

    /// Replaces synthetic keyframe estimates inside `[window_start, window_end]` with exact
    /// keyframe timestamps discovered from a fast local `ffprobe -read_intervals` scan.
    pub fn apply_local_keyframes(
        &mut self,
        window_start: TimestampUs,
        window_end: TimestampUs,
        keyframe_pts: &[TimestampUs],
    ) {
        if self.entries.is_empty() || keyframe_pts.is_empty() {
            return;
        }
        // Verify at least one probed keyframe maps inside [window_start, window_end]
        // before clearing synthetic keyframe estimates in that window.
        let has_in_window = keyframe_pts.iter().any(|&ts| {
            let idx = self.timestamp_to_frame(ts) as usize;
            self.entries
                .get(idx)
                .map(|e| e.pts_us >= window_start && e.pts_us <= window_end)
                .unwrap_or(false)
        });
        if !has_in_window {
            return;
        }
        for entry in &mut self.entries {
            if entry.pts_us >= window_start && entry.pts_us <= window_end && entry.frame_number > 0 {
                entry.is_keyframe = false;
            }
        }
        for &kf_ts in keyframe_pts {
            let idx = self.timestamp_to_frame(kf_ts) as usize;
            if let Some(entry) = self.entries.get_mut(idx) {
                entry.is_keyframe = true;
            }
        }
        if let Some(first) = self.entries.first_mut() {
            first.is_keyframe = true;
        }
    }
}

/// Information about an audio stream in a media source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    pub stream_index: usize,
    pub codec_name: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_layout: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub bit_rate: Option<u64>,
}

/// Information about a subtitle stream in a media source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleStreamInfo {
    pub stream_index: usize,
    pub codec_name: String,
    pub language: Option<String>,
    pub title: Option<String>,
}

/// User configuration for how an audio stream is handled on export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioExportMode {
    DirectStreamCopy,
    ReEncode { codec: String, bitrate_kbps: u32 },
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStreamConfig {
    pub info: AudioStreamInfo,
    pub mode: AudioExportMode,
    /// Audio delay / synchronization offset in milliseconds (positive delays audio, negative advances it).
    pub delay_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleStreamConfig {
    pub info: SubtitleStreamInfo,
    pub enabled: bool,
}

/// Normalized peak and RMS envelope for a single time bucket of an audio stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct WaveformPeak {
    /// Minimum normalized sample amplitude in `[-1.0, 0.0]`.
    pub min: f32,
    /// Maximum normalized sample amplitude in `[0.0, 1.0]`.
    pub max: f32,
    /// Root-mean-square energy in `[0.0, 1.0]`.
    pub rms: f32,
}

/// Extracted audio waveform envelope for a `SourceClip`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioWaveform {
    pub stream_index: usize,
    pub sample_rate: u32,
    pub peaks: Vec<WaveformPeak>,
}

impl AudioWaveform {
    /// Samples the waveform at a normalized position `t` in `[0.0, 1.0]`.
    pub fn sample_at_fraction(&self, t: f64) -> WaveformPeak {
        if self.peaks.is_empty() {
            return WaveformPeak::default();
        }
        let idx = ((t.clamp(0.0, 1.0) * (self.peaks.len() - 1) as f64).round() as usize)
            .min(self.peaks.len() - 1);
        self.peaks[idx]
    }
}

/// A loaded video file in the project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceClip {
    pub id: usize,
    pub path: PathBuf,
    pub container_format: String,
    pub video_stream_index: usize,
    pub video_codec: String,
    pub pixel_format: String,
    pub width: u32,
    pub height: u32,
    pub bit_rate: Option<u64>,
    pub file_size_bytes: u64,
    pub timing: FrameTimingMap,
    pub audio_streams: Vec<AudioStreamInfo>,
    pub subtitle_streams: Vec<SubtitleStreamInfo>,
    #[serde(default)]
    pub waveform: Option<AudioWaveform>,
}

/// A contiguous half-open range `[start_frame, end_frame)` from a `SourceClip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditSegment {
    pub source_id: usize,
    pub start_frame: u64,
    pub end_frame: u64,
}

impl EditSegment {
    pub fn frame_count(&self) -> u64 {
        self.end_frame.saturating_sub(self.start_frame)
    }
}

/// Resolved mapping of a virtual timeline frame back to its underlying source clip and frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedFrame {
    pub virtual_frame: u64,
    pub virtual_pts: TimestampUs,
    pub segment_index: usize,
    pub source_id: usize,
    pub source_frame: u64,
    pub source_pts: TimestampUs,
    pub duration_us: i64,
    pub is_keyframe: bool,
}

/// Analysis of whether the current timeline / selection can be exported via Direct Stream Copy without keyframe drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCopyAnalysis {
    pub starts_on_keyframe: bool,
    pub requested_start_frame: u64,
    pub actual_keyframe_start: u64,
    pub next_keyframe_after_start: u64,
    pub pre_roll_frames: u64,
    pub smart_cut_reencode_frames: u64,
    pub pre_roll_duration: TimestampUs,
    pub segment_count: usize,
    pub warning_message: Option<String>,
}

/// User-placed named bookmark / chapter marker on the virtual timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineMarker {
    pub frame: u64,
    pub label: String,
}

/// Snapshot of mutable timeline state for Undo/Redo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSnapshot {
    pub segments: Vec<EditSegment>,
    pub selection_start: Option<u64>,
    pub selection_end: Option<u64>,
    pub current_frame: u64,
}

#[derive(Debug, Clone)]
struct UndoEntry {
    description: String,
    snapshot: TimelineSnapshot,
}

/// Non-destructive editing project state with Undo/Redo support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub sources: Vec<SourceClip>,
    pub segments: Vec<EditSegment>,
    pub selection_start: Option<u64>,
    pub selection_end: Option<u64>,
    pub current_frame: u64,
    pub audio_configs: Vec<AudioStreamConfig>,
    pub subtitle_configs: Vec<SubtitleStreamConfig>,
    #[serde(default)]
    pub markers: Vec<TimelineMarker>,
    #[serde(default)]
    pub external_audio_path: Option<PathBuf>,
    #[serde(default = "default_true_core")]
    pub replace_source_audio_with_external: bool,
    #[serde(skip)]
    undo_stack: Vec<UndoEntry>,
    #[serde(skip)]
    redo_stack: Vec<UndoEntry>,
}

fn default_true_core() -> bool {
    true
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            segments: Vec::new(),
            selection_start: None,
            selection_end: None,
            current_frame: 0,
            audio_configs: Vec::new(),
            subtitle_configs: Vec::new(),
            markers: Vec::new(),
            external_audio_path: None,
            replace_source_audio_with_external: true,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    /// Loads a primary video clip into a fresh project.
    pub fn from_primary_source(mut source: SourceClip) -> Self {
        source.id = 0;
        let total = source.timing.total_frames().max(1);
        let audio_configs = source
            .audio_streams
            .iter()
            .cloned()
            .map(|info| AudioStreamConfig {
                info,
                mode: AudioExportMode::DirectStreamCopy,
                delay_ms: 0,
            })
            .collect();
        let subtitle_configs = source
            .subtitle_streams
            .iter()
            .cloned()
            .map(|info| SubtitleStreamConfig {
                info,
                enabled: true,
            })
            .collect();

        Self {
            sources: vec![source],
            segments: vec![EditSegment {
                source_id: 0,
                start_frame: 0,
                end_frame: total,
            }],
            selection_start: None,
            selection_end: None,
            current_frame: 0,
            audio_configs,
            subtitle_configs,
            markers: Vec::new(),
            external_audio_path: None,
            replace_source_audio_with_external: true,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn is_loaded(&self) -> bool {
        !self.sources.is_empty() && !self.segments.is_empty()
    }

    pub fn primary_source(&self) -> Option<&SourceClip> {
        self.sources.first()
    }

    pub fn source_by_id(&self, id: usize) -> Option<&SourceClip> {
        self.sources.iter().find(|s| s.id == id)
    }

    /// Updates the frame timing table of a loaded source (e.g. when background full frame index scan completes).
    pub fn update_source_timing(&mut self, source_id: usize, timing: FrameTimingMap) {
        let old_total = self
            .sources
            .iter()
            .find(|s| s.id == source_id)
            .map(|s| s.timing.total_frames())
            .unwrap_or(0);
        let new_total = timing.total_frames().max(1);

        if let Some(src) = self.sources.iter_mut().find(|s| s.id == source_id) {
            src.timing = timing;
        }
        // If a segment spanned to the end of the initial estimate, adjust it to the exact frame count
        for seg in &mut self.segments {
            if seg.source_id == source_id {
                if seg.end_frame == old_total || seg.end_frame > new_total {
                    seg.end_frame = new_total;
                }
                if seg.start_frame >= seg.end_frame {
                    seg.start_frame = seg.end_frame.saturating_sub(1);
                }
            }
        }
        let max_frame = self.total_frames().saturating_sub(1);
        if self.current_frame > max_frame {
            self.current_frame = max_frame;
        }
    }

    /// Attaches an extracted `AudioWaveform` to a loaded source clip.
    pub fn update_source_waveform(&mut self, source_id: usize, waveform: AudioWaveform) {
        if let Some(src) = self.sources.iter_mut().find(|s| s.id == source_id) {
            src.waveform = Some(waveform);
        }
    }

    /// Applies exact keyframe timestamps discovered in a local time window to a source clip.
    pub fn update_source_local_keyframes(
        &mut self,
        source_id: usize,
        window_start: TimestampUs,
        window_end: TimestampUs,
        keyframe_pts: &[TimestampUs],
    ) {
        if let Some(src) = self.sources.iter_mut().find(|s| s.id == source_id) {
            src.timing
                .apply_local_keyframes(window_start, window_end, keyframe_pts);
        }
    }

    /// Resamples the non-destructive timeline's audio waveform across `columns` horizontal display buckets.
    /// Automatically reflects deleted, trimmed, or appended segments.
    pub fn sample_timeline_waveform(&self, columns: usize) -> Vec<WaveformPeak> {
        let total = self.total_frames();
        self.sample_timeline_waveform_range(0, total, columns)
    }

    /// Resamples the audio waveform over the visible virtual frame window `[start_frame, end_frame)`.
    pub fn sample_timeline_waveform_range(
        &self,
        start_frame: u64,
        end_frame: u64,
        columns: usize,
    ) -> Vec<WaveformPeak> {
        let total_frames = self.total_frames();
        if columns == 0 || total_frames == 0 || start_frame >= end_frame {
            return Vec::new();
        }
        let has_any_waveform = self.sources.iter().any(|s| s.waveform.is_some());
        if !has_any_waveform {
            return Vec::new();
        }

        let lo = start_frame.min(total_frames - 1);
        let hi = end_frame.min(total_frames).max(lo + 1);
        let span = (hi - lo).saturating_sub(1).max(1);

        let mut out = Vec::with_capacity(columns);
        for col in 0..columns {
            let v_frame = (lo + (col as u64 * span) / (columns.saturating_sub(1).max(1) as u64))
                .min(total_frames - 1);
            let peak = if let Some(resolved) = self.resolve_frame(v_frame) {
                if let Some(src) = self.source_by_id(resolved.source_id) {
                    if let Some(wf) = &src.waveform {
                        let src_total = src.timing.total_frames().max(1);
                        let frac = resolved.source_frame as f64 / src_total as f64;
                        wf.sample_at_fraction(frac)
                    } else {
                        WaveformPeak::default()
                    }
                } else {
                    WaveformPeak::default()
                }
            } else {
                WaveformPeak::default()
            };
            out.push(peak);
        }
        out
    }

    /// Total number of frames in the active virtual timeline.
    pub fn total_frames(&self) -> u64 {
        self.segments.iter().map(|s| s.frame_count()).sum()
    }

    /// Total duration of the active virtual timeline in microseconds.
    pub fn total_duration(&self) -> TimestampUs {
        let mut sum_us = 0i64;
        for seg in &self.segments {
            if let Some(src) = self.source_by_id(seg.source_id) {
                let start_ts = src.timing.frame_to_timestamp(seg.start_frame);
                let end_ts = src.timing.frame_to_timestamp(seg.end_frame);
                sum_us += (end_ts.0 - start_ts.0).max(0);
            }
        }
        TimestampUs(sum_us)
    }

    /// Resolves a virtual frame index on the edited timeline to its underlying source clip, source frame, and PTS.
    pub fn resolve_frame(&self, virtual_frame: u64) -> Option<ResolvedFrame> {
        if self.segments.is_empty() {
            return None;
        }
        let clamped = virtual_frame.min(self.total_frames().saturating_sub(1));
        let mut accum_frames = 0u64;
        let mut accum_pts_us = 0i64;

        for (seg_idx, seg) in self.segments.iter().enumerate() {
            let count = seg.frame_count();
            let src = self.source_by_id(seg.source_id)?;
            let seg_start_pts = src.timing.frame_to_timestamp(seg.start_frame);

            if clamped < accum_frames + count {
                let offset = clamped - accum_frames;
                let source_frame = seg.start_frame + offset;
                let entry = src.timing.frame_entry(source_frame).unwrap_or(FrameEntry {
                    frame_number: source_frame,
                    pts_us: src.timing.frame_to_timestamp(source_frame),
                    duration_us: src.timing.nominal_fps.frame_duration_us(),
                    is_keyframe: source_frame == 0,
                });
                let within_seg_us = (entry.pts_us.0 - seg_start_pts.0).max(0);
                return Some(ResolvedFrame {
                    virtual_frame: clamped,
                    virtual_pts: TimestampUs(accum_pts_us + within_seg_us),
                    segment_index: seg_idx,
                    source_id: seg.source_id,
                    source_frame,
                    source_pts: entry.pts_us,
                    duration_us: entry.duration_us,
                    is_keyframe: entry.is_keyframe,
                });
            }

            accum_frames += count;
            let seg_end_pts = src.timing.frame_to_timestamp(seg.end_frame);
            accum_pts_us += (seg_end_pts.0 - seg_start_pts.0).max(0);
        }

        None
    }

    /// Checks if any virtual frame in `[start_frame, end_frame)` is a keyframe,
    /// returning the exact virtual frame index of the first keyframe in that range.
    pub fn first_keyframe_in_range(&self, start_frame: u64, end_frame: u64) -> Option<u64> {
        if start_frame >= end_frame || self.segments.is_empty() {
            return None;
        }
        let mut accum_frames = 0u64;
        for seg in &self.segments {
            let count = seg.frame_count();
            let seg_v_start = accum_frames;
            let seg_v_end = accum_frames + count;
            accum_frames = seg_v_end;

            let overlap_start = seg_v_start.max(start_frame);
            let overlap_end = seg_v_end.min(end_frame);
            if overlap_start < overlap_end {
                if let Some(src) = self.source_by_id(seg.source_id) {
                    let src_lo = seg.start_frame + (overlap_start - seg_v_start);
                    let src_hi = seg.start_frame + (overlap_end - seg_v_start);
                    if let Some(kf_src) = src.timing.first_keyframe_in_range(src_lo, src_hi) {
                        return Some(seg_v_start + (kf_src - seg.start_frame));
                    }
                }
            }
            if seg_v_start >= end_frame {
                break;
            }
        }
        None
    }

    /// Converts a virtual timeline timestamp into the corresponding virtual frame number.
    pub fn virtual_timestamp_to_frame(&self, target_ts: TimestampUs) -> u64 {
        if self.segments.is_empty() || target_ts.0 <= 0 {
            return 0;
        }
        let mut accum_frames = 0u64;
        let mut accum_pts_us = 0i64;

        for seg in &self.segments {
            let Some(src) = self.source_by_id(seg.source_id) else {
                continue;
            };
            let seg_start_pts = src.timing.frame_to_timestamp(seg.start_frame);
            let seg_end_pts = src.timing.frame_to_timestamp(seg.end_frame);
            let seg_dur = (seg_end_pts.0 - seg_start_pts.0).max(0);

            if target_ts.0 < accum_pts_us + seg_dur {
                let local_target = TimestampUs(seg_start_pts.0 + (target_ts.0 - accum_pts_us));
                let src_frame = src
                    .timing
                    .timestamp_to_frame(local_target)
                    .clamp(seg.start_frame, seg.end_frame.saturating_sub(1));
                return accum_frames + (src_frame - seg.start_frame);
            }

            accum_frames += seg.frame_count();
            accum_pts_us += seg_dur;
        }

        self.total_frames().saturating_sub(1)
    }

    pub fn step_forward(&mut self, frames: u64) {
        let max_f = self.total_frames().saturating_sub(1);
        self.current_frame = (self.current_frame + frames).min(max_f);
    }

    pub fn step_backward(&mut self, frames: u64) {
        self.current_frame = self.current_frame.saturating_sub(frames);
    }

    pub fn seek_start(&mut self) {
        self.current_frame = 0;
    }

    pub fn seek_end(&mut self) {
        self.current_frame = self.total_frames().saturating_sub(1);
    }

    pub fn seek_prev_keyframe(&mut self) {
        if self.current_frame == 0 {
            return;
        }
        for candidate in (0..self.current_frame).rev() {
            if let Some(resolved) = self.resolve_frame(candidate) {
                if resolved.is_keyframe {
                    self.current_frame = candidate;
                    return;
                }
            }
        }
        self.current_frame = 0;
    }

    pub fn seek_next_keyframe(&mut self) {
        let total = self.total_frames();
        if total == 0 {
            return;
        }
        for candidate in (self.current_frame + 1)..total {
            if let Some(resolved) = self.resolve_frame(candidate) {
                if resolved.is_keyframe {
                    self.current_frame = candidate;
                    return;
                }
            }
        }
        self.current_frame = total.saturating_sub(1);
    }

    /// Returns the normalized active selection `[start_frame, end_frame)` (end is exclusive, or inclusive+1).
    /// Note: in VirtualDub2 style, setting Mark In (`[`) at frame A and Mark Out (`]`) at frame B
    /// selects frames `A..=B`, which as a half-open range is `[A, B + 1)`.
    pub fn selection_range(&self) -> Option<(u64, u64)> {
        match (self.selection_start, self.selection_end) {
            (Some(a), Some(b)) => {
                let lo = a.min(b);
                let hi = (a.max(b) + 1).min(self.total_frames());
                if lo < hi {
                    Some((lo, hi))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn set_mark_in(&mut self) {
        self.selection_start = Some(self.current_frame);
        if let Some(end) = self.selection_end {
            if end < self.current_frame {
                self.selection_end = Some(self.current_frame);
            }
        }
    }

    pub fn set_mark_out(&mut self) {
        self.selection_end = Some(self.current_frame);
        if let Some(start) = self.selection_start {
            if start > self.current_frame {
                self.selection_start = Some(self.current_frame);
            }
        } else {
            self.selection_start = Some(0);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
    }

    fn snapshot(&self) -> TimelineSnapshot {
        TimelineSnapshot {
            segments: self.segments.clone(),
            selection_start: self.selection_start,
            selection_end: self.selection_end,
            current_frame: self.current_frame,
        }
    }

    fn push_undo(&mut self, description: impl Into<String>) {
        self.undo_stack.push(UndoEntry {
            description: description.into(),
            snapshot: self.snapshot(),
        });
        self.redo_stack.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo_description(&self) -> Option<&str> {
        self.undo_stack.last().map(|e| e.description.as_str())
    }

    pub fn redo_description(&self) -> Option<&str> {
        self.redo_stack.last().map(|e| e.description.as_str())
    }

    pub fn undo(&mut self) -> bool {
        if let Some(entry) = self.undo_stack.pop() {
            let current = self.snapshot();
            self.redo_stack.push(UndoEntry {
                description: entry.description,
                snapshot: current,
            });
            self.apply_snapshot(entry.snapshot);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(entry) = self.redo_stack.pop() {
            let current = self.snapshot();
            self.undo_stack.push(UndoEntry {
                description: entry.description,
                snapshot: current,
            });
            self.apply_snapshot(entry.snapshot);
            true
        } else {
            false
        }
    }

    fn apply_snapshot(&mut self, snap: TimelineSnapshot) {
        self.segments = snap.segments;
        self.selection_start = snap.selection_start;
        self.selection_end = snap.selection_end;
        self.current_frame = snap
            .current_frame
            .min(self.total_frames().saturating_sub(1));
    }

    /// Deletes the currently selected virtual frame range `[sel_start, sel_end)` from the timeline.
    pub fn delete_selection(&mut self) -> Result<(), CoreError> {
        if !self.is_loaded() {
            return Err(CoreError::NoMediaLoaded);
        }
        let (sel_start, sel_end) = self.selection_range().ok_or(CoreError::InvalidSelection)?;
        let total = self.total_frames();
        if sel_start == 0 && sel_end >= total {
            return Err(CoreError::CannotDeleteEntireTimeline);
        }

        self.push_undo(format!("Delete frames {}..{}", sel_start, sel_end - 1));

        let mut new_segments = Vec::new();
        let mut cursor = 0u64;

        for seg in &self.segments {
            let seg_len = seg.frame_count();
            let seg_v_start = cursor;
            let seg_v_end = cursor + seg_len;
            cursor = seg_v_end;

            // Entirely outside selection
            if seg_v_end <= sel_start || seg_v_start >= sel_end {
                new_segments.push(*seg);
                continue;
            }

            // Left portion before sel_start
            if seg_v_start < sel_start {
                let keep_left = sel_start - seg_v_start;
                new_segments.push(EditSegment {
                    source_id: seg.source_id,
                    start_frame: seg.start_frame,
                    end_frame: seg.start_frame + keep_left,
                });
            }

            // Right portion after sel_end
            if seg_v_end > sel_end {
                let skip_from_seg_start = sel_end - seg_v_start;
                new_segments.push(EditSegment {
                    source_id: seg.source_id,
                    start_frame: seg.start_frame + skip_from_seg_start,
                    end_frame: seg.end_frame,
                });
            }
        }

        self.segments = merge_adjacent_segments(new_segments);
        self.selection_start = None;
        self.selection_end = None;
        self.current_frame = sel_start.min(self.total_frames().saturating_sub(1));
        Ok(())
    }

    /// Trims the timeline so only the selected virtual frame range `[sel_start, sel_end)` remains.
    pub fn trim_to_selection(&mut self) -> Result<(), CoreError> {
        if !self.is_loaded() {
            return Err(CoreError::NoMediaLoaded);
        }
        let (sel_start, sel_end) = self.selection_range().ok_or(CoreError::InvalidSelection)?;

        self.push_undo(format!("Trim to frames {}..{}", sel_start, sel_end - 1));

        let mut new_segments = Vec::new();
        let mut cursor = 0u64;

        for seg in &self.segments {
            let seg_len = seg.frame_count();
            let seg_v_start = cursor;
            let seg_v_end = cursor + seg_len;
            cursor = seg_v_end;

            let overlap_start = seg_v_start.max(sel_start);
            let overlap_end = seg_v_end.min(sel_end);

            if overlap_start < overlap_end {
                let local_start = overlap_start - seg_v_start;
                let local_end = overlap_end - seg_v_start;
                new_segments.push(EditSegment {
                    source_id: seg.source_id,
                    start_frame: seg.start_frame + local_start,
                    end_frame: seg.start_frame + local_end,
                });
            }
        }

        self.segments = merge_adjacent_segments(new_segments);
        self.selection_start = None;
        self.selection_end = None;
        self.current_frame = 0;
        Ok(())
    }

    /// Appends another video source to the end of the timeline.
    pub fn append_source(&mut self, mut clip: SourceClip) -> Result<(), CoreError> {
        let Some(primary) = self.primary_source() else {
            return Err(CoreError::NoMediaLoaded);
        };
        if clip.width != primary.width || clip.height != primary.height {
            return Err(CoreError::IncompatibleAppend(format!(
                "Resolution mismatch: primary is {}x{}, appended clip is {}x{}",
                primary.width, primary.height, clip.width, clip.height
            )));
        }

        let next_id = self.sources.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        clip.id = next_id;
        let frames = clip.timing.total_frames().max(1);
        let name = clip
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("clip")
            .to_string();

        self.push_undo(format!("Append {name}"));
        self.sources.push(clip);
        self.segments.push(EditSegment {
            source_id: next_id,
            start_frame: 0,
            end_frame: frames,
        });
        Ok(())
    }

    /// Resets the timeline back to the full uncut primary source.
    pub fn reset_timeline(&mut self) {
        if !self.is_loaded() {
            return;
        }
        self.push_undo("Reset timeline to full source");
        self.segments = self
            .sources
            .iter()
            .map(|src| EditSegment {
                source_id: src.id,
                start_frame: 0,
                end_frame: src.timing.total_frames().max(1),
            })
            .collect();
        self.selection_start = None;
        self.selection_end = None;
        self.current_frame = 0;
    }

    /// Returns the effective segments to export: if a selection is active and `export_selection_only` is true,
    /// slices the current segments to the selection; otherwise returns the full edited timeline segments.
    pub fn effective_export_segments(&self, export_selection_only: bool) -> Vec<EditSegment> {
        if export_selection_only {
            if let Some((sel_start, sel_end)) = self.selection_range() {
                return self.segments_for_virtual_range(sel_start, sel_end);
            }
        }
        self.segments.clone()
    }

    /// Slices the virtual timeline into `EditSegment`s covering `[v_start, v_end)`.
    pub fn segments_for_virtual_range(&self, v_start: u64, v_end: u64) -> Vec<EditSegment> {
        if v_start >= v_end {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut cursor = 0u64;
        for seg in &self.segments {
            let seg_v_start = cursor;
            let seg_v_end = cursor + seg.frame_count();
            cursor = seg_v_end;

            let overlap_start = seg_v_start.max(v_start);
            let overlap_end = seg_v_end.min(v_end);
            if overlap_start < overlap_end {
                out.push(EditSegment {
                    source_id: seg.source_id,
                    start_frame: seg.start_frame + (overlap_start - seg_v_start),
                    end_frame: seg.start_frame + (overlap_end - seg_v_start),
                });
            }
        }
        merge_adjacent_segments(out)
    }

    /// Toggles a timeline marker at `self.current_frame`. Returns `true` if added, `false` if removed.
    pub fn toggle_marker_at_current(&mut self) -> bool {
        let frame = self.current_frame;
        if let Some(pos) = self.markers.iter().position(|m| m.frame == frame) {
            self.markers.remove(pos);
            false
        } else {
            let idx = self.markers.len() + 1;
            self.add_or_update_marker(frame, format!("Marker {idx}"));
            true
        }
    }

    /// Adds or updates a marker at `frame` and keeps `self.markers` sorted by frame index.
    pub fn add_or_update_marker(&mut self, frame: u64, label: impl Into<String>) {
        let clamped = frame.min(self.total_frames().saturating_sub(1));
        let label = label.into();
        if let Some(existing) = self.markers.iter_mut().find(|m| m.frame == clamped) {
            existing.label = label;
        } else {
            self.markers.push(TimelineMarker {
                frame: clamped,
                label,
            });
            self.markers.sort_by_key(|m| m.frame);
        }
    }

    pub fn seek_prev_marker(&mut self) -> bool {
        if let Some(m) = self
            .markers
            .iter()
            .rev()
            .find(|m| m.frame < self.current_frame)
        {
            self.current_frame = m.frame;
            true
        } else {
            false
        }
    }

    pub fn seek_next_marker(&mut self) -> bool {
        if let Some(m) = self.markers.iter().find(|m| m.frame > self.current_frame) {
            self.current_frame = m.frame;
            true
        } else {
            false
        }
    }

    /// Computes non-empty `(start_frame, end_frame, label)` sub-clip ranges partitioned by `self.markers`
    /// (or by EDL segments if no markers exist).
    pub fn split_clip_ranges(&self) -> Vec<(u64, u64, String)> {
        let total = self.total_frames();
        if total == 0 {
            return Vec::new();
        }
        if !self.markers.is_empty() {
            let mut cuts: Vec<(u64, String)> = Vec::new();
            if self.markers[0].frame > 0 {
                cuts.push((0, "Part_01".to_string()));
            }
            for (i, m) in self.markers.iter().enumerate() {
                if m.frame < total {
                    let safe_label: String = m
                        .label
                        .chars()
                        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
                        .collect();
                    let lbl = if safe_label.is_empty() {
                        format!("Part_{:02}", i + 1)
                    } else {
                        safe_label
                    };
                    if cuts.last().map(|(f, _)| *f) != Some(m.frame) {
                        cuts.push((m.frame, lbl));
                    }
                }
            }
            let mut ranges = Vec::new();
            for i in 0..cuts.len() {
                let start = cuts[i].0;
                let end = if i + 1 < cuts.len() {
                    cuts[i + 1].0
                } else {
                    total
                };
                if start < end {
                    ranges.push((start, end, cuts[i].1.clone()));
                }
            }
            return ranges;
        }

        // Fallback: split by EDL segments
        let mut ranges = Vec::new();
        let mut cursor = 0u64;
        for (i, seg) in self.segments.iter().enumerate() {
            let len = seg.frame_count();
            if len > 0 {
                ranges.push((cursor, cursor + len, format!("Segment_{:02}", i + 1)));
                cursor += len;
            }
        }
        ranges
    }

    /// Analyzes keyframe constraints for Direct Stream Copy of the effective export segments.
    pub fn analyze_stream_copy(&self, export_selection_only: bool) -> Option<StreamCopyAnalysis> {
        let segs = self.effective_export_segments(export_selection_only);
        let first = segs.first()?;
        let src = self.source_by_id(first.source_id)?;
        let kf_before = src.timing.keyframe_at_or_before(first.start_frame);
        let starts_on_keyframe = kf_before == first.start_frame;
        let next_kf = if starts_on_keyframe {
            first.start_frame
        } else {
            src.timing.next_keyframe(first.start_frame).min(first.end_frame)
        };
        let smart_cut_reencode_frames = next_kf.saturating_sub(first.start_frame);
        let pre_roll_frames = first.start_frame.saturating_sub(kf_before);
        let req_pts = src.timing.frame_to_timestamp(first.start_frame);
        let kf_pts = src.timing.frame_to_timestamp(kf_before);
        let pre_roll_duration = TimestampUs((req_pts.0 - kf_pts.0).max(0));

        let warning_message = if !starts_on_keyframe {
            Some(format!(
                "Cut starts at non-keyframe #{}. Direct Stream Copy cannot decode inter-frames without their preceding keyframe (#{}, {} earlier). Use Full Processing for frame-exact cuts, or snap to keyframe.",
                first.start_frame, kf_before, pre_roll_duration
            ))
        } else if segs.len() > 1 {
            let non_kf_joins = segs
                .iter()
                .skip(1)
                .filter(|s| {
                    self.source_by_id(s.source_id)
                        .map(|c| c.timing.keyframe_at_or_before(s.start_frame) != s.start_frame)
                        .unwrap_or(false)
                })
                .count();
            if non_kf_joins > 0 {
                Some(format!(
                    "{non_kf_joins} internal segment splice(s) start on non-keyframes. Direct Stream Copy may show brief artifacts or shift splice points to keyframes; Full Processing guarantees frame-exact splices."
                ))
            } else {
                None
            }
        } else {
            None
        };

        Some(StreamCopyAnalysis {
            starts_on_keyframe,
            requested_start_frame: first.start_frame,
            actual_keyframe_start: kf_before,
            next_keyframe_after_start: next_kf,
            pre_roll_frames,
            smart_cut_reencode_frames,
            pre_roll_duration,
            segment_count: segs.len(),
            warning_message,
        })
    }

    pub fn save_to_file(&self, path: &Path) -> Result<(), CoreError> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn load_from_file(path: &Path) -> Result<Self, CoreError> {
        let data = fs::read(path)?;
        let project: Self = serde_json::from_slice(&data)?;
        Ok(project)
    }
}

fn merge_adjacent_segments(segments: Vec<EditSegment>) -> Vec<EditSegment> {
    let mut merged: Vec<EditSegment> = Vec::with_capacity(segments.len());
    for seg in segments {
        if seg.frame_count() == 0 {
            continue;
        }
        if let Some(last) = merged.last_mut() {
            if last.source_id == seg.source_id && last.end_frame == seg.start_frame {
                last.end_frame = seg.end_frame;
                continue;
            }
        }
        merged.push(seg);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_source(frames: u64, kf_step: u64) -> SourceClip {
        SourceClip {
            id: 0,
            path: PathBuf::from("/tmp/sample.mp4"),
            container_format: "mov,mp4,m4a,3gp,3g2,mj2".into(),
            video_stream_index: 0,
            video_codec: "h264".into(),
            pixel_format: "yuv420p".into(),
            width: 1920,
            height: 1080,
            bit_rate: Some(5_000_000),
            file_size_bytes: 10_000_000,
            timing: FrameTimingMap::from_cfr(
                frames,
                Rational::new(30, 1),
                Rational::new(1, 90000),
                kf_step,
            ),
            audio_streams: vec![],
            subtitle_streams: vec![],
            waveform: None,
        }
    }

    #[test]
    fn test_timestamp_formatting_and_parsing() {
        let ts = TimestampUs::from_seconds_f64(3661.250);
        assert_eq!(ts.format_hms_millis(), "01:01:01.250");
        let parsed = TimestampUs::parse_str("01:01:01.250").unwrap();
        assert_eq!(parsed, ts);

        let short = TimestampUs::parse_str("12.5").unwrap();
        assert_eq!(short.format_hms_millis(), "00:00:12.500");
    }

    #[test]
    fn test_vfr_timing_map_lookup() {
        // Variable frame rate: frame 0 at 0ms, frame 1 at 33ms, frame 2 at 100ms, frame 3 at 116ms
        let entries = vec![
            FrameEntry {
                frame_number: 0,
                pts_us: TimestampUs(0),
                duration_us: 33_000,
                is_keyframe: true,
            },
            FrameEntry {
                frame_number: 1,
                pts_us: TimestampUs(33_000),
                duration_us: 67_000,
                is_keyframe: false,
            },
            FrameEntry {
                frame_number: 2,
                pts_us: TimestampUs(100_000),
                duration_us: 16_000,
                is_keyframe: true,
            },
            FrameEntry {
                frame_number: 3,
                pts_us: TimestampUs(116_000),
                duration_us: 33_000,
                is_keyframe: false,
            },
        ];
        let map = FrameTimingMap::from_entries(
            entries,
            Rational::new(30, 1),
            Rational::new(1, 1000),
        );
        assert!(map.is_vfr);
        assert_eq!(map.timestamp_to_frame(TimestampUs(0)), 0);
        assert_eq!(map.timestamp_to_frame(TimestampUs(50_000)), 1);
        assert_eq!(map.timestamp_to_frame(TimestampUs(100_000)), 2);
        assert_eq!(map.timestamp_to_frame(TimestampUs(115_000)), 2);
        assert_eq!(map.timestamp_to_frame(TimestampUs(116_000)), 3);
        assert_eq!(map.prev_keyframe(3), 2);
        assert_eq!(map.prev_keyframe(2), 0);
        assert_eq!(map.next_keyframe(0), 2);
    }

    #[test]
    fn test_delete_trim_and_undo_redo() {
        let mut proj = Project::from_primary_source(sample_source(100, 10));
        assert_eq!(proj.total_frames(), 100);

        // Select frames 20..=29 (10 frames) and delete
        proj.current_frame = 20;
        proj.set_mark_in();
        proj.current_frame = 29;
        proj.set_mark_out();
        assert_eq!(proj.selection_range(), Some((20, 30)));

        proj.delete_selection().unwrap();
        assert_eq!(proj.total_frames(), 90);
        assert_eq!(proj.segments.len(), 2);
        // Virtual frame 20 should now map to source frame 30
        let resolved = proj.resolve_frame(20).unwrap();
        assert_eq!(resolved.source_frame, 30);

        // Undo deletion
        assert!(proj.undo());
        assert_eq!(proj.total_frames(), 100);
        assert_eq!(proj.resolve_frame(20).unwrap().source_frame, 20);

        // Redo deletion
        assert!(proj.redo());
        assert_eq!(proj.total_frames(), 90);

        // Trim to virtual frames 10..=29 (20 frames spanning the splice)
        proj.current_frame = 10;
        proj.set_mark_in();
        proj.current_frame = 29;
        proj.set_mark_out();
        proj.trim_to_selection().unwrap();
        assert_eq!(proj.total_frames(), 20);
        assert_eq!(proj.resolve_frame(0).unwrap().source_frame, 10);
        assert_eq!(proj.resolve_frame(10).unwrap().source_frame, 30);
    }

    #[test]
    fn test_stream_copy_keyframe_analysis() {
        let mut proj = Project::from_primary_source(sample_source(100, 10));
        // Select starting at frame 15 (not a keyframe; keyframe is at 10)
        proj.current_frame = 15;
        proj.set_mark_in();
        proj.current_frame = 50;
        proj.set_mark_out();

        let analysis = proj.analyze_stream_copy(true).unwrap();
        assert!(!analysis.starts_on_keyframe);
        assert_eq!(analysis.requested_start_frame, 15);
        assert_eq!(analysis.actual_keyframe_start, 10);
        assert_eq!(analysis.pre_roll_frames, 5);
        assert!(analysis.warning_message.is_some());
    }
}
