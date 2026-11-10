//! Bounded LRU frame cache, wall-clock VFR/CFR playback clock, and continuous
//! flow-controlled background FFmpeg streaming engine for smooth playback.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use redub_core::{Project, SourceClip, TimestampUs};
use redub_media::{ContinuousFrameStream, DecodedFrame, FfmpegProcessDecoder};
use redub_platform::{configure_background_command, FfmpegBinaries};
use rodio::buffer::SamplesBuffer;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use tracing::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameKey {
    pub source_id: usize,
    pub source_frame: u64,
}

/// Bounded Least-Recently-Used (LRU) cache for decoded RGBA frames.
pub struct FrameCache {
    capacity: usize,
    order: VecDeque<FrameKey>,
    map: HashMap<FrameKey, DecodedFrame>,
    pub hits: u64,
    pub misses: u64,
}

impl FrameCache {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(16);
        Self {
            capacity: cap,
            order: VecDeque::with_capacity(cap),
            map: HashMap::with_capacity(cap),
            hits: 0,
            misses: 0,
        }
    }

    pub fn get(&mut self, key: FrameKey) -> Option<DecodedFrame> {
        if let Some(frame) = self.map.get(&key).cloned() {
            self.hits += 1;
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(key);
            Some(frame)
        } else {
            self.misses += 1;
            None
        }
    }

    pub fn contains(&self, key: FrameKey) -> bool {
        self.map.contains_key(&key)
    }

    pub fn hit_rate_percent(&self) -> f32 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            (self.hits as f32 / total as f32) * 100.0
        }
    }

    pub fn memory_bytes(&self) -> usize {
        self.map.values().map(|f| f.rgba.len()).sum()
    }

    pub fn insert(&mut self, frame: DecodedFrame) {
        let key = FrameKey {
            source_id: frame.source_id,
            source_frame: frame.source_frame,
        };
        if self.map.contains_key(&key) {
            if let Some(pos) = self.order.iter().position(|k| *k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(key);
            self.map.insert(key, frame);
            return;
        }
        while self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.map.remove(&evicted);
            }
        }
        self.order.push_back(key);
        self.map.insert(key, frame);
    }

    pub fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Wall-clock synchronized playback controller supporting variable and constant frame rates.
#[derive(Debug, Clone)]
pub struct PlaybackClock {
    pub is_playing: bool,
    pub loop_selection: bool,
    pub speed: f64,
    anchor_wall: Option<Instant>,
    anchor_pts: TimestampUs,
}

impl Default for PlaybackClock {
    fn default() -> Self {
        Self {
            is_playing: false,
            loop_selection: false,
            speed: 1.0,
            anchor_wall: None,
            anchor_pts: TimestampUs::ZERO,
        }
    }
}

impl PlaybackClock {
    pub const SPEED_PRESETS: [f64; 6] = [0.25, 0.5, 1.0, 1.5, 2.0, 4.0];

    pub fn start(&mut self, current_pts: TimestampUs) {
        self.is_playing = true;
        self.anchor_wall = Some(Instant::now());
        self.anchor_pts = current_pts;
    }

    pub fn pause(&mut self) {
        self.is_playing = false;
        self.anchor_wall = None;
    }

    pub fn toggle(&mut self, current_pts: TimestampUs) {
        if self.is_playing {
            self.pause();
        } else {
            self.start(current_pts);
        }
    }

    /// Sets playback speed multiplier (`0.25x` .. `4.0x`) and re-anchors the clock at `current_pts`.
    pub fn set_speed(&mut self, new_speed: f64, current_pts: TimestampUs) {
        self.speed = new_speed.clamp(0.1, 8.0);
        if self.is_playing {
            self.anchor_wall = Some(Instant::now());
            self.anchor_pts = current_pts;
        }
    }

    pub fn step_speed_up(&mut self, current_pts: TimestampUs) {
        let next = Self::SPEED_PRESETS
            .iter()
            .copied()
            .find(|&s| s > self.speed + 0.05)
            .unwrap_or(4.0);
        self.set_speed(next, current_pts);
    }

    pub fn step_speed_down(&mut self, current_pts: TimestampUs) {
        let prev = Self::SPEED_PRESETS
            .iter()
            .rev()
            .copied()
            .find(|&s| s < self.speed - 0.05)
            .unwrap_or(0.25);
        self.set_speed(prev, current_pts);
    }

    /// Re-anchors the playback clock after a manual seek or initial stream buffer warmup.
    pub fn reanchor(&mut self, new_pts: TimestampUs) {
        if self.is_playing {
            self.anchor_wall = Some(Instant::now());
            self.anchor_pts = new_pts;
        }
    }

    /// Advances `project.current_frame` according to elapsed wall-clock time and exact frame PTS.
    /// Returns `true` if playback is active.
    pub fn tick(&mut self, project: &mut Project) -> bool {
        if !self.is_playing || !project.is_loaded() {
            return false;
        }
        let Some(anchor) = self.anchor_wall else {
            return false;
        };
        let elapsed_us = (anchor.elapsed().as_secs_f64() * self.speed * 1_000_000.0) as i64;
        let target_pts = TimestampUs(self.anchor_pts.0 + elapsed_us);

        let total_frames = project.total_frames();
        let next_frame = project.virtual_timestamp_to_frame(target_pts);

        if let Some((_sel_start, sel_end)) = project.selection_range() {
            if self.loop_selection && next_frame >= sel_end {
                let loop_start = project.selection_range().map(|(s, _)| s).unwrap_or(0);
                project.current_frame = loop_start;
                let loop_pts = project
                    .resolve_frame(loop_start)
                    .map(|r| r.virtual_pts)
                    .unwrap_or(TimestampUs::ZERO);
                self.reanchor(loop_pts);
                return true;
            }
        }

        if next_frame >= total_frames.saturating_sub(1) && target_pts >= project.total_duration() {
            project.current_frame = total_frames.saturating_sub(1);
            self.pause();
            return true;
        }

        project.current_frame = next_frame;
        true
    }
}

#[derive(Debug, Clone)]
struct DecodeJob {
    clip: Arc<SourceClip>,
    start_frame: u64,
    is_playing: bool,
    burst_count: Option<usize>,
    max_dimension: Option<u32>,
}

/// Number of frames to keep decoded ahead of the playhead during continuous playback.
const STREAM_LOOKAHEAD_FRAMES: u64 = 45;

/// Non-blocking preview engine that manages the `FrameCache` and a persistent background
/// streaming FFmpeg decode worker.
pub struct PreviewEngine {
    pub cache: FrameCache,
    latest_job: Arc<Mutex<Option<DecodeJob>>>,
    playhead_frame: Arc<AtomicU64>,
    is_playing_flag: Arc<AtomicBool>,
    stream_active: Arc<AtomicBool>,
    stream_head_frame: Arc<AtomicU64>,
    stream_start_frame: Arc<AtomicU64>,
    wakeup_tx: Sender<()>,
    result_rx: Receiver<DecodedFrame>,
    last_scrub_request: Option<(usize, u64)>,
    pub max_preview_dimension: Option<u32>,
}

impl PreviewEngine {
    pub fn new(decoder: FfmpegProcessDecoder, cache_capacity: usize) -> Self {
        let cap = cache_capacity.max(96);
        let latest_job: Arc<Mutex<Option<DecodeJob>>> = Arc::new(Mutex::new(None));
        let playhead_frame = Arc::new(AtomicU64::new(0));
        let is_playing_flag = Arc::new(AtomicBool::new(false));
        let stream_active = Arc::new(AtomicBool::new(false));
        let stream_head_frame = Arc::new(AtomicU64::new(0));
        let stream_start_frame = Arc::new(AtomicU64::new(0));
        let (wakeup_tx, wakeup_rx) = mpsc::channel::<()>();
        let (result_tx, result_rx) = mpsc::channel::<DecodedFrame>();

        let job_slot = Arc::clone(&latest_job);
        let playhead_shared = Arc::clone(&playhead_frame);
        let playing_shared = Arc::clone(&is_playing_flag);
        let active_shared = Arc::clone(&stream_active);
        let head_shared = Arc::clone(&stream_head_frame);
        let start_shared = Arc::clone(&stream_start_frame);

        thread::Builder::new()
            .name("cutdub-preview-decoder".into())
            .spawn(move || {
                let mut active_stream: Option<ContinuousFrameStream> = None;
                let mut stream_is_continuous = false;

                loop {
                    let maybe_job = {
                        let mut guard = job_slot.lock().unwrap();
                        guard.take()
                    };

                    if let Some(job) = maybe_job {
                        let can_reuse = if let Some(ref stream) = active_stream {
                            stream.clip.id == job.clip.id
                                && job.start_frame >= stream.next_frame
                                && job.start_frame <= stream.next_frame + 12
                                && (stream_is_continuous == job.is_playing)
                        } else {
                            false
                        };

                        if !can_reuse {
                            active_stream = None;
                            stream_is_continuous = job.is_playing;
                            let max_frames = if job.is_playing {
                                None
                            } else {
                                job.burst_count
                            };

                            start_shared.store(job.start_frame, Ordering::SeqCst);
                            head_shared.store(job.start_frame, Ordering::SeqCst);
                            active_shared.store(job.is_playing, Ordering::SeqCst);

                            let spawned = if decoder.prefer_hwaccel {
                                decoder
                                    .spawn_stream(
                                        Arc::clone(&job.clip),
                                        job.start_frame,
                                        max_frames,
                                        job.max_dimension,
                                        true,
                                    )
                                    .or_else(|_| {
                                        decoder.spawn_stream(
                                            Arc::clone(&job.clip),
                                            job.start_frame,
                                            max_frames,
                                            job.max_dimension,
                                            false,
                                        )
                                    })
                            } else {
                                decoder.spawn_stream(
                                    Arc::clone(&job.clip),
                                    job.start_frame,
                                    max_frames,
                                    job.max_dimension,
                                    false,
                                )
                            };

                            match spawned {
                                Ok(s) => {
                                    active_stream = Some(s);
                                }
                                Err(e) => {
                                    active_shared.store(false, Ordering::SeqCst);
                                    debug!(error = %e, "Failed to spawn preview stream");
                                }
                            }
                        }
                    }

                    if let Some(ref mut stream) = active_stream {
                        let cur_playhead = playhead_shared.load(Ordering::Relaxed);
                        let is_playing = playing_shared.load(Ordering::Relaxed);

                        // If user paused a continuous stream and we have buffered 6 frames ahead, close it
                        if stream_is_continuous
                            && !is_playing
                            && stream.next_frame > cur_playhead + 6
                        {
                            active_stream = None;
                            active_shared.store(false, Ordering::SeqCst);
                            continue;
                        }

                        // Flow control: keep up to STREAM_LOOKAHEAD_FRAMES buffered ahead of playhead
                        if stream_is_continuous
                            && is_playing
                            && stream.next_frame > cur_playhead + STREAM_LOOKAHEAD_FRAMES
                        {
                            let _ = wakeup_rx.recv_timeout(Duration::from_millis(5));
                            while wakeup_rx.try_recv().is_ok() {}
                            continue;
                        }

                        match stream.read_next_frame() {
                            Ok(Some(frame)) => {
                                head_shared.store(stream.next_frame, Ordering::Relaxed);
                                if result_tx.send(frame).is_err() {
                                    break;
                                }
                            }
                            Ok(None) | Err(_) => {
                                active_stream = None;
                                active_shared.store(false, Ordering::SeqCst);
                            }
                        }
                    } else {
                        active_shared.store(false, Ordering::SeqCst);
                        if wakeup_rx.recv().is_err() {
                            break;
                        }
                        while wakeup_rx.try_recv().is_ok() {}
                    }
                }
            })
            .expect("Failed to spawn preview decoder thread");

        Self {
            cache: FrameCache::new(cap),
            latest_job,
            playhead_frame,
            is_playing_flag,
            stream_active,
            stream_head_frame,
            stream_start_frame,
            wakeup_tx,
            result_rx,
            last_scrub_request: None,
            max_preview_dimension: Some(960),
        }
    }

    /// Polls any newly decoded frames from the background worker into the LRU cache.
    /// Returns `true` if new frames arrived.
    pub fn poll_decoded_frames(&mut self) -> bool {
        let mut received = false;
        while let Ok(frame) = self.result_rx.try_recv() {
            self.cache.insert(frame);
            received = true;
        }
        received
    }

    pub fn sync_playhead(&self, source_frame: u64, is_playing: bool) {
        self.playhead_frame.store(source_frame, Ordering::Relaxed);
        self.is_playing_flag.store(is_playing, Ordering::Relaxed);
    }

    /// Requests a frame from the cache; if missing or if continuous playback needs to start,
    /// coordinates with the background streaming worker without blocking the UI thread.
    pub fn request_frame(
        &mut self,
        clip: &SourceClip,
        source_frame: u64,
        is_playing: bool,
    ) -> Option<DecodedFrame> {
        self.poll_decoded_frames();

        self.playhead_frame.store(source_frame, Ordering::Relaxed);
        self.is_playing_flag.store(is_playing, Ordering::Relaxed);

        let key = FrameKey {
            source_id: clip.id,
            source_frame,
        };

        let cached = self.cache.get(key);

        if is_playing {
            self.last_scrub_request = None;
            let _ = self.wakeup_tx.send(());

            let active = self.stream_active.load(Ordering::SeqCst);
            let head = self.stream_head_frame.load(Ordering::SeqCst);
            let start = self.stream_start_frame.load(Ordering::SeqCst);

            // Check if the continuous stream is already covering our current playhead trajectory
            let covered_by_stream = active
                && source_frame >= start
                && source_frame <= head + 20;

            if !covered_by_stream {
                let start_at = if cached.is_none() {
                    source_frame
                } else {
                    source_frame + 1
                };
                self.stream_start_frame.store(start_at, Ordering::SeqCst);
                self.stream_head_frame.store(start_at, Ordering::SeqCst);
                self.stream_active.store(true, Ordering::SeqCst);

                if let Ok(mut guard) = self.latest_job.lock() {
                    *guard = Some(DecodeJob {
                        clip: Arc::new(clip.clone()),
                        start_frame: start_at,
                        is_playing: true,
                        burst_count: None,
                        max_dimension: self.max_preview_dimension,
                    });
                }
                let _ = self.wakeup_tx.send(());
            }
        } else if cached.is_none() {
            // Paused / interactive scrubbing: decode a small 3-frame burst and deliver Frame #0 immediately
            let req = (clip.id, source_frame);
            if self.last_scrub_request != Some(req) {
                self.last_scrub_request = Some(req);
                if let Ok(mut guard) = self.latest_job.lock() {
                    *guard = Some(DecodeJob {
                        clip: Arc::new(clip.clone()),
                        start_frame: source_frame,
                        is_playing: false,
                        burst_count: Some(3),
                        max_dimension: self.max_preview_dimension,
                    });
                }
                let _ = self.wakeup_tx.send(());
            }
        }

        cached
    }
}

/// Real-time synchronized audio output engine for video preview playback.
/// Decodes the active audio stream via an FFmpeg `f32le` pipe into a cross-platform `rodio` sink,
/// automatically re-synchronizing on timeline seeks, EDL splices, and live Audio Sync Delay changes.
pub struct AudioPreviewPlayer {
    _stream: Option<OutputStream>,
    stream_handle: Option<OutputStreamHandle>,
    active_sink: Option<Arc<Sink>>,
    stop_flag: Option<Arc<AtomicBool>>,
    active_path: Option<PathBuf>,
    active_stream_index: Option<usize>,
    active_delay_ms: i64,
    active_speed: f64,
    session_wall_start: Option<Instant>,
    session_source_start_us: i64,
    pub muted: bool,
    pub volume: f32,
}

impl Default for AudioPreviewPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPreviewPlayer {
    pub fn new() -> Self {
        let (stream, handle) = match OutputStream::try_default() {
            Ok((s, h)) => (Some(s), Some(h)),
            Err(e) => {
                debug!(error = %e, "No default audio output device available");
                (None, None)
            }
        };
        Self {
            _stream: stream,
            stream_handle: handle,
            active_sink: None,
            stop_flag: None,
            active_path: None,
            active_stream_index: None,
            active_delay_ms: 0,
            active_speed: 1.0,
            session_wall_start: None,
            session_source_start_us: 0,
            muted: false,
            volume: 1.0,
        }
    }

    pub fn stop(&mut self) {
        if let Some(flag) = self.stop_flag.take() {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(sink) = self.active_sink.take() {
            sink.stop();
        }
        self.active_path = None;
        self.active_stream_index = None;
        self.session_wall_start = None;
    }

    /// Synchronizes live speaker audio output with the current video playback state.
    pub fn sync_playback(
        &mut self,
        binaries: &FfmpegBinaries,
        clip_path: &Path,
        stream_index: Option<usize>,
        source_pts: TimestampUs,
        delay_ms: i64,
        speed: f64,
        is_playing: bool,
    ) {
        let clamped_speed = speed.clamp(0.25, 4.0);
        if !is_playing || self.muted || stream_index.is_none() || self.stream_handle.is_none() {
            self.stop();
            return;
        }
        let s_idx = stream_index.unwrap();

        if let Some(sink) = &self.active_sink {
            sink.set_volume(self.volume.clamp(0.0, 1.5));
            sink.set_speed(clamped_speed as f32);
        }

        // Check if current audio stream session is already locked to the video playhead
        if let (Some(cur_path), Some(cur_idx), Some(wall_start)) = (
            &self.active_path,
            self.active_stream_index,
            self.session_wall_start,
        ) {
            if cur_path == clip_path
                && cur_idx == s_idx
                && self.active_delay_ms == delay_ms
                && (self.active_speed - clamped_speed).abs() < 0.01
            {
                let elapsed_us =
                    (wall_start.elapsed().as_secs_f64() * clamped_speed * 1_000_000.0) as i64;
                let expected_source_us = self.session_source_start_us + elapsed_us;
                let drift_us = (expected_source_us - source_pts.0).abs();
                // Allow up to 280ms tolerance before forcing an audio re-seek
                if drift_us < (280_000.0 * clamped_speed) as i64 {
                    return;
                }
            }
        }

        // Otherwise, start a new synchronized audio stream at `source_pts - delay_ms`
        self.stop();
        let Some(ref handle) = self.stream_handle else {
            return;
        };
        let Ok(sink) = Sink::try_new(handle) else {
            return;
        };
        sink.set_volume(self.volume.clamp(0.0, 1.5));
        sink.set_speed(clamped_speed as f32);
        let sink_arc = Arc::new(sink);
        let stop_flag = Arc::new(AtomicBool::new(false));

        let effective_start_us = (source_pts.0 - delay_ms * 1_000).max(0);
        let start_secs = effective_start_us as f64 / 1_000_000.0;
        let ffmpeg_path = binaries.ffmpeg_path.clone();
        let input_path = clip_path.to_path_buf();
        let sink_worker = Arc::clone(&sink_arc);
        let stop_worker = Arc::clone(&stop_flag);

        thread::Builder::new()
            .name("cutdub-audio-player".into())
            .spawn(move || {
                let sample_rate = 48_000u32;
                let channels = 2u16;
                let mut cmd = Command::new(ffmpeg_path);
                configure_background_command(&mut cmd);
                cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
                if start_secs > 0.001 {
                    cmd.arg("-ss").arg(format!("{start_secs:.6}"));
                }
                cmd.arg("-i").arg(&input_path);
                cmd.args([
                    "-map",
                    &format!("0:{s_idx}"),
                    "-vn",
                    "-ac",
                    "2",
                    "-ar",
                    &sample_rate.to_string(),
                    "-f",
                    "f32le",
                    "pipe:1",
                ]);
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::null());

                let Ok(mut child) = cmd.spawn() else {
                    return;
                };
                let Some(mut stdout) = child.stdout.take() else {
                    let _ = child.kill();
                    return;
                };

                // Read 20ms chunks (960 stereo frames = 1920 f32 samples = 7680 bytes)
                let chunk_samples = 1920usize;
                let mut byte_buf = vec![0u8; chunk_samples * 4];

                while !stop_worker.load(Ordering::Relaxed) {
                    // Backpressure: keep ~240ms (12 chunks) queued in the audio sink
                    if sink_worker.len() >= 12 {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    match stdout.read_exact(&mut byte_buf) {
                        Ok(()) => {
                            if stop_worker.load(Ordering::Relaxed) {
                                break;
                            }
                            let mut samples = Vec::with_capacity(chunk_samples);
                            for b in byte_buf.chunks_exact(4) {
                                let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                                samples.push(s.clamp(-1.0, 1.0));
                            }
                            let buf = SamplesBuffer::new(channels, sample_rate, samples);
                            sink_worker.append(buf);
                        }
                        Err(_) => break,
                    }
                }

                let _ = child.kill();
                let _ = child.wait();
            })
            .ok();

        self.active_sink = Some(sink_arc);
        self.stop_flag = Some(stop_flag);
        self.active_path = Some(clip_path.to_path_buf());
        self.active_stream_index = Some(s_idx);
        self.active_delay_ms = delay_ms;
        self.active_speed = clamped_speed;
        self.session_wall_start = Some(Instant::now());
        self.session_source_start_us = source_pts.0;
    }
}

impl Drop for AudioPreviewPlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_frame_cache_eviction() {
        let mut cache = FrameCache::new(16);
        for i in 0..20 {
            cache.insert(DecodedFrame {
                source_id: 0,
                source_frame: i,
                pts_us: TimestampUs(i as i64 * 33_333),
                width: 16,
                height: 16,
                rgba: Arc::new(vec![0u8; 16 * 16 * 4]),
            });
        }
        assert_eq!(cache.len(), 16);
        assert!(cache
            .get(FrameKey {
                source_id: 0,
                source_frame: 0
            })
            .is_none());
        assert!(cache
            .get(FrameKey {
                source_id: 0,
                source_frame: 19
            })
            .is_some());
    }
}
