use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use egui::{
    Align, Color32, ColorImage, Context, CornerRadius, FontId, Key, Layout, Margin, Modifiers,
    Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use redub_core::{
    AudioExportMode, AudioWaveform, FrameTimingMap, Project, ResolvedFrame, TimestampUs,
};
use redub_export::{
    execute_automation_script, is_lossless_or_pcm_audio_encoder, start_audio_only_export_job,
    start_batch_queue, start_export_job, start_split_by_markers_export_job, AudioChannelMode,
    AudioProcessingMode, AutomationScript, BatchEvent, BatchItemStatus, BatchJobHandle,
    BatchJobItem, ContainerFormat, EncodingPreset, ExportEvent, ExportJobHandle, ExportSettings,
    KeyframeCutPolicy, VideoProcessingMode, VideoRateControlMode,
};
use redub_filters::{
    compute_rgba_histogram, AspectRatioMode, ConvolutionPreset, DeinterlaceEngine,
    DeinterlaceMode, DenoiseMode, FieldParity, FilterKind, FilterPipeline, FrameHistogram,
    OverlayPosition, ResizeSpecMode, RotationAngle, ScalingAlgorithm, SharpenMode,
};
use redub_media::{
    detect_scenes_and_black_frames, export_frame_snapshot, extract_audio_waveform,
    generate_synthetic_video, probe_local_keyframes, probe_media_file, probe_packet_timing_map,
    DetectedSceneMarker, FfmpegProcessDecoder, MediaCapabilities, SceneMarkerKind,
};
use redub_platform::{AppPaths, AppSettings, FfmpegBinaries};
use redub_preview::{AudioPreviewPlayer, NavigationHint, PlaybackClock, PreviewEngine};
use tracing::{error, info};

#[derive(Default)]
struct PreviewLatency {
    total_ms: f64,
    wait_ms: f64,
    filters_ms: f64,
    histogram_ms: f64,
    texture_ms: f64,
    cached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewLayoutMode {
    DualSideBySide,
    SourceOnly,
    OutputOnly,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreviewZoom {
    FitToPanel,
    Scale(f32),
}

impl PreviewZoom {
    pub fn label(self) -> &'static str {
        match self {
            Self::FitToPanel => "Fit",
            Self::Scale(s) if (s - 0.5).abs() < 0.01 => "50%",
            Self::Scale(s) if (s - 1.0).abs() < 0.01 => "100%",
            Self::Scale(s) if (s - 2.0).abs() < 0.01 => "200%",
            Self::Scale(_) => "Custom",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightSidebarTab {
    Streams,
    Filters,
    Export,
    Batch,
    Edl,
}

#[derive(Debug, Clone)]
struct ActiveExportStatus {
    fraction: f32,
    encoded_time: TimestampUs,
    total_duration: TimestampUs,
    fps: f32,
    speed: String,
    size_bytes: u64,
}

pub struct CutDubApp {
    paths: AppPaths,
    settings: AppSettings,
    binaries: Option<FfmpegBinaries>,
    toolchain_error: Option<String>,
    capabilities: MediaCapabilities,

    project: Project,
    filters: FilterPipeline,
    export_settings: ExportSettings,
    presets: Vec<EncodingPreset>,
    new_preset_name: String,
    selected_preset: Option<(String, ExportSettings)>,

    preview_engine: Option<PreviewEngine>,
    playback: PlaybackClock,
    audio_player: AudioPreviewPlayer,

    source_texture: Option<TextureHandle>,
    output_texture: Option<TextureHandle>,
    displayed_frame_key: Option<(usize, u64, u64)>, // (source_id, source_frame, filter_hash)
    pending_preview: Option<((usize, u64), Instant)>,
    pending_preview_cache_hit: Option<bool>,
    preview_latency: Option<PreviewLatency>,
    current_histogram: Option<FrameHistogram>,
    show_histogram: bool,

    preview_layout: PreviewLayoutMode,
    preview_zoom: PreviewZoom,
    show_right_sidebar: bool,
    active_sidebar_tab: RightSidebarTab,

    // Modals / dialogs
    show_goto_dialog: bool,
    goto_frame_input: String,
    goto_time_input: String,
    show_shortcuts_dialog: bool,
    show_ffmpeg_dialog: bool,
    show_about_dialog: bool,

    // Background packet timing scanner channel: (source_id, FrameTimingMap)
    timing_scan_rx: Option<Receiver<(usize, FrameTimingMap)>>,
    is_scanning_timing: bool,
    has_full_timing_index: bool,
    local_kf_rx: Option<Receiver<(usize, TimestampUs, TimestampUs, Vec<TimestampUs>)>>,
    probed_kf_window: Option<(usize, TimestampUs, TimestampUs)>,

    // Background audio waveform extractor channel: (source_id, AudioWaveform)
    waveform_rx: Option<Receiver<(usize, AudioWaveform)>>,
    show_waveform: bool,

    // Active export job
    export_job: Option<ExportJobHandle>,
    export_progress: Option<ActiveExportStatus>,

    // Batch processing queue
    batch_items: Vec<BatchJobItem>,
    batch_output_dir: Option<PathBuf>,
    batch_job: Option<BatchJobHandle>,

    // Scene change & black-frame detector
    scene_markers: Vec<DetectedSceneMarker>,
    scene_scan_rx: Option<Receiver<Vec<DetectedSceneMarker>>>,
    is_scanning_scenes: bool,
    scene_threshold: f32,
    min_black_duration: f32,

    // Timeline zoom (1.0..64.0) & horizontal pan center fraction (0.0..1.0)
    timeline_zoom: f32,
    timeline_pan_center: f32,

    status_banner: Option<(String, bool)>, // (message, is_error)
}

impl CutDubApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<PathBuf>) -> Self {
        configure_dark_theme(&cc.egui_ctx);

        let paths = AppPaths::resolve();
        let _ = paths.ensure_dirs();
        let settings = AppSettings::load(&paths);
        let presets = EncodingPreset::load_all(&paths.presets_file);

        let discovery = FfmpegBinaries::discover(
            settings.custom_ffmpeg_path.as_deref(),
            settings.custom_ffprobe_path.as_deref(),
        );
        let toolchain_error = discovery.as_ref().err().map(ToString::to_string);
        let show_ffmpeg_dialog = toolchain_error.is_some();
        let binaries = discovery.ok();

        let capabilities = binaries
            .as_ref()
            .map(MediaCapabilities::detect)
            .unwrap_or_default();

        let preview_engine = binaries.as_ref().map(|bins| {
            let decoder =
                FfmpegProcessDecoder::new(bins.clone(), settings.prefer_hardware_decode);
            PreviewEngine::new(decoder, settings.frame_cache_capacity)
        });

        let mut export_settings = ExportSettings::default();
        if let Some(first_enc) = capabilities.preferred_video_encoders().first() {
            export_settings.video_encoder = first_enc.name.clone();
        }

        let mut app = Self {
            paths,
            settings,
            binaries,
            toolchain_error,
            capabilities,
            project: Project::new(),
            filters: FilterPipeline::new(),
            export_settings,
            presets,
            new_preset_name: String::new(),
            selected_preset: None,
            preview_engine,
            playback: PlaybackClock::default(),
            audio_player: AudioPreviewPlayer::new(),
            source_texture: None,
            output_texture: None,
            displayed_frame_key: None,
            pending_preview: None,
            pending_preview_cache_hit: None,
            preview_latency: None,
            current_histogram: None,
            show_histogram: true,
            preview_layout: PreviewLayoutMode::DualSideBySide,
            preview_zoom: PreviewZoom::FitToPanel,
            show_right_sidebar: true,
            active_sidebar_tab: RightSidebarTab::Streams,
            show_goto_dialog: false,
            goto_frame_input: String::new(),
            goto_time_input: String::new(),
            show_shortcuts_dialog: false,
            show_ffmpeg_dialog,
            show_about_dialog: false,
            timing_scan_rx: None,
            is_scanning_timing: false,
            has_full_timing_index: false,
            local_kf_rx: None,
            probed_kf_window: None,
            waveform_rx: None,
            show_waveform: true,
            export_job: None,
            export_progress: None,
            batch_items: Vec::new(),
            batch_output_dir: None,
            batch_job: None,
            scene_markers: Vec::new(),
            scene_scan_rx: None,
            is_scanning_scenes: false,
            scene_threshold: 12.0,
            min_black_duration: 0.20,
            timeline_zoom: 1.0,
            timeline_pan_center: 0.5,
            status_banner: None,
        };

        if let Some(path) = initial_file {
            app.open_media_file(path);
        }

        app
    }

    fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        let s = msg.into();
        if is_error {
            error!("{s}");
        } else {
            info!("{s}");
        }
        self.status_banner = Some((s, is_error));
    }

    fn open_media_file(&mut self, path: PathBuf) {
        let Some(bins) = self.binaries.clone() else {
            self.show_ffmpeg_dialog = true;
            self.set_status(self.toolchain_error.clone().unwrap_or_else(||
                "FFmpeg and ffprobe are required to open videos.".into()), true);
            return;
        };

        self.playback.pause();
        self.audio_player.stop();
        match probe_media_file(&bins, &path, false) {
            Ok(clip) => {
                let nominal_fps = clip.timing.nominal_fps;
                let time_base = clip.timing.time_base;
                let source_id = 0;

                let file_size = clip.file_size_bytes;
                self.project = Project::from_primary_source(clip);
                if let Some(engine) = &mut self.preview_engine {
                    engine.reset();
                }
                self.displayed_frame_key = None;
                self.pending_preview = None;
                self.preview_latency = None;
                self.pending_preview_cache_hit = None;
                self.has_full_timing_index = false;
                self.probed_kf_window = None;
                self.local_kf_rx = None;
                self.scene_markers.clear();
                self.scene_scan_rx = None;
                self.is_scanning_scenes = false;
                self.settings.add_recent_file(path.clone());
                let _ = self.settings.save(&self.paths);

                self.set_status(
                    format!("Loaded '{}'", path.display()),
                    false,
                );

                // Avoid saturating external hard drive I/O on large files (> 150 MB);
                // only auto-run full-file packet & audio waveform scans on smaller clips.
                if file_size <= 150 * 1024 * 1024 {
                    let (tx, rx) = mpsc::channel();
                    self.timing_scan_rx = Some(rx);
                    self.is_scanning_timing = true;
                    let bins_for_timing = bins.clone();
                    let path_for_timing = path.clone();
                    thread::Builder::new()
                        .name("cutdub-packet-indexer".into())
                        .spawn(move || {
                            if let Ok(map) = probe_packet_timing_map(
                                &bins_for_timing,
                                &path_for_timing,
                                nominal_fps,
                                time_base,
                            ) {
                                let _ = tx.send((source_id, map));
                            }
                        })
                        .ok();

                    if let Some(first_audio) = self
                        .project
                        .primary_source()
                        .and_then(|s| s.audio_streams.first())
                    {
                        let stream_idx = first_audio.stream_index;
                        let (w_tx, w_rx) = mpsc::channel();
                        self.waveform_rx = Some(w_rx);
                        thread::Builder::new()
                            .name("cutdub-waveform-extractor".into())
                            .spawn(move || {
                                if let Ok(wf) =
                                    extract_audio_waveform(&bins, &path, stream_idx, 512)
                                {
                                    let _ = w_tx.send((source_id, wf));
                                }
                            })
                            .ok();
                    }
                }
            }
            Err(e) => {
                self.set_status(format!("Failed to open '{}': {e}", path.display()), true);
            }
        }
    }

    fn append_media_file(&mut self, path: PathBuf) {
        let Some(bins) = self.binaries.clone() else {
            return;
        };
        match probe_media_file(&bins, &path, true) {
            Ok(clip) => match self.project.append_source(clip) {
                Ok(()) => {
                    self.set_status(
                        format!("Appended '{}' to timeline", path.display()),
                        false,
                    );
                }
                Err(e) => {
                    self.set_status(format!("Cannot append file: {e}"), true);
                }
            },
            Err(e) => {
                self.set_status(format!("Failed to probe '{}': {e}", path.display()), true);
            }
        }
    }

    fn generate_and_load_sample_video(&mut self) {
        let Some(bins) = self.binaries.clone() else {
            self.set_status("FFmpeg binary not found.", true);
            return;
        };
        let sample_path = self.paths.cache_dir.join("cutdub_demo_clip.mp4");
        let _ = self.paths.ensure_dirs();
        match generate_synthetic_video(&bins, &sample_path, 960, 540, 30, 5.0, 15) {
            Ok(()) => {
                self.open_media_file(sample_path);
            }
            Err(e) => {
                self.set_status(format!("Failed to generate sample video: {e}"), true);
            }
        }
    }

    fn trigger_snapshot_dialog(&mut self) {
        let Some(resolved) = self.project.resolve_frame(self.project.current_frame) else {
            self.set_status("Load a video file before taking a frame snapshot.", true);
            return;
        };
        let Some(clip) = self.project.source_by_id(resolved.source_id).cloned() else {
            return;
        };
        let Some(bins) = self.binaries.clone() else {
            return;
        };

        let stem = clip
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("frame");
        let default_name = format!("{stem}_frame_{:06}.png", resolved.virtual_frame);

        let Some(out_path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter("PNG Image", &["png"])
            .add_filter("JPEG Image", &["jpg", "jpeg"])
            .save_file()
        else {
            return;
        };

        let (vf, _, _) = self.filters.to_ffmpeg_filtergraph(clip.width, clip.height);
        match export_frame_snapshot(&bins, &clip, resolved.source_frame, vf.as_deref(), &out_path) {
            Ok(()) => {
                self.set_status(
                    format!("Saved frame #{} snapshot to '{}'", resolved.virtual_frame, out_path.display()),
                    false,
                );
            }
            Err(e) => {
                self.set_status(format!("Snapshot failed: {e}"), true);
            }
        }
    }

    fn trigger_export_dialog(&mut self) {
        if !self.export_settings.encoders_available(&self.capabilities) {
            self.set_status("The selected encoder is unavailable. Choose an installed encoder or an available preset.", true);
            return;
        }
        if !self.project.is_loaded() {
            self.set_status("Load a video file before exporting.", true);
            return;
        }
        let Some(bins) = self.binaries.clone() else {
            self.set_status("FFmpeg binary not available.", true);
            return;
        };

        let ext = self.export_settings.container.extension();
        let default_name = self
            .project
            .primary_source()
            .and_then(|s| s.path.file_stem())
            .and_then(|s| s.to_str())
            .map(|stem| format!("{stem}_cutdub.{ext}"))
            .unwrap_or_else(|| format!("output.{ext}"));

        let Some(output_path) = rfd::FileDialog::new()
            .set_file_name(&default_name)
            .add_filter(self.export_settings.container.label(), &[ext])
            .save_file()
        else {
            return;
        };

        // Pause preview playback and close any background streaming decoder so 100% of CPU & disk I/O go to export
        self.playback.pause();
        self.audio_player.stop();
        if let Some(engine) = &mut self.preview_engine {
            engine.sync_playhead(self.project.current_frame, false);
        }

        match start_export_job(
            bins,
            self.project.clone(),
            self.filters.clone(),
            self.export_settings.clone(),
            output_path,
        ) {
            Ok(handle) => {
                self.export_progress = Some(ActiveExportStatus {
                    fraction: 0.0,
                    encoded_time: TimestampUs::ZERO,
                    total_duration: self.project.total_duration(),
                    fps: 0.0,
                    speed: "starting...".into(),
                    size_bytes: 0,
                });
                self.export_job = Some(handle);
                self.set_status("Export started in background...", false);
            }
            Err(e) => {
                self.set_status(format!("Failed to start export: {e}"), true);
            }
        }
    }

    fn poll_background_tasks(&mut self, ctx: &Context) {
        // 1. Poll background packet timing scanner
        if let Some(rx) = &self.timing_scan_rx {
            if let Ok((source_id, map)) = rx.try_recv() {
                let kf_count = map.keyframe_count();
                let total = map.total_frames();
                let is_vfr = map.is_vfr;
                self.project.update_source_timing(source_id, map);
                self.is_scanning_timing = false;
                self.has_full_timing_index = true;
                self.timing_scan_rx = None;
                self.set_status(
                    format!(
                        "Indexed {total} frames ({kf_count} keyframes, {})",
                        if is_vfr { "VFR" } else { "CFR" }
                    ),
                    false,
                );
                ctx.request_repaint();
            }
        }

        // 1a. Fast local-window keyframe indexing on large files (>150 MB)
        if let Some(rx) = &self.local_kf_rx {
            if let Ok((src_id, w_start, w_end, keyframes)) = rx.try_recv() {
                self.project
                    .update_source_local_keyframes(src_id, w_start, w_end, &keyframes);
                self.probed_kf_window = Some((src_id, w_start, w_end));
                self.local_kf_rx = None;
                ctx.request_repaint();
            }
        } else if !self.has_full_timing_index
            && !self.is_scanning_timing
            && !self.playback.is_playing
        {
            if let (Some(bins), Some(resolved)) = (
                self.binaries.clone(),
                self.project.resolve_frame(self.project.current_frame),
            ) {
                let covered = match self.probed_kf_window {
                    Some((id, ws, we)) => {
                        id == resolved.source_id
                            && resolved.source_pts.0 >= ws.0 + 10_000_000
                            && resolved.source_pts.0 <= we.0 - 10_000_000
                    }
                    None => false,
                };
                if !covered {
                    if let Some(clip) = self.project.source_by_id(resolved.source_id).cloned() {
                        let (tx, rx) = mpsc::channel();
                        self.local_kf_rx = Some(rx);
                        let src_id = resolved.source_id;
                        let center_ts = resolved.source_pts;
                        // Optimistically record window so we don't re-trigger while in flight
                        self.probed_kf_window = Some((
                            src_id,
                            TimestampUs((center_ts.0 - 60_000_000).max(0)),
                            TimestampUs(center_ts.0 + 60_000_000),
                        ));
                        thread::Builder::new()
                            .name("cutdub-local-kf-probe".into())
                            .spawn(move || {
                                if let Ok((ws, we, kfs)) =
                                    probe_local_keyframes(&bins, &clip.path, center_ts, 60.0)
                                {
                                    let _ = tx.send((src_id, ws, we, kfs));
                                }
                            })
                            .ok();
                    }
                }
            }
        }

        // 1b. Poll background audio waveform extractor
        if let Some(rx) = &self.waveform_rx {
            if let Ok((source_id, wf)) = rx.try_recv() {
                self.project.update_source_waveform(source_id, wf);
                self.waveform_rx = None;
                ctx.request_repaint();
            }
        }

        // 1b-2. Poll background scene & black-frame detector
        if let Some(rx) = &self.scene_scan_rx {
            if let Ok(markers) = rx.try_recv() {
                let count = markers.len();
                self.scene_markers = markers;
                self.is_scanning_scenes = false;
                self.scene_scan_rx = None;
                self.set_status(
                    format!("Detected {count} scene cut / black-frame marker(s)."),
                    false,
                );
                ctx.request_repaint();
            }
        }

        // 1c. Poll active batch processing queue
        let mut clear_batch = false;
        let mut batch_status_msg: Option<(String, bool)> = None;
        if let Some(b_job) = &self.batch_job {
            while let Ok(ev) = b_job.receiver.try_recv() {
                match ev {
                    BatchEvent::ItemStarted { index } => {
                        if let Some(item) = self.batch_items.get_mut(index) {
                            item.status = BatchItemStatus::Running { fraction: 0.0 };
                        }
                        ctx.request_repaint();
                    }
                    BatchEvent::ItemProgress { index, fraction } => {
                        if let Some(item) = self.batch_items.get_mut(index) {
                            item.status = BatchItemStatus::Running { fraction };
                        }
                        ctx.request_repaint();
                    }
                    BatchEvent::ItemCompleted { index, duration } => {
                        if let Some(item) = self.batch_items.get_mut(index) {
                            item.status = BatchItemStatus::Completed { duration };
                        }
                        ctx.request_repaint();
                    }
                    BatchEvent::ItemFailed { index, error } => {
                        if let Some(item) = self.batch_items.get_mut(index) {
                            item.status = BatchItemStatus::Failed(error);
                        }
                        ctx.request_repaint();
                    }
                    BatchEvent::AllFinished => {
                        batch_status_msg =
                            Some(("Batch processing queue completed.".to_string(), false));
                        clear_batch = true;
                        ctx.request_repaint();
                    }
                    BatchEvent::Cancelled => {
                        for item in &mut self.batch_items {
                            if matches!(item.status, BatchItemStatus::Running { .. }) {
                                item.status = BatchItemStatus::Cancelled;
                            }
                        }
                        batch_status_msg = Some(("Batch queue cancelled.".to_string(), false));
                        clear_batch = true;
                        ctx.request_repaint();
                    }
                }
            }
        }
        if let Some((m, err)) = batch_status_msg {
            self.set_status(m, err);
        }
        if clear_batch {
            self.batch_job = None;
        }
        let mut clear_job = false;
        let mut pending_status: Option<(String, bool)> = None;
        if let Some(job) = &self.export_job {
            while let Ok(event) = job.receiver.try_recv() {
                match event {
                    ExportEvent::Progress {
                        fraction,
                        encoded_time,
                        total_duration,
                        fps,
                        speed,
                        size_bytes,
                    } => {
                        self.export_progress = Some(ActiveExportStatus {
                            fraction,
                            encoded_time,
                            total_duration,
                            fps,
                            speed,
                            size_bytes,
                        });
                        ctx.request_repaint();
                    }
                    ExportEvent::Completed {
                        output_path,
                        duration,
                    } => {
                        pending_status = Some((
                            format!(
                                "Export complete: '{}' ({})",
                                output_path.display(),
                                duration
                            ),
                            false,
                        ));
                        clear_job = true;
                        ctx.request_repaint();
                    }
                    ExportEvent::Cancelled => {
                        pending_status = Some(("Export cancelled by user.".to_string(), false));
                        clear_job = true;
                        ctx.request_repaint();
                    }
                    ExportEvent::Failed(err) => {
                        pending_status = Some((format!("Export failed: {err}"), true));
                        clear_job = true;
                        ctx.request_repaint();
                    }
                }
            }
        }
        if let Some((msg, is_err)) = pending_status {
            self.set_status(msg, is_err);
        }
        if clear_job {
            self.export_job = None;
            self.export_progress = None;
        }

        // 3. Drain incoming decoded frames first, synchronize playback clock against buffer readiness,
        // and update preview textures.
        if let Some(engine) = &mut self.preview_engine {
            engine.poll_decoded_frames();
        }

        if self.playback.is_playing {
            let ready = if let (Some(engine), Some(resolved)) = (
                &self.preview_engine,
                self.project.resolve_frame(self.project.current_frame),
            ) {
                engine.cache.contains(redub_preview::FrameKey {
                    source_id: resolved.source_id,
                    source_frame: resolved.source_frame,
                })
            } else {
                false
            };

            if ready {
                if self.playback.tick(&mut self.project) {
                    ctx.request_repaint_after(Duration::from_millis(8));
                }
            } else {
                // Hold clock anchor until first frame arrives
                if let Some(resolved) = self.project.resolve_frame(self.project.current_frame) {
                    self.playback.reanchor(resolved.virtual_pts);
                }
                ctx.request_repaint_after(Duration::from_millis(8));
            }
        }

        // Synchronize live speaker audio output with video playback
        if let (Some(bins), Some(resolved)) = (
            self.binaries.as_ref(),
            self.project.resolve_frame(self.project.current_frame),
        ) {
            if let Some(clip) = self.project.source_by_id(resolved.source_id) {
                let active_audio = self
                    .project
                    .audio_configs
                    .iter()
                    .find(|c| c.mode != AudioExportMode::Disabled);
                let stream_idx = active_audio.map(|c| c.info.stream_index);
                let delay_ms = active_audio.map(|c| c.delay_ms).unwrap_or(0);
                self.audio_player.sync_playback(
                    bins,
                    &clip.path,
                    stream_idx,
                    resolved.source_pts,
                    delay_ms,
                    self.playback.speed,
                    self.playback.is_playing,
                );
            }
        } else {
            self.audio_player.stop();
        }

        self.update_preview_textures(ctx);
    }

    fn filter_pipeline_signature(&self) -> u64 {
        // Lightweight deterministic signature for filter changes so Output texture updates immediately
        let json = serde_json::to_string(&self.filters).unwrap_or_default();
        let mut hash = 14695981039346656037u64;
        for b in json.bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
        hash
    }

    fn update_preview_textures(&mut self, ctx: &Context) {
        let Some(resolved) = self.project.resolve_frame(self.project.current_frame) else {
            return;
        };
        let filter_sig = self.filter_pipeline_signature();
        let target_key = (resolved.source_id, resolved.source_frame, filter_sig);

        let source_key = (resolved.source_id, resolved.source_frame);
        if self.displayed_frame_key != Some(target_key)
            && self.pending_preview.as_ref().map(|(key, _)| *key) != Some(source_key)
        {
            self.pending_preview = Some((source_key, Instant::now()));
            self.pending_preview_cache_hit = None;
        }
        let Some(engine) = &mut self.preview_engine else {
            return;
        };

        let newly_arrived = engine.poll_decoded_frames();
        if newly_arrived {
            ctx.request_repaint();
        }
        engine.sync_playhead(resolved.source_frame, self.playback.is_playing);

        if self.displayed_frame_key == Some(target_key) && !self.playback.is_playing {
            return;
        }

        let Some(clip) = self.project.source_by_id(resolved.source_id) else {
            return;
        };
        let cached = engine.cache.contains(redub_preview::FrameKey {
            source_id: clip.id, source_frame: resolved.source_frame,
        });
        let cached = *self.pending_preview_cache_hit.get_or_insert(cached);
        if let Some(decoded) =
            engine.request_frame(clip, resolved.source_frame, self.playback.is_playing)
        {
            if self.displayed_frame_key != Some(target_key) {
                let requested_at = self.pending_preview.map(|(_, at)| at).unwrap_or_else(Instant::now);
                let mut sample = PreviewLatency {
                    wait_ms: requested_at.elapsed().as_secs_f64() * 1000.0,
                    cached,
                    ..Default::default()
                };
                let texture_started = Instant::now();
                let src_image = ColorImage::from_rgba_unmultiplied(
                    [decoded.width as usize, decoded.height as usize],
                    &decoded.rgba,
                );
                self.source_texture = Some(ctx.load_texture(
                    "cutdub_source_preview",
                    src_image,
                    TextureOptions::LINEAR,
                ));

                sample.texture_ms += texture_started.elapsed().as_secs_f64() * 1000.0;
                // Compute Output preview (applying active filters when in Full Processing mode or when filters are enabled)
                if !self.filters.is_empty() {
                    let filters_started = Instant::now();
                    let (out_w, out_h, out_rgba) = self.filters.apply_to_rgba(
                        decoded.width,
                        decoded.height,
                        (*decoded.rgba).clone(),
                    );
                    sample.filters_ms = filters_started.elapsed().as_secs_f64() * 1000.0;
                    if self.show_histogram {
                        let histogram_started = Instant::now();
                        self.current_histogram = Some(compute_rgba_histogram(&out_rgba));
                        sample.histogram_ms = histogram_started.elapsed().as_secs_f64() * 1000.0;
                    }
                    let texture_started = Instant::now();
                    let out_image = ColorImage::from_rgba_unmultiplied(
                        [out_w as usize, out_h as usize],
                        &out_rgba,
                    );
                    self.output_texture = Some(ctx.load_texture(
                        "cutdub_output_preview",
                        out_image,
                        TextureOptions::LINEAR,
                    ));
                    sample.texture_ms += texture_started.elapsed().as_secs_f64() * 1000.0;
                } else {
                    if self.show_histogram {
                        let histogram_started = Instant::now();
                        self.current_histogram = Some(compute_rgba_histogram(&decoded.rgba));
                        sample.histogram_ms = histogram_started.elapsed().as_secs_f64() * 1000.0;
                    }
                    self.output_texture = self.source_texture.clone();
                }

                self.displayed_frame_key = Some(target_key);
                sample.total_ms = requested_at.elapsed().as_secs_f64() * 1000.0;
                if !self.playback.is_playing {
                    tracing::debug!(source_id = resolved.source_id, frame = resolved.source_frame,
                        total_ms = sample.total_ms, wait_ms = sample.wait_ms,
                        filters_ms = sample.filters_ms, histogram_ms = sample.histogram_ms,
                        texture_queue_ms = sample.texture_ms, cached = sample.cached,
                        "Navigation preview prepared (CPU timing; excludes GPU presentation)");
                    engine.prefetch_navigation(clip, resolved.source_frame);
                }
                self.pending_preview = None;
                self.pending_preview_cache_hit = None;
                self.preview_latency = Some(sample);
            }
        } else {
            // Decode is in flight on worker thread; schedule repaint so we pick it up promptly
            ctx.request_repaint_after(Duration::from_millis(12));
        }
    }

    fn handle_dropped_files(&mut self, ctx: &Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for path in dropped {
            if path
                .extension()
                .and_then(|s| s.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("json"))
                .unwrap_or(false)
            {
                if let Ok(proj) = Project::load_from_file(&path) {
                    self.project = proj;
                    self.set_status(format!("Loaded project '{}'", path.display()), false);
                }
            } else {
                self.open_media_file(path);
            }
        }
    }

    fn handle_keyboard_shortcuts(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }

        let input_started = Instant::now();
        let previous_frame = self.project.current_frame;
        let mut open_dialog = false;
        let mut trigger_export = false;
        let mut trigger_snapshot = false;
        let preview_ready = self.project.resolve_frame(self.project.current_frame)
            .is_some_and(|resolved| self.displayed_frame_key
                .is_some_and(|(source_id, frame, _)|
                    source_id == resolved.source_id && frame == resolved.source_frame));
        ctx.input_mut(|i| {
            // File / Project shortcuts
            if i.consume_key(Modifiers::COMMAND, Key::O) {
                open_dialog = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::E) {
                trigger_export = true;
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::S) {
                trigger_snapshot = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::G) {
                self.show_goto_dialog = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                self.show_right_sidebar = true;
                self.active_sidebar_tab = RightSidebarTab::Filters;
            }

            // Undo / Redo
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
                || i.consume_key(Modifiers::COMMAND, Key::Y)
            {
                self.project.redo();
            } else if i.consume_key(Modifiers::COMMAND, Key::Z) {
                self.project.undo();
            }

            if !self.project.is_loaded() {
                return;
            }

            // Playback toggle: Space, plus J / K / L shuttle keys
            let cur_pts = self
                .project
                .resolve_frame(self.project.current_frame)
                .map(|r| r.virtual_pts)
                .unwrap_or(TimestampUs::ZERO);

            if i.key_pressed(Key::Space) {
                self.playback.toggle(cur_pts);
            } else if i.key_pressed(Key::K) {
                self.playback.pause();
                self.playback.set_speed(1.0, cur_pts);
            } else if i.key_pressed(Key::L) {
                if !self.playback.is_playing {
                    self.playback.start(cur_pts);
                } else {
                    self.playback.step_speed_up(cur_pts);
                }
            } else if i.key_pressed(Key::J) {
                if self.playback.is_playing {
                    self.playback.step_speed_down(cur_pts);
                } else {
                    self.project.step_backward(5);
                }
            }

            // Only deliberate presses may supersede an image still decoding.
            // Held-key repeats are discarded until the previous target is visible.
            let repeat_ready = navigation_repeat_ready(&i.events, preview_ready);
            // Navigation: Left/Right (1 frame), Shift+Left/Right (Prev/Next Keyframe), Alt+Left/Right (10 frames)
            if i.consume_key(Modifiers::SHIFT, Key::ArrowLeft)
                || i.key_pressed(Key::PageUp)
            {
                self.playback.pause();
                if repeat_ready {
                    self.project.seek_prev_keyframe();
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Keyframes { backwards: true });
                    }
                }
            } else if i.consume_key(Modifiers::SHIFT, Key::ArrowRight)
                || i.key_pressed(Key::PageDown)
            {
                self.playback.pause();
                if repeat_ready {
                    self.project.seek_next_keyframe();
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Keyframes { backwards: false });
                    }
                }
            } else if i.consume_key(Modifiers::ALT, Key::ArrowLeft) {
                self.playback.pause();
                if repeat_ready {
                    self.project.step_backward(10);
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Frames(-10));
                    }
                }
            } else if i.consume_key(Modifiers::ALT, Key::ArrowRight) {
                self.playback.pause();
                if repeat_ready {
                    self.project.step_forward(10);
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Frames(10));
                    }
                }
            } else if i.key_pressed(Key::ArrowLeft) {
                self.playback.pause();
                if repeat_ready {
                    self.project.step_backward(1);
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Frames(-1));
                    }
                }
            } else if i.key_pressed(Key::ArrowRight) {
                self.playback.pause();
                if repeat_ready {
                    self.project.step_forward(1);
                    if let Some(engine) = &mut self.preview_engine {
                        engine.navigation_hint = Some(NavigationHint::Frames(1));
                    }
                }
            }

            if i.key_pressed(Key::Home) {
                self.playback.pause();
                self.project.seek_start();
            }
            if i.key_pressed(Key::End) {
                self.playback.pause();
                self.project.seek_end();
            }

            // Selection markers: [ or I for Mark In, ] or O for Mark Out, M for Timeline Marker / Bookmark
            if i.key_pressed(Key::OpenBracket) || i.key_pressed(Key::I) {
                self.project.set_mark_in();
            }
            if i.key_pressed(Key::CloseBracket) || i.key_pressed(Key::O) {
                self.project.set_mark_out();
            }
            if i.key_pressed(Key::M) {
                let added = self.project.toggle_marker_at_current();
                let f = self.project.current_frame;
                self.status_banner = Some((
                    if added {
                        format!("Added timeline marker at frame #{f}")
                    } else {
                        format!("Removed timeline marker at frame #{f}")
                    },
                    false,
                ));
            }

            // Delete selection: Delete or Backspace
            if i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace) {
                if let Err(e) = self.project.delete_selection() {
                    self.status_banner = Some((e.to_string(), true));
                }
            }

            // Trim to selection: Cmd/Ctrl + T
            if i.consume_key(Modifiers::COMMAND, Key::T) {
                if let Err(e) = self.project.trim_to_selection() {
                    self.status_banner = Some((e.to_string(), true));
                }
            }
        });
        if self.project.current_frame != previous_frame {
            if let Some(resolved) = self.project.resolve_frame(self.project.current_frame) {
                self.pending_preview = Some(((resolved.source_id, resolved.source_frame), input_started));
                self.pending_preview_cache_hit = None;
            }
        }
        if open_dialog {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    "Video Files",
                    &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv"],
                )
                .pick_file()
            {
                self.open_media_file(path);
            }
        }
        if trigger_export {
            self.trigger_export_dialog();
        }
        if trigger_snapshot {
            self.trigger_snapshot_dialog();
        }
    }

    fn render_menu_bar(&mut self, ui: &mut Ui) {
        egui::menu::bar(ui, |ui| {
            // FILE MENU
            ui.menu_button("File", |ui| {
                if ui.button("Open Video File... (Cmd/Ctrl+O)").clicked() {
                    ui.close_menu();
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "Video Files",
                            &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv"],
                        )
                        .pick_file()
                    {
                        self.open_media_file(path);
                    }
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Append Video Segment..."),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Video Files", &["mp4", "mkv", "mov", "avi", "webm"])
                        .pick_file()
                    {
                        self.append_media_file(path);
                    }
                }
                if ui.button("Generate & Load Sample Test Video").clicked() {
                    ui.close_menu();
                    self.generate_and_load_sample_video();
                }

                ui.separator();
                ui.menu_button("Recent Files", |ui| {
                    let recents = self.settings.recent_files.clone();
                    if recents.is_empty() {
                        ui.label("No recent files");
                    } else {
                        for path in recents {
                            let label = path.display().to_string();
                            if ui.button(label).clicked() {
                                ui.close_menu();
                                self.open_media_file(path);
                            }
                        }
                    }
                });

                ui.separator();
                if ui.button("Load Project (.cutdub.json)...").clicked() {
                    ui.close_menu();
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("CutDub Project", &["json"])
                        .pick_file()
                    {
                        match Project::load_from_file(&path) {
                            Ok(proj) => {
                                self.project = proj;
                                self.set_status(
                                    format!("Loaded project '{}'", path.display()),
                                    false,
                                );
                            }
                            Err(e) => self.set_status(format!("Project load error: {e}"), true),
                        }
                    }
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Save Project (.cutdub.json)..."),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Some(path) = rfd::FileDialog::new()
                        .set_file_name("project.cutdub.json")
                        .add_filter("CutDub Project", &["json"])
                        .save_file()
                    {
                        match self.project.save_to_file(&path) {
                            Ok(()) => self.set_status(
                                format!("Saved project to '{}'", path.display()),
                                false,
                            ),
                            Err(e) => self.set_status(format!("Project save error: {e}"), true),
                        }
                    }
                }

                ui.separator();
                if ui
                    .add_enabled(
                        self.project.is_loaded() && self.export_job.is_none(),
                        egui::Button::new("Export Video... (Cmd/Ctrl+E)"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.trigger_export_dialog();
                }

                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });

            // EDIT MENU
            ui.menu_button("Edit", |ui| {
                let undo_label = match self.project.undo_description() {
                    Some(d) => format!("Undo: {d} (Cmd/Ctrl+Z)"),
                    None => "Undo (Cmd/Ctrl+Z)".to_string(),
                };
                if ui
                    .add_enabled(self.project.can_undo(), egui::Button::new(undo_label))
                    .clicked()
                {
                    ui.close_menu();
                    self.project.undo();
                }

                let redo_label = match self.project.redo_description() {
                    Some(d) => format!("Redo: {d} (Cmd/Ctrl+Y)"),
                    None => "Redo (Cmd/Ctrl+Y)".to_string(),
                };
                if ui
                    .add_enabled(self.project.can_redo(), egui::Button::new(redo_label))
                    .clicked()
                {
                    ui.close_menu();
                    self.project.redo();
                }

                ui.separator();
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Set Selection Start [Mark In] ( [ / I )"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.project.set_mark_in();
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Set Selection End [Mark Out] ( ] / O )"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.project.set_mark_out();
                }
                if ui
                    .add_enabled(
                        self.project.selection_range().is_some(),
                        egui::Button::new("Clear Selection"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.project.clear_selection();
                }

                ui.separator();
                if ui
                    .add_enabled(
                        self.project.selection_range().is_some(),
                        egui::Button::new("Delete Selected Frames (Del / Backspace)"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Err(e) = self.project.delete_selection() {
                        self.set_status(e.to_string(), true);
                    }
                }
                if ui
                    .add_enabled(
                        self.project.selection_range().is_some(),
                        egui::Button::new("Trim Timeline to Selection (Cmd/Ctrl+T)"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Err(e) = self.project.trim_to_selection() {
                        self.set_status(e.to_string(), true);
                    }
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Reset Timeline to Full Source"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.project.reset_timeline();
                }
            });

            // VIEW MENU
            ui.menu_button("View", |ui| {
                ui.label(RichText::new("Preview Layout").strong());
                ui.radio_value(
                    &mut self.preview_layout,
                    PreviewLayoutMode::DualSideBySide,
                    "Dual Preview (Source + Output)",
                );
                ui.radio_value(
                    &mut self.preview_layout,
                    PreviewLayoutMode::SourceOnly,
                    "Source Panel Only",
                );
                ui.radio_value(
                    &mut self.preview_layout,
                    PreviewLayoutMode::OutputOnly,
                    "Output Panel Only",
                );

                ui.separator();
                ui.label(RichText::new("Preview Zoom").strong());
                ui.radio_value(
                    &mut self.preview_zoom,
                    PreviewZoom::FitToPanel,
                    "Fit to Panel",
                );
                ui.radio_value(&mut self.preview_zoom, PreviewZoom::Scale(0.5), "50%");
                ui.radio_value(&mut self.preview_zoom, PreviewZoom::Scale(1.0), "100% (1:1)");
                ui.radio_value(&mut self.preview_zoom, PreviewZoom::Scale(2.0), "200%");

                ui.separator();
                ui.checkbox(&mut self.show_right_sidebar, "Show Inspector Sidebar");
            });

            // VIDEO MENU
            ui.menu_button("Video", |ui| {
                ui.radio_value(
                    &mut self.export_settings.processing_mode,
                    VideoProcessingMode::DirectStreamCopy,
                    "Direct Stream Copy (Lossless Remux)",
                );
                ui.radio_value(
                    &mut self.export_settings.processing_mode,
                    VideoProcessingMode::SmartCopy,
                    "Smart Cut (Re-encode GOP Boundary + Stream Copy)",
                );
                ui.radio_value(
                    &mut self.export_settings.processing_mode,
                    VideoProcessingMode::FullProcessing,
                    "Full Processing Mode (Filters + Re-encode)",
                );
                ui.separator();
                if ui.button("Filters Pipeline... (Cmd/Ctrl+F)").clicked() {
                    ui.close_menu();
                    self.show_right_sidebar = true;
                    self.active_sidebar_tab = RightSidebarTab::Filters;
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Save Frame Snapshot (PNG/JPG)... (Cmd/Ctrl+Shift+S)"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    self.trigger_snapshot_dialog();
                }
                if ui.button("Encoding & Export Settings...").clicked() {
                    ui.close_menu();
                    self.show_right_sidebar = true;
                    self.active_sidebar_tab = RightSidebarTab::Export;
                }
            });

            // AUDIO MENU
            ui.menu_button("Audio", |ui| {
                if ui.button("Set All Audio Streams to Direct Stream Copy").clicked() {
                    for cfg in &mut self.project.audio_configs {
                        cfg.mode = AudioExportMode::DirectStreamCopy;
                    }
                    ui.close_menu();
                }
                if ui.button("Set All Audio Streams to AAC Re-encode (192 kbps)").clicked() {
                    for cfg in &mut self.project.audio_configs {
                        cfg.mode = AudioExportMode::ReEncode {
                            codec: "aac".into(),
                            bitrate_kbps: 192,
                        };
                    }
                    ui.close_menu();
                }
                if ui.button("Disable All Audio Streams").clicked() {
                    for cfg in &mut self.project.audio_configs {
                        cfg.mode = AudioExportMode::Disabled;
                    }
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Audio Tracks & Sync Delay...").clicked() {
                    ui.close_menu();
                    self.show_right_sidebar = true;
                    self.active_sidebar_tab = RightSidebarTab::Streams;
                }
            });

            // TOOLS MENU
            ui.menu_button("Tools", |ui| {
                if ui.button("Go to Frame / Timestamp... (Cmd/Ctrl+G)").clicked() {
                    ui.close_menu();
                    self.goto_frame_input = self.project.current_frame.to_string();
                    self.goto_time_input = self
                        .project
                        .resolve_frame(self.project.current_frame)
                        .map(|r| r.virtual_pts.format_hms_millis())
                        .unwrap_or_else(|| "00:00:00.000".into());
                    self.show_goto_dialog = true;
                }
                if ui.button("Batch Processing Queue...").clicked() {
                    ui.close_menu();
                    self.show_right_sidebar = true;
                    self.active_sidebar_tab = RightSidebarTab::Batch;
                }
                if ui.button("Scene Change & Black-Frame Detector...").clicked() {
                    ui.close_menu();
                    self.show_right_sidebar = true;
                    self.active_sidebar_tab = RightSidebarTab::Edl;
                }
                ui.separator();
                if ui
                    .add_enabled(
                        self.project.is_loaded(),
                        egui::Button::new("Save Automation Script (.cutdub-script.json)..."),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let Some(script_path) = rfd::FileDialog::new()
                        .set_file_name("workflow.cutdub-script.json")
                        .add_filter("CutDub Automation Script", &["json"])
                        .save_file()
                    {
                        let out_ext = self.export_settings.container.extension();
                        let default_out = script_path.with_extension(out_ext);
                        if let Some(script) = AutomationScript::from_current_session(
                            "Exported Session Script",
                            &self.project,
                            &self.filters,
                            &self.export_settings,
                            default_out,
                        ) {
                            match script.save_to_file(&script_path) {
                                Ok(()) => self.set_status(
                                    format!("Saved automation script '{}'", script_path.display()),
                                    false,
                                ),
                                Err(e) => {
                                    self.set_status(format!("Failed to save script: {e}"), true)
                                }
                            }
                        }
                    }
                }
                if ui.button("Run Automation Script (.cutdub-script.json)...").clicked() {
                    ui.close_menu();
                    if let Some(script_path) = rfd::FileDialog::new()
                        .add_filter("CutDub Automation Script", &["json"])
                        .pick_file()
                    {
                        if let Some(bins) = self.binaries.clone() {
                            match AutomationScript::load_from_file(&script_path) {
                                Ok(script) => match execute_automation_script(&bins, &script) {
                                    Ok(dur) => self.set_status(
                                        format!(
                                            "Executed script '{}' -> '{}' ({})",
                                            script.name,
                                            script.output_path.display(),
                                            dur
                                        ),
                                        false,
                                    ),
                                    Err(e) => self.set_status(
                                        format!("Script execution error: {e}"),
                                        true,
                                    ),
                                },
                                Err(e) => {
                                    self.set_status(format!("Invalid script file: {e}"), true)
                                }
                            }
                        }
                    }
                }
                ui.separator();
                if ui.button("FFmpeg Capabilities & Cache Telemetry...").clicked() {
                    ui.close_menu();
                    self.show_ffmpeg_dialog = true;
                }
                if ui
                    .add_enabled(
                        self.project.is_loaded() && !self.is_scanning_timing,
                        egui::Button::new("Scan Full Packet & Waveform Index (Background)"),
                    )
                    .clicked()
                {
                    ui.close_menu();
                    if let (Some(bins), Some(src)) =
                        (self.binaries.clone(), self.project.primary_source().cloned())
                    {
                        let (tx, rx) = mpsc::channel();
                        self.timing_scan_rx = Some(rx);
                        self.is_scanning_timing = true;
                        let path = src.path.clone();
                        let nominal_fps = src.timing.nominal_fps;
                        let time_base = src.timing.time_base;
                        let bins_t = bins.clone();
                        let path_t = path.clone();
                        thread::spawn(move || {
                            if let Ok(map) =
                                probe_packet_timing_map(&bins_t, &path_t, nominal_fps, time_base)
                            {
                                let _ = tx.send((0, map));
                            }
                        });
                        if let Some(first_audio) = src.audio_streams.first() {
                            let stream_idx = first_audio.stream_index;
                            let (w_tx, w_rx) = mpsc::channel();
                            self.waveform_rx = Some(w_rx);
                            thread::spawn(move || {
                                if let Ok(wf) =
                                    extract_audio_waveform(&bins, &path, stream_idx, 512)
                                {
                                    let _ = w_tx.send((0, wf));
                                }
                            });
                        }
                        self.set_status("Started background packet & waveform scan...", false);
                    }
                }
            });

            // HELP MENU
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard Shortcuts Reference...").clicked() {
                    ui.close_menu();
                    self.show_shortcuts_dialog = true;
                }
                if ui.button("About CutDub...").clicked() {
                    ui.close_menu();
                    self.show_about_dialog = true;
                }
            });
        });
    }

    fn render_toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;

            if ui.button("📂 Open").on_hover_text("Open video file (Cmd/Ctrl+O)").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(
                        "Video Files",
                        &["mp4", "mkv", "mov", "avi", "webm", "m4v", "ts", "flv"],
                    )
                    .pick_file()
                {
                    self.open_media_file(path);
                }
            }

            if ui
                .add_enabled(self.project.is_loaded(), egui::Button::new("➕ Append"))
                .on_hover_text("Append compatible video segment to timeline")
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Video Files", &["mp4", "mkv", "mov", "avi", "webm"])
                    .pick_file()
                {
                    self.append_media_file(path);
                }
            }

            if !self.project.is_loaded()
                && ui
                    .button("🎬 Demo Clip")
                    .on_hover_text("Generate and load a 5-second test video using FFmpeg")
                    .clicked()
            {
                self.generate_and_load_sample_video();
            }

            if ui
                .add_enabled(
                    self.project.is_loaded() && self.export_job.is_none(),
                    egui::Button::new(
                        RichText::new("💾 Export Video").color(Color32::from_rgb(130, 220, 150)),
                    ),
                )
                .on_hover_text("Export video / selection (Cmd/Ctrl+E)")
                .clicked()
            {
                self.trigger_export_dialog();
            }

            if ui
                .add_enabled(self.project.is_loaded(), egui::Button::new("📷 Snapshot"))
                .on_hover_text("Save full-resolution PNG/JPEG frame snapshot (Cmd/Ctrl+Shift+S)")
                .clicked()
            {
                self.trigger_snapshot_dialog();
            }

            ui.separator();

            // Processing Mode Quick Switcher
            ui.label(RichText::new("Mode:").weak());
            ui.selectable_value(
                &mut self.export_settings.processing_mode,
                VideoProcessingMode::DirectStreamCopy,
                "⚡ Stream Copy",
            )
            .on_hover_text("Lossless stream copy without re-encoding (keyframe-constrained)");
            ui.selectable_value(
                &mut self.export_settings.processing_mode,
                VideoProcessingMode::SmartCopy,
                "🧠 Smart Cut",
            )
            .on_hover_text("Frame-exact hybrid mode: re-encodes only non-keyframe cut boundaries and stream-copies the rest");
            ui.selectable_value(
                &mut self.export_settings.processing_mode,
                VideoProcessingMode::FullProcessing,
                "🎛 Full Processing",
            )
            .on_hover_text("Frame-exact cutting, filter pipeline, and re-encoding");

            ui.separator();

            if ui
                .add_enabled(self.project.can_undo(), egui::Button::new("↩ Undo"))
                .clicked()
            {
                self.project.undo();
            }
            if ui
                .add_enabled(self.project.can_redo(), egui::Button::new("↪ Redo"))
                .clicked()
            {
                self.project.redo();
            }

            ui.separator();

            ui.label(RichText::new("View:").weak());
            ui.selectable_value(
                &mut self.preview_layout,
                PreviewLayoutMode::DualSideBySide,
                "Dual",
            );
            ui.selectable_value(
                &mut self.preview_layout,
                PreviewLayoutMode::SourceOnly,
                "Source",
            );
            ui.selectable_value(
                &mut self.preview_layout,
                PreviewLayoutMode::OutputOnly,
                "Output",
            );

            egui::ComboBox::from_id_salt("zoom_combo")
                .selected_text(format!("Zoom: {}", self.preview_zoom.label()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.preview_zoom,
                        PreviewZoom::FitToPanel,
                        "Fit to Panel",
                    );
                    ui.selectable_value(&mut self.preview_zoom, PreviewZoom::Scale(0.5), "50%");
                    ui.selectable_value(&mut self.preview_zoom, PreviewZoom::Scale(1.0), "100%");
                    ui.selectable_value(&mut self.preview_zoom, PreviewZoom::Scale(2.0), "200%");
                });

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.toggle_value(&mut self.show_right_sidebar, "Sidebar ◨");
                let active_filters = self.filters.filters.iter().filter(|f| f.enabled).count();
                if active_filters > 0 {
                    ui.label(
                        RichText::new(format!("{active_filters} filter(s) active"))
                            .color(Color32::from_rgb(245, 190, 85)),
                    );
                }
            });
        });
    }

    fn render_preview_area(&mut self, ui: &mut Ui) {
        if self.binaries.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("FFmpeg is unavailable");
                ui.add_space(8.0);
                ui.label("CutDub needs FFmpeg and ffprobe to open, preview, and export videos.");
                if let Some(error) = &self.toolchain_error {
                    ui.colored_label(Color32::from_rgb(255, 110, 110), error);
                }
                ui.add_space(12.0);
                if ui.button("Show setup instructions...").clicked() {
                    self.show_ffmpeg_dialog = true;
                }
            });
            return;
        }
        let avail = ui.available_size();
        if !self.project.is_loaded() {
            ui.allocate_ui_with_layout(
                avail,
                Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(avail.y * 0.28);
                        ui.label(
                            RichText::new("CutDub — Precision Video Processor")
                                .size(24.0)
                                .strong()
                                .color(Color32::from_rgb(210, 220, 235)),
                        );
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(
                                "Drop a video file anywhere in this window, click 'Open', or click 'Demo Clip' to generate a sample video.",
                            )
                            .size(14.0)
                            .color(Color32::from_rgb(150, 160, 175)),
                        );
                        ui.add_space(16.0);
                        ui.horizontal(|ui| {
                            let w = ui.available_width();
                            ui.add_space((w - 320.0).max(0.0) * 0.5);
                            if ui
                                .button(RichText::new("📂 Open Video File...").size(15.0))
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter(
                                        "Video Files",
                                        &["mp4", "mkv", "mov", "avi", "webm", "m4v"],
                                    )
                                    .pick_file()
                                {
                                    self.open_media_file(path);
                                }
                            }
                            if ui
                                .button(RichText::new("🎬 Generate Sample Clip").size(15.0))
                                .clicked()
                            {
                                self.generate_and_load_sample_video();
                            }
                        });
                    });
                },
            );
            return;
        }

        let resolved = self.project.resolve_frame(self.project.current_frame);
        let src_dims = self
            .project
            .primary_source()
            .map(|s| (s.width, s.height))
            .unwrap_or((1920, 1080));
        let (_, out_w, out_h) = self.filters.to_ffmpeg_filtergraph(src_dims.0, src_dims.1);

        // Extract active crop filter (if any) so the Source preview can render interactive crop handles
        let mut active_crop: Option<(u32, u32, u32, u32)> = self
            .filters
            .filters
            .iter()
            .find(|f| f.enabled && matches!(f.kind, FilterKind::Crop { .. }))
            .and_then(|f| match f.kind {
                FilterKind::Crop {
                    left,
                    top,
                    right,
                    bottom,
                } => Some((left, top, right, bottom)),
                _ => None,
            });
        let had_crop = active_crop;

        match self.preview_layout {
            PreviewLayoutMode::DualSideBySide => {
                ui.columns(2, |cols| {
                    Self::draw_single_preview_pane(
                        &mut cols[0],
                        "SOURCE INPUT",
                        self.source_texture.as_ref(),
                        src_dims,
                        resolved,
                        self.preview_zoom,
                        Color32::from_rgb(70, 145, 235),
                        &mut active_crop,
                    );
                    let output_title = match self.export_settings.processing_mode {
                        VideoProcessingMode::DirectStreamCopy => "OUTPUT (DIRECT STREAM COPY)",
                        VideoProcessingMode::SmartCopy => "OUTPUT (SMART CUT / GOP HYBRID)",
                        VideoProcessingMode::FullProcessing => "OUTPUT (FILTERED / RE-ENCODE)",
                    };
                    let tex = if self.export_settings.processing_mode
                        != VideoProcessingMode::FullProcessing
                        && self.filters.is_empty()
                    {
                        self.source_texture.as_ref()
                    } else {
                        self.output_texture.as_ref().or(self.source_texture.as_ref())
                    };
                    let mut no_crop = None;
                    Self::draw_single_preview_pane(
                        &mut cols[1],
                        output_title,
                        tex,
                        (out_w, out_h),
                        resolved,
                        self.preview_zoom,
                        Color32::from_rgb(80, 200, 130),
                        &mut no_crop,
                    );
                });
            }
            PreviewLayoutMode::SourceOnly => {
                Self::draw_single_preview_pane(
                    ui,
                    "SOURCE INPUT",
                    self.source_texture.as_ref(),
                    src_dims,
                    resolved,
                    self.preview_zoom,
                    Color32::from_rgb(70, 145, 235),
                    &mut active_crop,
                );
            }
            PreviewLayoutMode::OutputOnly => {
                let mut no_crop = None;
                Self::draw_single_preview_pane(
                    ui,
                    "OUTPUT PREVIEW",
                    self.output_texture.as_ref().or(self.source_texture.as_ref()),
                    (out_w, out_h),
                    resolved,
                    self.preview_zoom,
                    Color32::from_rgb(80, 200, 130),
                    &mut no_crop,
                );
            }
        }

        // Write back any interactive crop changes made by dragging handles on the Source canvas
        if active_crop != had_crop {
            if let Some((nl, nt, nr, nb)) = active_crop {
                if let Some(inst) = self
                    .filters
                    .filters
                    .iter_mut()
                    .find(|f| f.enabled && matches!(f.kind, FilterKind::Crop { .. }))
                {
                    inst.kind = FilterKind::Crop {
                        left: nl,
                        top: nt,
                        right: nr,
                        bottom: nb,
                    };
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_single_preview_pane(
        ui: &mut Ui,
        title: &str,
        texture: Option<&TextureHandle>,
        dimensions: (u32, u32),
        resolved: Option<ResolvedFrame>,
        zoom: PreviewZoom,
        badge_color: Color32,
        crop_overlay: &mut Option<(u32, u32, u32, u32)>,
    ) {
        let frame = egui::Frame::canvas(ui.style())
            .fill(Color32::from_rgb(16, 18, 22))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(44, 48, 58)))
            .inner_margin(Margin::same(6));

        frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(11.5).color(badge_color));
                if crop_overlay.is_some() {
                    ui.label(
                        RichText::new("[Drag edges to crop]")
                            .size(10.5)
                            .color(Color32::from_rgb(245, 195, 75)),
                    );
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(r) = resolved {
                        let kf_badge = if r.is_keyframe { "[K]" } else { "[I/P]" };
                        ui.label(
                            RichText::new(format!(
                                "{}x{}  |  Src #{} {}  |  {}",
                                dimensions.0, dimensions.1, r.source_frame, kf_badge, r.virtual_pts
                            ))
                            .monospace()
                            .size(11.0)
                            .color(Color32::from_rgb(165, 175, 190)),
                        );
                    }
                });
            });

            let canvas_size = ui.available_size();
            let sense = if crop_overlay.is_some() {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            };
            let (rect, resp) = ui.allocate_exact_size(canvas_size, sense);
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 2.0, Color32::from_rgb(11, 12, 15));

            if let Some(tex) = texture {
                let (w, h) = (dimensions.0.max(1) as f32, dimensions.1.max(1) as f32);
                let target_size = match zoom {
                    PreviewZoom::FitToPanel => {
                        let scale = (rect.width() / w).min(rect.height() / h).max(0.05);
                        Vec2::new(w * scale, h * scale)
                    }
                    PreviewZoom::Scale(s) => Vec2::new(w * s, h * s),
                };
                let img_rect = Rect::from_center_size(rect.center(), target_size);
                painter.image(
                    tex.id(),
                    img_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                painter.rect_stroke(
                    img_rect,
                    0.0,
                    Stroke::new(1.0_f32, Color32::from_rgb(55, 62, 74)),
                    StrokeKind::Outside,
                );

                // Interactive On-Canvas Crop Overlay & Handles
                if let Some((left, top, right, bottom)) = crop_overlay {
                    if resp.dragged() {
                        if let Some(ptr) = resp.interact_pointer_pos() {
                            let rel_x = ((ptr.x - img_rect.left()) / img_rect.width().max(1.0))
                                .clamp(0.0, 1.0);
                            let rel_y = ((ptr.y - img_rect.top()) / img_rect.height().max(1.0))
                                .clamp(0.0, 1.0);
                            let px_x = (rel_x * w).round() as u32;
                            let px_y = (rel_y * h).round() as u32;

                            // Adjust whichever edge is closest to pointer
                            let dist_l = (px_x as i64 - *left as i64).abs();
                            let dist_r = ((dimensions.0.saturating_sub(*right)) as i64 - px_x as i64).abs();
                            let dist_t = (px_y as i64 - *top as i64).abs();
                            let dist_b = ((dimensions.1.saturating_sub(*bottom)) as i64 - px_y as i64).abs();
                            let min_d = dist_l.min(dist_r).min(dist_t).min(dist_b);

                            if min_d == dist_l {
                                *left = px_x.min(dimensions.0 / 2) & !1;
                            } else if min_d == dist_r {
                                *right = dimensions.0.saturating_sub(px_x).min(dimensions.0 / 2) & !1;
                            } else if min_d == dist_t {
                                *top = px_y.min(dimensions.1 / 2) & !1;
                            } else {
                                *bottom = dimensions.1.saturating_sub(px_y).min(dimensions.1 / 2) & !1;
                            }
                        }
                    }

                    let c_left = img_rect.left() + (*left as f32 / w) * img_rect.width();
                    let c_right = img_rect.right() - (*right as f32 / w) * img_rect.width();
                    let c_top = img_rect.top() + (*top as f32 / h) * img_rect.height();
                    let c_bot = img_rect.bottom() - (*bottom as f32 / h) * img_rect.height();
                    let crop_rect = Rect::from_min_max(
                        Pos2::new(c_left, c_top),
                        Pos2::new(c_right.max(c_left + 4.0), c_bot.max(c_top + 4.0)),
                    );

                    // Dim cropped-out outer margins
                    let matte_col = Color32::from_rgba_unmultiplied(0, 0, 0, 140);
                    if crop_rect.top() > img_rect.top() {
                        painter.rect_filled(
                            Rect::from_min_max(
                                img_rect.left_top(),
                                Pos2::new(img_rect.right(), crop_rect.top()),
                            ),
                            0.0,
                            matte_col,
                        );
                    }
                    if crop_rect.bottom() < img_rect.bottom() {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(img_rect.left(), crop_rect.bottom()),
                                img_rect.right_bottom(),
                            ),
                            0.0,
                            matte_col,
                        );
                    }
                    if crop_rect.left() > img_rect.left() {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(img_rect.left(), crop_rect.top()),
                                Pos2::new(crop_rect.left(), crop_rect.bottom()),
                            ),
                            0.0,
                            matte_col,
                        );
                    }
                    if crop_rect.right() < img_rect.right() {
                        painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(crop_rect.right(), crop_rect.top()),
                                Pos2::new(img_rect.right(), crop_rect.bottom()),
                            ),
                            0.0,
                            matte_col,
                        );
                    }

                    // Rule-of-Thirds compositional guide inside crop_rect
                    let third_stroke =
                        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 220, 110, 90));
                    for i in 1..=2 {
                        let tx = crop_rect.left() + crop_rect.width() * (i as f32 / 3.0);
                        let ty = crop_rect.top() + crop_rect.height() * (i as f32 / 3.0);
                        painter.line_segment(
                            [Pos2::new(tx, crop_rect.top()), Pos2::new(tx, crop_rect.bottom())],
                            third_stroke,
                        );
                        painter.line_segment(
                            [Pos2::new(crop_rect.left(), ty), Pos2::new(crop_rect.right(), ty)],
                            third_stroke,
                        );
                    }

                    painter.rect_stroke(
                        crop_rect,
                        0.0,
                        Stroke::new(2.0_f32, Color32::from_rgb(250, 200, 65)),
                        StrokeKind::Inside,
                    );
                    // Corner and midpoint handles
                    for pt in [
                        crop_rect.left_top(),
                        crop_rect.right_top(),
                        crop_rect.left_bottom(),
                        crop_rect.right_bottom(),
                        Pos2::new(crop_rect.center().x, crop_rect.top()),
                        Pos2::new(crop_rect.center().x, crop_rect.bottom()),
                        Pos2::new(crop_rect.left(), crop_rect.center().y),
                        Pos2::new(crop_rect.right(), crop_rect.center().y),
                    ] {
                        painter.rect_filled(
                            Rect::from_center_size(pt, Vec2::splat(7.0)),
                            1.0,
                            Color32::from_rgb(255, 215, 80),
                        );
                    }
                }
            } else {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Decoding frame...",
                    FontId::proportional(14.0),
                    Color32::from_rgb(130, 140, 155),
                );
            }
        });
    }

    fn render_timeline_and_transport(&mut self, ui: &mut Ui) {
        let total_frames = self.project.total_frames();

        // Compute visible frame window [vis_start, vis_end) from timeline_zoom and timeline_pan_center
        self.timeline_zoom = self.timeline_zoom.clamp(1.0, 64.0);
        let (vis_start, vis_end) = if total_frames == 0 || self.timeline_zoom <= 1.001 {
            self.timeline_pan_center = 0.5;
            (0u64, total_frames.max(1))
        } else {
            let win_len = ((total_frames as f64 / self.timeline_zoom as f64).round() as u64)
                .clamp(2, total_frames);
            let half_frac = (win_len as f32 / total_frames as f32) * 0.5;
            self.timeline_pan_center = self
                .timeline_pan_center
                .clamp(half_frac, (1.0 - half_frac).max(half_frac));
            let center_f = (self.timeline_pan_center as f64 * total_frames as f64).round() as u64;
            let s = center_f
                .saturating_sub(win_len / 2)
                .min(total_frames.saturating_sub(win_len));
            let e = (s + win_len).min(total_frames);
            (s, e)
        };
        let vis_span = (vis_end.saturating_sub(vis_start)).max(1);
        let vis_max_idx = vis_span.saturating_sub(1).max(1) as f32;

        // 1. Custom VirtualDub2-style Timeline Scrubber with Keyframe, Selection & Bookmark Markers
        let timeline_height = 42.0;
        let avail_w = ui.available_width();
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(avail_w, timeline_height), Sense::click_and_drag());

        // Support Ctrl/Cmd + Mouse Wheel over timeline to zoom in/out
        if response.hovered() && total_frames > 1 {
            let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll_y.abs() > 0.5 {
                let factor = if scroll_y > 0.0 { 1.25 } else { 0.8 };
                self.timeline_zoom = (self.timeline_zoom * factor).clamp(1.0, 64.0);
                self.timeline_pan_center =
                    (self.project.current_frame as f32 / total_frames.max(1) as f32).clamp(0.0, 1.0);
            }
        }

        if (response.clicked() || response.dragged()) && total_frames > 0 {
            if let Some(pos) = response.interact_pointer_pos() {
                let frac = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
                let target_frame = (vis_start
                    + (frac as f64 * (vis_span.saturating_sub(1)) as f64).round() as u64)
                    .min(total_frames.saturating_sub(1));
                self.project.current_frame = target_frame;
                if let Some(r) = self.project.resolve_frame(target_frame) {
                    self.playback.reanchor(r.virtual_pts);
                }
            }
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(20, 23, 29));
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0_f32, Color32::from_rgb(52, 58, 70)),
            StrokeKind::Inside,
        );

        if total_frames > 0 {
            let frame_to_x = |f: u64| -> f32 {
                let rel = (f as f64 - vis_start as f64) / (vis_max_idx as f64);
                rect.left() + (rel.clamp(0.0, 1.0) as f32) * rect.width()
            };

            // Render resampled Audio Waveform envelope inside the visible timeline window
            if self.show_waveform {
                let cols = (rect.width() as usize / 2).clamp(32, 600);
                let peaks = self
                    .project
                    .sample_timeline_waveform_range(vis_start, vis_end, cols);
                if !peaks.is_empty() {
                    let mid_y = rect.center().y;
                    let half_h = (rect.height() * 0.44).max(4.0);
                    for (col_idx, peak) in peaks.iter().enumerate() {
                        let x = rect.left()
                            + (col_idx as f32 / (cols.saturating_sub(1).max(1) as f32))
                                * rect.width();
                        let y_top = mid_y - peak.max * half_h;
                        let y_bot = mid_y - peak.min * half_h;
                        painter.line_segment(
                            [Pos2::new(x, y_top), Pos2::new(x, y_bot)],
                            Stroke::new(
                                1.5_f32,
                                Color32::from_rgba_unmultiplied(70, 195, 145, 110),
                            ),
                        );
                        let rms_h = peak.rms * half_h;
                        painter.line_segment(
                            [Pos2::new(x, mid_y - rms_h), Pos2::new(x, mid_y + rms_h)],
                            Stroke::new(
                                1.5_f32,
                                Color32::from_rgba_unmultiplied(110, 235, 175, 175),
                            ),
                        );
                    }
                }
            }

            // Highlight Selection Range [sel_start, sel_end)
            if let Some((sel_start, sel_end)) = self.project.selection_range() {
                if sel_end > vis_start && sel_start < vis_end {
                    let x0 = frame_to_x(sel_start.max(vis_start));
                    let x1 = frame_to_x(sel_end.saturating_sub(1).min(vis_end.saturating_sub(1)));
                    let sel_rect = Rect::from_min_max(
                        Pos2::new(x0, rect.top() + 2.0),
                        Pos2::new(x1.max(x0 + 3.0), rect.bottom() - 2.0),
                    );
                    painter.rect_filled(
                        sel_rect,
                        2.0,
                        Color32::from_rgba_unmultiplied(65, 150, 240, 75),
                    );
                    if sel_start >= vis_start {
                        painter.line_segment(
                            [
                                Pos2::new(x0, rect.top() + 1.0),
                                Pos2::new(x0, rect.bottom() - 1.0),
                            ],
                            Stroke::new(2.0_f32, Color32::from_rgb(90, 185, 255)),
                        );
                    }
                    if sel_end.saturating_sub(1) < vis_end {
                        painter.line_segment(
                            [
                                Pos2::new(x1, rect.top() + 1.0),
                                Pos2::new(x1, rect.bottom() - 1.0),
                            ],
                            Stroke::new(2.0_f32, Color32::from_rgb(90, 185, 255)),
                        );
                    }
                }
            }

            // Draw Keyframe tick marks along bottom edge within [vis_start, vis_end)
            let max_ticks = (rect.width() / 3.0) as usize;
            let step = (vis_span as usize / max_ticks.max(1)).max(1) as u64;
            let mut f = vis_start;
            while f < vis_end {
                let bucket_end = (f + step).min(vis_end);
                if let Some(kf_frame) = self.project.first_keyframe_in_range(f, bucket_end) {
                    let x = frame_to_x(kf_frame);
                    painter.line_segment(
                        [
                            Pos2::new(x, rect.bottom() - 10.0),
                            Pos2::new(x, rect.bottom() - 2.0),
                        ],
                        Stroke::new(1.2_f32, Color32::from_rgb(235, 175, 65)),
                    );
                }
                f += step;
            }

            // Draw Segment boundary markers
            let mut accum = 0u64;
            for seg in self
                .project
                .segments
                .iter()
                .take(self.project.segments.len().saturating_sub(1))
            {
                accum += seg.frame_count();
                if accum >= vis_start && accum < vis_end {
                    let x = frame_to_x(accum);
                    painter.line_segment(
                        [Pos2::new(x, rect.top() + 2.0), Pos2::new(x, rect.bottom() - 2.0)],
                        Stroke::new(1.5_f32, Color32::from_rgb(230, 95, 95)),
                    );
                }
            }

            // Draw detected Scene Cut & Black-Frame markers along the top edge of the timeline
            for marker in &self.scene_markers {
                let f_idx = self.project.virtual_timestamp_to_frame(marker.timestamp);
                if f_idx >= vis_start && f_idx < vis_end {
                    let x = frame_to_x(f_idx);
                    let color = match marker.kind {
                        SceneMarkerKind::SceneCut { .. } => Color32::from_rgb(235, 95, 225),
                        SceneMarkerKind::BlackSegment { .. } => Color32::from_rgb(75, 215, 235),
                    };
                    painter.line_segment(
                        [Pos2::new(x, rect.top() + 2.0), Pos2::new(x, rect.top() + 13.0)],
                        Stroke::new(2.0_f32, color),
                    );
                }
            }

            // Draw User Timeline Markers / Bookmarks (`self.project.markers`)
            for bm in &self.project.markers {
                if bm.frame >= vis_start && bm.frame < vis_end {
                    let x = frame_to_x(bm.frame);
                    let m_col = Color32::from_rgb(80, 235, 195);
                    painter.line_segment(
                        [Pos2::new(x, rect.top() + 2.0), Pos2::new(x, rect.bottom() - 2.0)],
                        Stroke::new(1.8_f32, m_col),
                    );
                    painter.rect_filled(
                        Rect::from_center_size(Pos2::new(x, rect.top() + 6.0), Vec2::new(7.0, 7.0)),
                        1.5,
                        m_col,
                    );
                }
            }

            // Draw current frame playhead
            if self.project.current_frame >= vis_start && self.project.current_frame < vis_end {
                let playhead_x = frame_to_x(self.project.current_frame);
                painter.line_segment(
                    [
                        Pos2::new(playhead_x, rect.top()),
                        Pos2::new(playhead_x, rect.bottom()),
                    ],
                    Stroke::new(2.2_f32, Color32::from_rgb(255, 95, 75)),
                );
                painter.circle_filled(
                    Pos2::new(playhead_x, rect.top() + 7.0),
                    5.0,
                    Color32::from_rgb(255, 95, 75),
                );
            }
        }

        // Optional Horizontal Pan Bar when zoomed in (`timeline_zoom > 1.01`)
        if self.timeline_zoom > 1.01 && total_frames > 1 {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "🔍 {:.1}× (Frames {}..{})",
                        self.timeline_zoom,
                        vis_start,
                        vis_end.saturating_sub(1)
                    ))
                    .monospace()
                    .size(11.0)
                    .color(Color32::from_rgb(135, 200, 250)),
                );
                ui.add(
                    egui::Slider::new(&mut self.timeline_pan_center, 0.0..=1.0)
                        .show_value(false)
                        .text("Pan"),
                );
            });
        }

        ui.add_space(4.0);

        // 2. VirtualDub2 Transport, Markers, Zoom & Selection Controls Bar
        ui.horizontal_wrapped(|ui| {
            let enabled = self.project.is_loaded();
            ui.spacing_mut().item_spacing.x = 5.0;

            if ui
                .add_enabled(enabled, egui::Button::new("|< Start"))
                .on_hover_text("Jump to start frame (Home)")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_start();
            }
            if ui
                .add_enabled(enabled, egui::Button::new("<< Key"))
                .on_hover_text("Jump to previous keyframe (Shift+Left / PageUp)")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_prev_keyframe();
            }
            if ui
                .add_enabled(enabled, egui::Button::new("< Frame"))
                .on_hover_text("Step backward 1 frame (Left Arrow)")
                .clicked()
            {
                self.playback.pause();
                self.project.step_backward(1);
            }

            let play_label = if self.playback.is_playing {
                "⏸ Pause"
            } else {
                "▶ Play"
            };
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(RichText::new(play_label).strong()),
                )
                .on_hover_text("Play / Pause (Space, or J/K/L shuttle keys)")
                .clicked()
            {
                let cur_pts = self
                    .project
                    .resolve_frame(self.project.current_frame)
                    .map(|r| r.virtual_pts)
                    .unwrap_or(TimestampUs::ZERO);
                self.playback.toggle(cur_pts);
            }

            let cur_pts = self
                .project
                .resolve_frame(self.project.current_frame)
                .map(|r| r.virtual_pts)
                .unwrap_or(TimestampUs::ZERO);
            let mut selected_speed = self.playback.speed;
            egui::ComboBox::from_id_salt("playback_speed_combo")
                .width(64.0)
                .selected_text(
                    format!("{:.2}×", self.playback.speed)
                        .replace(".00×", "×")
                        .replace(".50×", ".5×"),
                )
                .show_ui(ui, |ui| {
                    for spd in PlaybackClock::SPEED_PRESETS {
                        let lbl = format!("{spd:.2}×")
                            .replace(".00×", ".0×")
                            .replace(".50×", ".5×");
                        ui.selectable_value(&mut selected_speed, spd, lbl);
                    }
                })
                .response
                .on_hover_text(
                    "Preview Playback Speed (Shortcut: J = slower, K = pause/1x, L = faster)",
                );
            if (selected_speed - self.playback.speed).abs() > 0.001 {
                self.playback.set_speed(selected_speed, cur_pts);
            }

            if ui
                .add_enabled(enabled, egui::Button::new("Frame >"))
                .on_hover_text("Step forward 1 frame (Right Arrow)")
                .clicked()
            {
                self.playback.pause();
                self.project.step_forward(1);
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Key >>"))
                .on_hover_text("Jump to next keyframe (Shift+Right / PageDown)")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_next_keyframe();
            }
            if ui
                .add_enabled(enabled, egui::Button::new("End >|"))
                .on_hover_text("Jump to last frame (End)")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_end();
            }

            ui.separator();

            if ui
                .add_enabled(enabled, egui::Button::new("[ Mark In"))
                .on_hover_text("Set selection start at current frame ( [ or I )")
                .clicked()
            {
                self.project.set_mark_in();
            }
            if ui
                .add_enabled(enabled, egui::Button::new("Mark Out ]"))
                .on_hover_text("Set selection end at current frame ( ] or O )")
                .clicked()
            {
                self.project.set_mark_out();
            }

            let has_sel = self.project.selection_range().is_some();
            if ui
                .add_enabled(has_sel, egui::Button::new("✂ Delete Sel"))
                .on_hover_text("Delete selected range from timeline (Delete / Backspace)")
                .clicked()
            {
                if let Err(e) = self.project.delete_selection() {
                    self.set_status(e.to_string(), true);
                }
            }
            if ui
                .add_enabled(has_sel, egui::Button::new("Crop Trim to Sel"))
                .on_hover_text("Keep only the selected range (Cmd/Ctrl+T)")
                .clicked()
            {
                if let Err(e) = self.project.trim_to_selection() {
                    self.set_status(e.to_string(), true);
                }
            }

            ui.separator();

            // Timeline Bookmarks / Markers (`M`)
            if ui
                .add_enabled(enabled && !self.project.markers.is_empty(), egui::Button::new("< M"))
                .on_hover_text("Jump to previous Timeline Marker")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_prev_marker();
            }
            if ui
                .add_enabled(enabled, egui::Button::new("🔖 Mark (M)"))
                .on_hover_text("Toggle Timeline Marker / Bookmark at current frame (Shortcut: M)")
                .clicked()
            {
                self.project.toggle_marker_at_current();
            }
            if ui
                .add_enabled(enabled && !self.project.markers.is_empty(), egui::Button::new("M >"))
                .on_hover_text("Jump to next Timeline Marker")
                .clicked()
            {
                self.playback.pause();
                self.project.seek_next_marker();
            }

            ui.separator();

            // Timeline Zoom controls
            if ui
                .add_enabled(enabled && self.timeline_zoom > 1.01, egui::Button::new("1×"))
                .on_hover_text("Reset timeline zoom to full duration")
                .clicked()
            {
                self.timeline_zoom = 1.0;
                self.timeline_pan_center = 0.5;
            }
            if ui
                .add_enabled(enabled && self.timeline_zoom > 1.01, egui::Button::new("🔍-"))
                .on_hover_text("Zoom out timeline")
                .clicked()
            {
                self.timeline_zoom = (self.timeline_zoom * 0.5).max(1.0);
            }
            if ui
                .add_enabled(enabled && self.timeline_zoom < 64.0, egui::Button::new("🔍+"))
                .on_hover_text("Zoom in timeline around playhead")
                .clicked()
            {
                self.timeline_zoom = (self.timeline_zoom * 2.0).min(64.0);
                if total_frames > 0 {
                    self.timeline_pan_center =
                        (self.project.current_frame as f32 / total_frames as f32).clamp(0.0, 1.0);
                }
            }
            if ui
                .add_enabled(has_sel, egui::Button::new("🔍 [Sel]"))
                .on_hover_text("Zoom timeline to fit active selection")
                .clicked()
            {
                if let Some((s, e)) = self.project.selection_range() {
                    let sel_len = (e.saturating_sub(s)).max(2) as f32;
                    let tot = total_frames.max(2) as f32;
                    self.timeline_zoom = (tot / sel_len).clamp(1.0, 64.0);
                    self.timeline_pan_center = (((s + e) as f32 * 0.5) / tot).clamp(0.0, 1.0);
                }
            }

            ui.separator();
            ui.checkbox(&mut self.playback.loop_selection, "Loop Sel");
            ui.checkbox(&mut self.show_waveform, "Waveform");

            ui.separator();
            let mute_label = if self.audio_player.muted {
                "🔇 Muted"
            } else {
                "🔊 Audio"
            };
            if ui
                .selectable_label(!self.audio_player.muted, mute_label)
                .on_hover_text("Toggle live speaker audio during preview playback")
                .clicked()
            {
                self.audio_player.muted = !self.audio_player.muted;
            }
            ui.add(
                egui::Slider::new(&mut self.audio_player.volume, 0.0..=1.5)
                    .show_value(false)
                    .trailing_fill(true),
            )
            .on_hover_text(format!(
                "Preview Audio Volume: {:.0}%",
                self.audio_player.volume * 100.0
            ));

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(enabled, egui::Button::new("Go To..."))
                    .on_hover_text("Jump to exact frame number or timestamp (Cmd/Ctrl+G)")
                    .clicked()
                {
                    self.goto_frame_input = self.project.current_frame.to_string();
                    self.goto_time_input = self
                        .project
                        .resolve_frame(self.project.current_frame)
                        .map(|r| r.virtual_pts.format_hms_millis())
                        .unwrap_or_else(|| "00:00:00.000".into());
                    self.show_goto_dialog = true;
                }
            });
        });
    }

    fn render_status_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;

            if let Some(resolved) = self.project.resolve_frame(self.project.current_frame) {
                let total_f = self.project.total_frames();
                let total_dur = self.project.total_duration();
                let ftype = if resolved.is_keyframe {
                    RichText::new("[K - Keyframe]")
                        .color(Color32::from_rgb(240, 185, 75))
                        .monospace()
                } else {
                    RichText::new("[Inter-frame]")
                        .color(Color32::from_rgb(145, 155, 170))
                        .monospace()
                };

                ui.label(
                    RichText::new(format!(
                        "Frame: {} / {}",
                        resolved.virtual_frame,
                        total_f.saturating_sub(1)
                    ))
                    .monospace()
                    .strong(),
                );
                ui.label(ftype);
                ui.separator();
                ui.label(
                    RichText::new(format!("Time: {} / {}", resolved.virtual_pts, total_dur))
                        .monospace(),
                );

                if let Some(src) = self.project.source_by_id(resolved.source_id) {
                    ui.separator();
                    let rate_mode = if src.timing.is_vfr { "VFR" } else { "CFR" };
                    ui.label(
                        RichText::new(format!(
                            "{}x{}  {} ({})  {:.3} fps ({})",
                            src.width,
                            src.height,
                            src.video_codec.to_uppercase(),
                            src.pixel_format,
                            src.timing.nominal_fps.as_f64(),
                            rate_mode
                        ))
                        .monospace(),
                    );
                }

                if let Some((sel_s, sel_e)) = self.project.selection_range() {
                    ui.separator();
                    let count = sel_e - sel_s;
                    let s_pts = self
                        .project
                        .resolve_frame(sel_s)
                        .map(|r| r.virtual_pts)
                        .unwrap_or(TimestampUs::ZERO);
                    let e_pts = self
                        .project
                        .resolve_frame(sel_e.saturating_sub(1))
                        .map(|r| TimestampUs(r.virtual_pts.0 + r.duration_us))
                        .unwrap_or(TimestampUs::ZERO);
                    let sel_dur = TimestampUs((e_pts.0 - s_pts.0).max(0));
                    ui.label(
                        RichText::new(format!(
                            "Sel: {}..{} ({}f, {})",
                            sel_s,
                            sel_e - 1,
                            count,
                            sel_dur
                        ))
                        .color(Color32::from_rgb(100, 195, 255))
                        .monospace(),
                    );
                }
            } else {
                ui.label(RichText::new("Ready — No media loaded").weak());
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some((msg, is_err)) = &self.status_banner {
                    let color = if *is_err {
                        Color32::from_rgb(255, 110, 110)
                    } else {
                        Color32::from_rgb(140, 210, 160)
                    };
                    ui.label(RichText::new(msg).color(color).size(11.5));
                }
            });
        });
    }

    fn render_right_sidebar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut self.active_sidebar_tab,
                RightSidebarTab::Streams,
                "Streams & Audio",
            );
            ui.selectable_value(
                &mut self.active_sidebar_tab,
                RightSidebarTab::Filters,
                "Filters",
            );
            ui.selectable_value(
                &mut self.active_sidebar_tab,
                RightSidebarTab::Export,
                "Export",
            );
            ui.selectable_value(
                &mut self.active_sidebar_tab,
                RightSidebarTab::Batch,
                "Batch",
            );
            ui.selectable_value(
                &mut self.active_sidebar_tab,
                RightSidebarTab::Edl,
                "EDL",
            );
        });
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| match self.active_sidebar_tab {
            RightSidebarTab::Streams => self.render_streams_panel(ui),
            RightSidebarTab::Filters => self.render_filters_panel(ui),
            RightSidebarTab::Export => self.render_export_panel(ui),
            RightSidebarTab::Batch => self.render_batch_panel(ui),
            RightSidebarTab::Edl => self.render_edl_panel(ui),
        });
    }

    fn render_streams_panel(&mut self, ui: &mut Ui) {
        let Some(primary) = self.project.primary_source().cloned() else {
            ui.label("Open a video file to inspect container, video, audio, and subtitle streams.");
            return;
        };

        ui.heading("Container & Video Stream");
        egui::Grid::new("video_meta_grid")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.label("File:");
                ui.label(
                    primary
                        .path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(""),
                );
                ui.end_row();

                ui.label("Container:");
                ui.label(&primary.container_format);
                ui.end_row();

                ui.label("File Size:");
                ui.label(format!(
                    "{:.2} MB",
                    primary.file_size_bytes as f64 / (1024.0 * 1024.0)
                ));
                ui.end_row();

                ui.label("Video Codec:");
                ui.label(format!(
                    "{} (stream #{})",
                    primary.video_codec, primary.video_stream_index
                ));
                ui.end_row();

                ui.label("Resolution:");
                ui.label(format!(
                    "{} x {} ({})",
                    primary.width, primary.height, primary.pixel_format
                ));
                ui.end_row();

                ui.label("Frame Rate:");
                ui.label(format!(
                    "{} ({})",
                    primary.timing.nominal_fps,
                    if primary.timing.is_vfr { "VFR" } else { "CFR" }
                ));
                ui.end_row();

                ui.label("Time Base:");
                ui.label(primary.timing.time_base.to_string());
                ui.end_row();

                ui.label("Total Frames:");
                ui.label(format!(
                    "{} ({} keyframes)",
                    primary.timing.total_frames(),
                    primary.timing.keyframe_count()
                ));
                ui.end_row();
            });

        ui.add_space(12.0);
        ui.heading("Audio Tracks & Encoding");
        self.render_audio_encoding_section(ui, "streams_tab");

        ui.add_space(12.0);
        ui.heading("Subtitle Tracks");
        if self.project.subtitle_configs.is_empty() {
            ui.label(RichText::new("No subtitle streams present in source.").weak());
        } else {
            for sub in &mut self.project.subtitle_configs {
                ui.checkbox(
                    &mut sub.enabled,
                    format!(
                        "Stream #{} — {} ({})",
                        sub.info.stream_index,
                        sub.info.codec_name,
                        sub.info.language.as_deref().unwrap_or("und")
                    ),
                );
            }
        }
    }

    fn render_filters_panel(&mut self, ui: &mut Ui) {
        ui.heading("Video Filter Pipeline");
        ui.label(
            RichText::new(
                "Filters execute in top-to-bottom order. Output preview updates in real time.",
            )
            .size(12.0)
            .weak(),
        );

        let (src_w, src_h) = self
            .project
            .primary_source()
            .map(|s| (s.width, s.height))
            .unwrap_or((1920, 1080));

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.menu_button("➕ Add Video Filter...", |ui| {
                if ui.button("Crop (Margins / Aspect Presets)").clicked() {
                    self.filters.add(FilterKind::Crop {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui
                    .button("Resize / Scale (Dimensions / % / Aspect Lock)")
                    .clicked()
                {
                    self.filters.add(FilterKind::new_resize(
                        src_w,
                        src_h,
                        ScalingAlgorithm::Lanczos,
                    ));
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui
                    .button("Rotate (90° / 180° / 270° / Fine Angle)")
                    .clicked()
                {
                    self.filters.add(FilterKind::Rotate {
                        angle: RotationAngle::Rotate90Cw,
                        custom_degrees: 0.0,
                        bilinear: true,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Flip / Mirror").clicked() {
                    self.filters.add(FilterKind::Flip {
                        horizontal: true,
                        vertical: false,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Brightness, Contrast & Gamma").clicked() {
                    self.filters.add(FilterKind::BrightnessContrast {
                        brightness: 0.0,
                        contrast: 1.0,
                        gamma: 1.0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui
                    .button("Colour & White Balance (Sat / Temp / RGB)")
                    .clicked()
                {
                    self.filters.add(FilterKind::ColorAdjust {
                        saturation: 1.0,
                        gamma: 1.0,
                        hue_degrees: 0.0,
                        temperature: 0.0,
                        r_offset: 0.0,
                        g_offset: 0.0,
                        b_offset: 0.0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Deinterlace (BWDIF / YADIF)").clicked() {
                    self.filters.add(FilterKind::Deinterlace {
                        mode: DeinterlaceMode::SendFrame,
                        engine: DeinterlaceEngine::Bwdif,
                        parity: FieldParity::Auto,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Sharpen (Unsharp Mask / CAS)").clicked() {
                    self.filters.add(FilterKind::Sharpen {
                        amount: 1.0,
                        mode: SharpenMode::UnsharpMask,
                        radius: 5,
                        chroma_amount: 0.0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Denoise (3D Spatial + Temporal)").clicked() {
                    self.filters.add(FilterKind::Denoise {
                        luma_spatial: 4.0,
                        chroma_spatial: 3.0,
                        luma_temporal: 6.0,
                        chroma_temporal: 4.5,
                        mode: DenoiseMode::Hqdn3d,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Lens Vignette").clicked() {
                    self.filters.add(FilterKind::Vignette {
                        angle_rad: 0.45,
                        backward: false,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui
                    .button("Temporal Speed & Framerate (setpts / fps)")
                    .clicked()
                {
                    self.filters.add(FilterKind::TemporalSpeed {
                        speed_factor: 1.0,
                        target_fps: None,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Grayscale (Monochrome)").clicked() {
                    self.filters.add(FilterKind::Grayscale);
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                ui.separator();
                ui.label(
                    RichText::new("Classic VirtualDub Filters")
                        .size(11.0)
                        .weak(),
                );
                if ui.button("Levels (Input / Gamma / Output Range)").clicked() {
                    self.filters.add(FilterKind::Levels {
                        input_black: 0,
                        input_white: 255,
                        gamma: 1.0,
                        output_black: 0,
                        output_white: 255,
                        luma_only: false,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("HSV Adjust (Hue / Saturation / Value)").clicked() {
                    self.filters.add(FilterKind::HsvAdjust {
                        hue_degrees: 0.0,
                        saturation: 1.0,
                        value: 1.0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Blur / Gaussian Blur").clicked() {
                    self.filters.add(FilterKind::Blur {
                        sigma: 2.0,
                        steps: 2,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Invert / Negate Channels").clicked() {
                    self.filters.add(FilterKind::Invert {
                        invert_r: true,
                        invert_g: true,
                        invert_b: true,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Threshold (1-Bit Binary Luma)").clicked() {
                    self.filters.add(FilterKind::Threshold { level: 128 });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui
                    .button("Emboss & Edge Matrix (3×3 Convolution)")
                    .clicked()
                {
                    self.filters.add(FilterKind::EmbossEdge {
                        preset: ConvolutionPreset::Emboss,
                        strength: 1.0,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("2:1 Reduction (Half-Size Downsample)").clicked() {
                    self.filters
                        .add(FilterKind::TwoToOneReduction { high_quality: true });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Border / Letterbox / Canvas Pad").clicked() {
                    self.filters.add(FilterKind::BorderFill {
                        left: 0,
                        top: 40,
                        right: 0,
                        bottom: 40,
                        color_rgb: [0, 0, 0],
                        expand_canvas: false,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Field Swap (Swap Even/Odd Scanlines)").clicked() {
                    self.filters.add(FilterKind::FieldSwap);
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                ui.separator();
                ui.label(RichText::new("Overlays & Subtitles").size(11.0).weak());
                if ui
                    .button("Text / Timecode / Frame Counter Overlay")
                    .clicked()
                {
                    self.filters.add(FilterKind::TextOverlay {
                        text: String::new(),
                        show_timecode: true,
                        show_frame_number: true,
                        position: OverlayPosition::BottomLeft,
                        font_size: 28,
                        box_opacity: 0.55,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
                if ui.button("Subtitle Burn-In (.srt / .ass / .vtt)").clicked() {
                    self.filters.add(FilterKind::SubtitleBurnIn {
                        subtitle_path: String::new(),
                        font_size: 24,
                    });
                    self.export_settings.processing_mode = VideoProcessingMode::FullProcessing;
                    ui.close_menu();
                }
            });

            if !self.filters.filters.is_empty() {
                if ui
                    .small_button("Collapse All")
                    .on_hover_text("Collapse all filters to compact headers")
                    .clicked()
                {
                    for f in &mut self.filters.filters {
                        f.is_open = false;
                    }
                }
                if ui
                    .small_button("Expand All")
                    .on_hover_text("Expand all filter settings")
                    .clicked()
                {
                    for f in &mut self.filters.filters {
                        f.is_open = true;
                    }
                }
            }
        });

        ui.add_space(8.0);
        let mut action_up = None;
        let mut action_down = None;
        let mut action_remove = None;
        let filter_len = self.filters.filters.len();

        // Precompute input dimensions for each filter stage so Resize/Crop know their exact input aspect ratio
        let stage_dims: Vec<(u32, u32)> = (0..filter_len)
            .map(|i| self.filters.input_dimensions_at(i, src_w, src_h))
            .collect();

        for (idx, instance) in self.filters.filters.iter_mut().enumerate() {
            let (in_w, in_h) = stage_dims.get(idx).copied().unwrap_or((src_w, src_h));
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut instance.enabled, "");

                    let arrow_text = if instance.is_open { "▼" } else { "▶" };
                    let arrow_tooltip = if instance.is_open {
                        "Collapse filter"
                    } else {
                        "Expand filter"
                    };
                    if ui.small_button(arrow_text).on_hover_text(arrow_tooltip).clicked() {
                        instance.is_open = !instance.is_open;
                    }

                    let title_text = RichText::new(instance.kind.title())
                        .strong()
                        .color(if instance.enabled {
                            ui.visuals().text_color()
                        } else {
                            ui.visuals().weak_text_color()
                        });
                    let title_resp = ui.add(egui::Label::new(title_text).sense(egui::Sense::click()));
                    if title_resp
                        .on_hover_text(if instance.is_open {
                            "Click to collapse"
                        } else {
                            "Click to expand"
                        })
                        .clicked()
                    {
                        instance.is_open = !instance.is_open;
                    }

                    if !instance.is_open {
                        let summary = instance.kind.summary();
                        let summary_resp = ui.add(
                            egui::Label::new(
                                RichText::new(format!("— {summary}"))
                                    .size(11.5)
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .sense(egui::Sense::click())
                            .truncate(),
                        );
                        if summary_resp.on_hover_text("Click to expand").clicked() {
                            instance.is_open = true;
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("✖").on_hover_text("Remove filter").clicked() {
                            action_remove = Some(idx);
                        }
                        if ui
                            .add_enabled(idx + 1 < filter_len, egui::Button::new("⬇").small())
                            .on_hover_text("Move filter down")
                            .clicked()
                        {
                            action_down = Some(idx);
                        }
                        if ui
                            .add_enabled(idx > 0, egui::Button::new("⬆").small())
                            .on_hover_text("Move filter up")
                            .clicked()
                        {
                            action_up = Some(idx);
                        }
                    });
                });

                if instance.is_open {
                    ui.add_space(3.0);
                    match &mut instance.kind {
                    FilterKind::Crop {
                        left,
                        top,
                        right,
                        bottom,
                    } => {
                        ui.horizontal(|ui| {
                            ui.label("Left:");
                            ui.add(egui::DragValue::new(left).range(0..=in_w / 2).speed(2));
                            ui.label("Right:");
                            ui.add(egui::DragValue::new(right).range(0..=in_w / 2).speed(2));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Top:");
                            ui.add(egui::DragValue::new(top).range(0..=in_h / 2).speed(2));
                            ui.label("Bottom:");
                            ui.add(egui::DragValue::new(bottom).range(0..=in_h / 2).speed(2));
                        });
                        let cropped_w = in_w.saturating_sub(*left + *right).max(2) & !1;
                        let cropped_h = in_h.saturating_sub(*top + *bottom).max(2) & !1;
                        ui.label(
                            RichText::new(format!(
                                "Cropped Output: {cropped_w} × {cropped_h} (from {in_w} × {in_h})"
                            ))
                            .monospace()
                            .size(11.0)
                            .weak(),
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("Aspect Crop:").size(11.0).weak());
                            for (label, num, den) in [
                                ("16:9", 16u32, 9u32),
                                ("9:16", 9, 16),
                                ("4:3", 4, 3),
                                ("1:1", 1, 1),
                                ("2.35:1", 235, 100),
                            ] {
                                if ui.small_button(label).clicked() {
                                    let target_aspect = num as f64 / den as f64;
                                    let cur_aspect = in_w as f64 / in_h.max(1) as f64;
                                    if cur_aspect > target_aspect {
                                        let keep_w = ((in_h as f64 * target_aspect).round() as u32)
                                            .min(in_w)
                                            & !1;
                                        let margin = (in_w.saturating_sub(keep_w)) / 2;
                                        *left = margin & !1;
                                        *right = margin & !1;
                                        *top = 0;
                                        *bottom = 0;
                                    } else {
                                        let keep_h = ((in_w as f64 / target_aspect).round() as u32)
                                            .min(in_h)
                                            & !1;
                                        let margin = (in_h.saturating_sub(keep_h)) / 2;
                                        *top = margin & !1;
                                        *bottom = margin & !1;
                                        *left = 0;
                                        *right = 0;
                                    }
                                }
                            }
                            if ui.small_button("Center").clicked() {
                                let sum_x = *left + *right;
                                let sum_y = *top + *bottom;
                                *left = (sum_x / 2) & !1;
                                *right = (sum_x / 2) & !1;
                                *top = (sum_y / 2) & !1;
                                *bottom = (sum_y / 2) & !1;
                            }
                            if ui.small_button("Reset").clicked() {
                                *left = 0;
                                *top = 0;
                                *right = 0;
                                *bottom = 0;
                            }
                        });
                    }
                    FilterKind::Resize {
                        width,
                        height,
                        algorithm,
                        spec_mode,
                        scale_percent,
                        aspect_mode,
                    } => {
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                spec_mode,
                                ResizeSpecMode::Dimensions,
                                "Dimensions (W × H)",
                            );
                            ui.selectable_value(
                                spec_mode,
                                ResizeSpecMode::Percentage,
                                "Percentage (%)",
                            );
                        });

                        match *spec_mode {
                            ResizeSpecMode::Percentage => {
                                ui.add(
                                    egui::Slider::new(scale_percent, 10.0..=400.0)
                                        .suffix("%")
                                        .text("Scale"),
                                );
                                ui.horizontal_wrapped(|ui| {
                                    for pct in [25.0f32, 50.0, 75.0, 100.0, 125.0, 150.0, 200.0] {
                                        if ui.small_button(format!("{pct:.0}%")).clicked() {
                                            *scale_percent = pct;
                                        }
                                    }
                                });
                                let (eff_w, eff_h) = FilterKind::compute_resize_dimensions(
                                    in_w,
                                    in_h,
                                    *width,
                                    *height,
                                    *spec_mode,
                                    *scale_percent,
                                    *aspect_mode,
                                );
                                *width = eff_w;
                                *height = eff_h;
                                ui.label(
                                    RichText::new(format!(
                                        "Target Size: {eff_w} × {eff_h} (from {in_w} × {in_h})"
                                    ))
                                    .monospace()
                                    .size(11.0)
                                    .color(Color32::from_rgb(130, 210, 155)),
                                );
                            }
                            ResizeSpecMode::Dimensions => {
                                egui::ComboBox::from_id_salt(format!("aspect_mode_{}", instance.id))
                                    .selected_text(aspect_mode.label())
                                    .show_ui(ui, |ui| {
                                        for m in AspectRatioMode::ALL {
                                            ui.selectable_value(aspect_mode, m, m.label());
                                        }
                                    });

                                let prev_w = *width;
                                let prev_h = *height;
                                let aspect = in_w.max(1) as f64 / in_h.max(1) as f64;

                                ui.horizontal(|ui| {
                                    ui.label("W:");
                                    let w_changed = ui
                                        .add(
                                            egui::DragValue::new(width)
                                                .range(16..=7680)
                                                .speed(2)
                                                .suffix(" px"),
                                        )
                                        .changed();
                                    ui.label("H:");
                                    let h_changed = ui
                                        .add(
                                            egui::DragValue::new(height)
                                                .range(16..=4320)
                                                .speed(2)
                                                .suffix(" px"),
                                        )
                                        .changed();

                                    if *aspect_mode == AspectRatioMode::KeepAspect {
                                        if w_changed && *width != prev_w {
                                            *height = (((*width as f64) / aspect).round() as u32)
                                                .clamp(16, 4320)
                                                & !1;
                                        } else if h_changed && *height != prev_h {
                                            *width = (((*height as f64) * aspect).round() as u32)
                                                .clamp(16, 7680)
                                                & !1;
                                        }
                                    }
                                });

                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new("Presets:").size(11.0).weak());
                                    for (label, pw, ph) in [
                                        ("4K", 3840u32, 2160u32),
                                        ("1440p", 2560, 1440),
                                        ("1080p", 1920, 1080),
                                        ("720p", 1280, 720),
                                        ("480p", 854, 480),
                                        ("9:16", 1080, 1920),
                                        ("1:1", 1080, 1080),
                                    ] {
                                        if ui.small_button(label).clicked() {
                                            if *aspect_mode == AspectRatioMode::KeepAspect {
                                                let (cw, ch) =
                                                    FilterKind::compute_resize_dimensions(
                                                        in_w,
                                                        in_h,
                                                        pw,
                                                        ph,
                                                        ResizeSpecMode::Dimensions,
                                                        100.0,
                                                        AspectRatioMode::KeepAspect,
                                                    );
                                                *width = cw;
                                                *height = ch;
                                            } else {
                                                *width = pw;
                                                *height = ph;
                                            }
                                        }
                                    }
                                    if ui.small_button("Source").clicked() {
                                        *width = in_w;
                                        *height = in_h;
                                    }
                                });
                            }
                        }

                        egui::ComboBox::from_id_salt(format!("scale_alg_{}", instance.id))
                            .selected_text(format!("Filter: {}", algorithm.label()))
                            .show_ui(ui, |ui| {
                                for alg in ScalingAlgorithm::ALL {
                                    ui.selectable_value(algorithm, alg, alg.label());
                                }
                            });
                    }
                    FilterKind::Rotate {
                        angle,
                        custom_degrees,
                        bilinear,
                    } => {
                        for rot in [
                            RotationAngle::Rotate90Cw,
                            RotationAngle::Rotate180,
                            RotationAngle::Rotate270Cw,
                            RotationAngle::CustomDegrees,
                        ] {
                            ui.radio_value(angle, rot, rot.label());
                        }
                        if *angle == RotationAngle::CustomDegrees {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::Slider::new(custom_degrees, -45.0..=45.0)
                                        .suffix("°")
                                        .text("Angle"),
                                );
                                if ui.small_button("0°").clicked() {
                                    *custom_degrees = 0.0;
                                }
                            });
                            ui.checkbox(bilinear, "Bilinear Sub-pixel Interpolation");
                        }
                    }
                    FilterKind::Flip {
                        horizontal,
                        vertical,
                    } => {
                        ui.checkbox(horizontal, "Flip Horizontally (Mirror)");
                        ui.checkbox(vertical, "Flip Vertically");
                    }
                    FilterKind::BrightnessContrast {
                        brightness,
                        contrast,
                        gamma,
                    } => {
                        ui.add(egui::Slider::new(brightness, -0.5..=0.5).text("Brightness"));
                        ui.add(egui::Slider::new(contrast, 0.2..=2.5).text("Contrast"));
                        ui.add(egui::Slider::new(gamma, 0.2..=3.0).text("Midtone Gamma"));
                        if ui.small_button("Reset").clicked() {
                            *brightness = 0.0;
                            *contrast = 1.0;
                            *gamma = 1.0;
                        }
                    }
                    FilterKind::ColorAdjust {
                        saturation,
                        gamma,
                        hue_degrees,
                        temperature,
                        r_offset,
                        g_offset,
                        b_offset,
                    } => {
                        ui.add(egui::Slider::new(saturation, 0.0..=3.0).text("Saturation"));
                        ui.add(egui::Slider::new(gamma, 0.2..=3.0).text("Gamma"));
                        ui.add(egui::Slider::new(hue_degrees, -180.0..=180.0).text("Hue (°)"));
                        ui.add(
                            egui::Slider::new(temperature, -1.0..=1.0)
                                .text("Temp (Cool ↔ Warm)"),
                        );
                        ui.collapsing("RGB Channel Balance", |ui| {
                            ui.add(egui::Slider::new(r_offset, -0.5..=0.5).text("Red Offset"));
                            ui.add(egui::Slider::new(g_offset, -0.5..=0.5).text("Green Offset"));
                            ui.add(egui::Slider::new(b_offset, -0.5..=0.5).text("Blue Offset"));
                        });
                        if ui.small_button("Reset Colour").clicked() {
                            *saturation = 1.0;
                            *gamma = 1.0;
                            *hue_degrees = 0.0;
                            *temperature = 0.0;
                            *r_offset = 0.0;
                            *g_offset = 0.0;
                            *b_offset = 0.0;
                        }
                    }
                    FilterKind::Deinterlace {
                        mode,
                        engine,
                        parity,
                    } => {
                        egui::ComboBox::from_id_salt(format!("deint_eng_{}", instance.id))
                            .selected_text(engine.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    engine,
                                    DeinterlaceEngine::Bwdif,
                                    DeinterlaceEngine::Bwdif.label(),
                                );
                                ui.selectable_value(
                                    engine,
                                    DeinterlaceEngine::Yadif,
                                    DeinterlaceEngine::Yadif.label(),
                                );
                            });
                        egui::ComboBox::from_id_salt(format!("deint_par_{}", instance.id))
                            .selected_text(parity.label())
                            .show_ui(ui, |ui| {
                                for p in [
                                    FieldParity::Auto,
                                    FieldParity::TopFieldFirst,
                                    FieldParity::BottomFieldFirst,
                                ] {
                                    ui.selectable_value(parity, p, p.label());
                                }
                            });
                        ui.radio_value(
                            mode,
                            DeinterlaceMode::SendFrame,
                            DeinterlaceMode::SendFrame.label(),
                        );
                        ui.radio_value(
                            mode,
                            DeinterlaceMode::SendField,
                            DeinterlaceMode::SendField.label(),
                        );
                    }
                    FilterKind::Sharpen {
                        amount,
                        mode,
                        radius,
                        chroma_amount,
                    } => {
                        ui.horizontal(|ui| {
                            ui.selectable_value(
                                mode,
                                SharpenMode::UnsharpMask,
                                "Unsharp Mask",
                            );
                            ui.selectable_value(
                                mode,
                                SharpenMode::ContrastAdaptive,
                                "CAS (Adaptive)",
                            );
                        });
                        ui.add(egui::Slider::new(amount, 0.0..=3.0).text("Luma Strength"));
                        if *mode == SharpenMode::UnsharpMask {
                            ui.add(
                                egui::Slider::new(chroma_amount, -0.5..=1.5)
                                    .text("Chroma Strength"),
                            );
                            ui.horizontal(|ui| {
                                ui.label("Kernel Radius:");
                                for r in [3u32, 5, 7] {
                                    ui.selectable_value(radius, r, format!("{r}×{r}"));
                                }
                            });
                        }
                    }
                    FilterKind::Denoise {
                        luma_spatial,
                        chroma_spatial,
                        luma_temporal,
                        chroma_temporal,
                        mode,
                    } => {
                        egui::ComboBox::from_id_salt(format!("denoise_mode_{}", instance.id))
                            .selected_text(mode.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    mode,
                                    DenoiseMode::Hqdn3d,
                                    DenoiseMode::Hqdn3d.label(),
                                );
                                ui.selectable_value(
                                    mode,
                                    DenoiseMode::AtaDenoise,
                                    DenoiseMode::AtaDenoise.label(),
                                );
                            });
                        if *mode == DenoiseMode::Hqdn3d {
                            ui.add(
                                egui::Slider::new(luma_spatial, 0.0..=16.0).text("Luma Spatial"),
                            );
                            ui.add(
                                egui::Slider::new(chroma_spatial, 0.0..=16.0)
                                    .text("Chroma Spatial"),
                            );
                            ui.add(
                                egui::Slider::new(luma_temporal, 0.0..=24.0)
                                    .text("Luma Temporal"),
                            );
                            ui.add(
                                egui::Slider::new(chroma_temporal, 0.0..=24.0)
                                    .text("Chroma Temporal"),
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.label(RichText::new("Presets:").size(11.0).weak());
                                if ui.small_button("Light").clicked() {
                                    *luma_spatial = 2.0;
                                    *chroma_spatial = 1.5;
                                    *luma_temporal = 3.0;
                                    *chroma_temporal = 2.5;
                                }
                                if ui.small_button("Medium").clicked() {
                                    *luma_spatial = 4.0;
                                    *chroma_spatial = 3.0;
                                    *luma_temporal = 6.0;
                                    *chroma_temporal = 4.5;
                                }
                                if ui.small_button("Heavy").clicked() {
                                    *luma_spatial = 8.0;
                                    *chroma_spatial = 6.0;
                                    *luma_temporal = 12.0;
                                    *chroma_temporal = 9.0;
                                }
                            });
                        }
                    }
                    FilterKind::Vignette {
                        angle_rad,
                        backward,
                    } => {
                        ui.add(
                            egui::Slider::new(angle_rad, 0.1..=1.2)
                                .text("Vignette Angle (rad)"),
                        );
                        ui.checkbox(backward, "Brighten Corners (Reverse Vignette)");
                    }
                    FilterKind::TemporalSpeed {
                        speed_factor,
                        target_fps,
                    } => {
                        ui.add(
                            egui::Slider::new(speed_factor, 0.25..=4.0)
                                .suffix("×")
                                .text("Speed Multiplier"),
                        );
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("Speed:").size(11.0).weak());
                            for spd in [0.25f32, 0.5, 1.0, 1.5, 2.0, 4.0] {
                                if ui.small_button(format!("{spd}×")).clicked() {
                                    *speed_factor = spd;
                                }
                            }
                        });
                        let mut convert_fps = target_fps.is_some();
                        if ui.checkbox(&mut convert_fps, "Convert Output Framerate (fps)").changed() {
                            *target_fps = if convert_fps { Some(30.0) } else { None };
                        }
                        if let Some(fps) = target_fps {
                            ui.add(
                                egui::Slider::new(fps, 10.0..=120.0)
                                    .suffix(" fps")
                                    .text("Target FPS"),
                            );
                            ui.horizontal_wrapped(|ui| {
                                for preset_fps in [23.976f32, 24.0, 25.0, 29.97, 30.0, 50.0, 59.94, 60.0] {
                                    if ui.small_button(format!("{preset_fps}")).clicked() {
                                        *fps = preset_fps;
                                    }
                                }
                            });
                        }
                        ui.label(
                            RichText::new(
                                "Note: Audio tracks are automatically pitch-compensated via FFmpeg atempo during export.",
                            )
                            .size(10.5)
                            .weak(),
                        );
                    }
                    FilterKind::Grayscale => {
                        ui.label(RichText::new("Converts frame to BT.709 monochrome").weak());
                    }
                    FilterKind::Levels {
                        input_black,
                        input_white,
                        gamma,
                        output_black,
                        output_white,
                        luma_only,
                    } => {
                        ui.horizontal(|ui| {
                            ui.label("Input Black:");
                            ui.add(egui::Slider::new(input_black, 0..=250));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Input White:");
                            ui.add(egui::Slider::new(input_white, 5..=255));
                        });
                        if *input_white <= *input_black {
                            *input_white = input_black.saturating_add(1).max(1);
                        }
                        ui.add(egui::Slider::new(gamma, 0.2..=3.0).text("Gamma"));
                        ui.horizontal(|ui| {
                            ui.label("Output Black:");
                            ui.add(egui::Slider::new(output_black, 0..=255));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Output White:");
                            ui.add(egui::Slider::new(output_white, 0..=255));
                        });
                        ui.checkbox(luma_only, "Operate on Luma (Y') Only (Preserve Chroma)");
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("TV→PC (16..235 → 0..255)").clicked() {
                                *input_black = 16;
                                *input_white = 235;
                                *output_black = 0;
                                *output_white = 255;
                            }
                            if ui.small_button("PC→TV (0..255 → 16..235)").clicked() {
                                *input_black = 0;
                                *input_white = 255;
                                *output_black = 16;
                                *output_white = 235;
                            }
                            if ui.small_button("Reset").clicked() {
                                *input_black = 0;
                                *input_white = 255;
                                *gamma = 1.0;
                                *output_black = 0;
                                *output_white = 255;
                            }
                        });
                    }
                    FilterKind::HsvAdjust {
                        hue_degrees,
                        saturation,
                        value,
                    } => {
                        ui.add(
                            egui::Slider::new(hue_degrees, -180.0..=180.0)
                                .suffix("°")
                                .text("Hue Shift"),
                        );
                        ui.add(egui::Slider::new(saturation, 0.0..=3.0).text("Saturation"));
                        ui.add(egui::Slider::new(value, 0.0..=2.5).text("Value (Brightness)"));
                        if ui.small_button("Reset HSV").clicked() {
                            *hue_degrees = 0.0;
                            *saturation = 1.0;
                            *value = 1.0;
                        }
                    }
                    FilterKind::Blur { sigma, steps } => {
                        ui.add(egui::Slider::new(sigma, 0.5..=20.0).text("Blur Radius (Sigma)"));
                        ui.add(egui::Slider::new(steps, 1..=4).text("Gaussian Passes"));
                    }
                    FilterKind::Invert {
                        invert_r,
                        invert_g,
                        invert_b,
                    } => {
                        ui.horizontal(|ui| {
                            ui.checkbox(invert_r, "Red");
                            ui.checkbox(invert_g, "Green");
                            ui.checkbox(invert_b, "Blue");
                        });
                    }
                    FilterKind::Threshold { level } => {
                        ui.add(egui::Slider::new(level, 1..=254).text("Threshold Level (0..255)"));
                    }
                    FilterKind::EmbossEdge { preset, strength } => {
                        egui::ComboBox::from_id_salt(format!("conv_preset_{}", instance.id))
                            .selected_text(preset.label())
                            .show_ui(ui, |ui| {
                                for p in ConvolutionPreset::ALL {
                                    ui.selectable_value(preset, p, p.label());
                                }
                            });
                        ui.add(egui::Slider::new(strength, 0.2..=3.0).text("Kernel Strength"));
                    }
                    FilterKind::TwoToOneReduction { high_quality } => {
                        let nw = ((in_w / 2).max(2)) & !1;
                        let nh = ((in_h / 2).max(2)) & !1;
                        ui.label(
                            RichText::new(format!(
                                "Downsamples {in_w}×{in_h} → {nw}×{nh} (Exact 2:1 half-size)"
                            ))
                            .monospace()
                            .size(11.0),
                        );
                        ui.checkbox(
                            high_quality,
                            "High-Quality 3-Tap Lanczos Kernel (Uncheck for classic 2×2 box)",
                        );
                    }
                    FilterKind::BorderFill {
                        left,
                        top,
                        right,
                        bottom,
                        color_rgb,
                        expand_canvas,
                    } => {
                        ui.horizontal(|ui| {
                            ui.label("Left:");
                            ui.add(egui::DragValue::new(left).range(0..=1024).speed(2));
                            ui.label("Right:");
                            ui.add(egui::DragValue::new(right).range(0..=1024).speed(2));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Top:");
                            ui.add(egui::DragValue::new(top).range(0..=1024).speed(2));
                            ui.label("Bottom:");
                            ui.add(egui::DragValue::new(bottom).range(0..=1024).speed(2));
                        });
                        ui.horizontal(|ui| {
                            ui.label("Border Colour:");
                            ui.color_edit_button_srgb(color_rgb);
                        });
                        ui.checkbox(
                            expand_canvas,
                            "Expand Output Canvas (Pad) instead of Letterbox Overwrite",
                        );
                    }
                    FilterKind::FieldSwap => {
                        ui.label(
                            RichText::new(
                                "Swaps even and odd horizontal scanlines to repair reversed field dominance.",
                            )
                            .size(11.0)
                            .weak(),
                        );
                    }
                    FilterKind::TextOverlay {
                        text,
                        show_timecode,
                        show_frame_number,
                        position,
                        font_size,
                        box_opacity,
                    } => {
                        ui.horizontal(|ui| {
                            ui.label("Custom Text:");
                            ui.text_edit_singleline(text);
                        });
                        ui.horizontal(|ui| {
                            ui.checkbox(show_timecode, "Burn-In Timecode (HH:MM:SS)");
                            ui.checkbox(show_frame_number, "Burn-In Frame #");
                        });
                        egui::ComboBox::from_id_salt(format!("ov_pos_{}", instance.id))
                            .selected_text(format!("Position: {}", position.label()))
                            .show_ui(ui, |ui| {
                                for pos in OverlayPosition::ALL {
                                    ui.selectable_value(position, pos, pos.label());
                                }
                            });
                        ui.add(egui::Slider::new(font_size, 12..=72).suffix(" px").text("Font Size"));
                        ui.add(egui::Slider::new(box_opacity, 0.0..=1.0).text("Background Box Opacity"));
                    }
                    FilterKind::SubtitleBurnIn {
                        subtitle_path,
                        font_size,
                    } => {
                        ui.horizontal(|ui| {
                            if ui.button("📂 Choose .SRT / .ASS / .VTT...").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("Subtitle Files", &["srt", "ass", "ssa", "vtt"])
                                    .pick_file()
                                {
                                    *subtitle_path = path.display().to_string();
                                }
                            }
                            if !subtitle_path.is_empty() && ui.small_button("Clear").clicked() {
                                subtitle_path.clear();
                            }
                        });
                        ui.text_edit_singleline(subtitle_path);
                        ui.add(egui::Slider::new(font_size, 12..=56).suffix(" pt").text("Font Size"));
                    }
                }
            }
        });
        }

        if let Some(i) = action_up {
            self.filters.move_up(i);
        }
        if let Some(i) = action_down {
            self.filters.move_down(i);
        }
        if let Some(i) = action_remove {
            self.filters.remove(i);
        }

        let (vf_str, out_w, out_h) = self.filters.to_ffmpeg_filtergraph(src_w, src_h);
        ui.add_space(8.0);
        ui.label(RichText::new(format!("Output Dimensions: {out_w} x {out_h}")).strong());
        if let Some(vf) = vf_str {
            ui.label(RichText::new("Compiled FFmpeg -vf filtergraph:").weak());
            ui.code(&vf);
            if self.project.is_loaded()
                && ui
                    .button("🔍 Render Exact FFmpeg -vf Frame to Output Preview")
                    .clicked()
            {
                if let (Some(bins), Some(resolved)) = (
                    self.binaries.clone(),
                    self.project.resolve_frame(self.project.current_frame),
                ) {
                    if let Some(clip) = self.project.source_by_id(resolved.source_id).cloned() {
                        let dec = FfmpegProcessDecoder::new(bins, self.settings.prefer_hardware_decode);
                        if let Ok(exact) = dec.decode_filtered_frame(
                            &clip,
                            resolved.source_frame,
                            &vf,
                            out_w,
                            out_h,
                        ) {
                            let img = ColorImage::from_rgba_unmultiplied(
                                [exact.width as usize, exact.height as usize],
                                &exact.rgba,
                            );
                            self.output_texture = Some(ui.ctx().load_texture(
                                "cutdub_output_exact",
                                img,
                                TextureOptions::LINEAR,
                            ));
                        }
                    }
                }
            }
        }

        // Live RGB + Luma Histogram & Clipping Scope
        ui.add_space(10.0);
        ui.separator();
        ui.checkbox(&mut self.show_histogram, "Show Live RGB / Luma Histogram Scope");
        if self.show_histogram {
            if let Some(hist) = &self.current_histogram {
                let scope_w = ui.available_width();
                let scope_h = 84.0;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(scope_w, scope_h), Sense::hover());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 3.0, Color32::from_rgb(13, 15, 19));
                painter.rect_stroke(
                    rect,
                    3.0,
                    Stroke::new(1.0_f32, Color32::from_rgb(48, 54, 66)),
                    StrokeKind::Inside,
                );

                let col_w = (rect.width() / 64.0).max(1.0);
                for i in 0..64 {
                    let x = rect.left() + i as f32 * col_w;
                    let bot = rect.bottom() - 2.0;
                    let max_h = rect.height() - 6.0;

                    let r_y = bot - hist.r_bins[i] * max_h;
                    let g_y = bot - hist.g_bins[i] * max_h;
                    let b_y = bot - hist.b_bins[i] * max_h;
                    let l_y = bot - hist.luma_bins[i] * max_h;

                    painter.line_segment(
                        [Pos2::new(x, bot), Pos2::new(x, r_y)],
                        Stroke::new(col_w * 0.85, Color32::from_rgba_unmultiplied(235, 75, 75, 85)),
                    );
                    painter.line_segment(
                        [Pos2::new(x, bot), Pos2::new(x, g_y)],
                        Stroke::new(col_w * 0.85, Color32::from_rgba_unmultiplied(75, 225, 110, 85)),
                    );
                    painter.line_segment(
                        [Pos2::new(x, bot), Pos2::new(x, b_y)],
                        Stroke::new(col_w * 0.85, Color32::from_rgba_unmultiplied(85, 145, 245, 85)),
                    );
                    painter.line_segment(
                        [Pos2::new(x, l_y), Pos2::new(x + col_w, l_y)],
                        Stroke::new(1.4_f32, Color32::from_rgb(230, 235, 245)),
                    );
                }

                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Avg Luma: {:.0}%  |  Shadows Clip: {:.1}%  |  Highlights Clip: {:.1}%",
                            hist.mean_luma * 100.0,
                            hist.shadow_clip_pct,
                            hist.highlight_clip_pct
                        ))
                        .monospace()
                        .size(10.5)
                        .weak(),
                    );
                });
            }
        }
    }

    fn render_export_panel(&mut self, ui: &mut Ui) {
        ui.heading("Export & Encoding");

        // Presets selector
        ui.group(|ui| {
            ui.label(RichText::new("Encoding Presets").strong());
            egui::ComboBox::from_id_salt("preset_selector")
                .selected_text(self.selected_preset.as_ref()
                    .filter(|(_, settings)| settings == &self.export_settings)
                    .map(|(name, _)| name.as_str()).unwrap_or("Custom"))
                .show_ui(ui, |ui| {
                    for preset in &self.presets {
                        if let Some(available) = preset.available_with(&self.capabilities) {
                            if ui.button(&available.name).clicked() {
                                self.selected_preset = Some((available.name, available.settings.clone()));
                                self.export_settings = available.settings;
                                ui.close_menu();
                            }
                        }
                    }
                });
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut self.new_preset_name);
                if ui.button("Save Preset").clicked() && !self.new_preset_name.trim().is_empty() {
                    self.presets.push(EncodingPreset {
                        name: self.new_preset_name.trim().to_string(),
                        settings: self.export_settings.clone(),
                    });
                    match EncodingPreset::save_all(&self.paths.presets_file, &self.presets) {
                        Ok(()) => {
                            self.selected_preset = Some((self.new_preset_name.trim().to_string(), self.export_settings.clone()));
                            self.new_preset_name.clear();
                        }
                        Err(error) => {
                            self.presets.pop();
                            self.set_status(format!("Could not save preset: {error}"), true);
                        }
                    }
                }
            });
        });

        ui.add_space(6.0);
        ui.checkbox(
            &mut self.export_settings.export_selection_only,
            "Export Active Selection Only (when marked)",
        );
        ui.checkbox(
            &mut self.export_settings.preserve_metadata,
            "Preserve Container Metadata",
        );

        ui.add_space(6.0);
        ui.label(RichText::new("Processing Mode:").strong());
        ui.radio_value(
            &mut self.export_settings.processing_mode,
            VideoProcessingMode::DirectStreamCopy,
            "Direct Stream Copy (Lossless, No Re-encoding)",
        );
        ui.radio_value(
            &mut self.export_settings.processing_mode,
            VideoProcessingMode::SmartCopy,
            "Smart Cut / Smart Rendering (Re-encode GOP boundary + Lossless Stream Copy)",
        );
        ui.radio_value(
            &mut self.export_settings.processing_mode,
            VideoProcessingMode::FullProcessing,
            "Full Processing Mode (Frame-Exact + Filters + Re-encode)",
        );

        ui.add_space(6.0);
        ui.label(RichText::new("Target Container:").strong());
        egui::ComboBox::from_id_salt("container_combo")
            .selected_text(self.export_settings.container.label())
            .show_ui(ui, |ui| {
                for c in ContainerFormat::ALL {
                    ui.selectable_value(&mut self.export_settings.container, c, c.label());
                }
            });

        match self.export_settings.processing_mode {
            VideoProcessingMode::DirectStreamCopy => {
                ui.add_space(8.0);
                ui.group(|ui| {
                    ui.label(RichText::new("Direct Stream Copy Keyframe Policy").strong());
                    ui.radio_value(
                        &mut self.export_settings.keyframe_policy,
                        KeyframeCutPolicy::SnapToPrecedingKeyframe,
                        "Snap start point to nearest preceding keyframe (Recommended)",
                    );
                    ui.radio_value(
                        &mut self.export_settings.keyframe_policy,
                        KeyframeCutPolicy::ExactTimestamp,
                        "Cut at exact requested timestamp (May include undecodable lead-in frames)",
                    );

                    if let Some(analysis) = self
                        .project
                        .analyze_stream_copy(self.export_settings.export_selection_only)
                    {
                        ui.separator();
                        if analysis.starts_on_keyframe && analysis.warning_message.is_none() {
                            ui.label(
                                RichText::new(format!(
                                    "✔ Cut starts cleanly on Keyframe #{}. Lossless stream copy will be frame-exact at the start.",
                                    analysis.requested_start_frame
                                ))
                                .color(Color32::from_rgb(110, 220, 140)),
                            );
                        } else if let Some(warn) = &analysis.warning_message {
                            ui.label(
                                RichText::new(format!("⚠ {warn}"))
                                    .color(Color32::from_rgb(245, 185, 80)),
                            );
                        }
                    }
                });
            }
            VideoProcessingMode::SmartCopy => {
                ui.add_space(8.0);
                ui.group(|ui| {
                    ui.label(RichText::new("Smart Cut / Hybrid GOP Rendering Analysis").strong());
                    if let Some(analysis) = self
                        .project
                        .analyze_stream_copy(self.export_settings.export_selection_only)
                    {
                        if analysis.starts_on_keyframe {
                            ui.label(
                                RichText::new(format!(
                                    "✔ Cut starts on Keyframe #{}. 100% of the video stream will be copied losslessly without re-encoding.",
                                    analysis.requested_start_frame
                                ))
                                .color(Color32::from_rgb(110, 220, 140)),
                            );
                        } else {
                            ui.label(
                                RichText::new(format!(
                                    "🧠 Cut starts at non-keyframe #{}. Smart Cut will frame-accurately re-encode only {} lead-in frame(s) (#{}..#{}) and lossless stream-copy the remaining video from Keyframe #{} onward.",
                                    analysis.requested_start_frame,
                                    analysis.smart_cut_reencode_frames,
                                    analysis.requested_start_frame,
                                    analysis.next_keyframe_after_start.saturating_sub(1),
                                    analysis.next_keyframe_after_start
                                ))
                                .color(Color32::from_rgb(125, 205, 255)),
                            );
                        }
                    }
                });
            }
            VideoProcessingMode::FullProcessing => {
                ui.add_space(8.0);
                ui.group(|ui| {
                    ui.label(RichText::new("Video Encoder & Quality").strong());
                    let encoders = self.capabilities.preferred_video_encoders();
                    let mut picked_encoder: Option<String> = None;
                    egui::ComboBox::from_id_salt("video_encoder_combo")
                        .selected_text(if self.capabilities.has_encoder(&self.export_settings.video_encoder) {
                            self.export_settings.video_encoder.clone()
                        } else { "Select an available encoder...".into() })
                        .show_ui(ui, |ui| {
                            for enc in &encoders {
                                let badge = if enc.is_hardware { " [HW]" } else { " [SW]" };
                                let selected = self.export_settings.video_encoder == enc.name;
                                if ui
                                    .selectable_label(
                                        selected,
                                        format!("{}{} — {}", enc.name, badge, enc.description),
                                    )
                                    .clicked()
                                {
                                    picked_encoder = Some(enc.name.clone());
                                }
                            }
                        });
                    if let Some(new_enc) = picked_encoder {
                        self.export_settings
                            .apply_recommended_defaults_for_encoder(&new_enc);
                    }

                    let cpus = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(8);
                    ui.label(
                        RichText::new(format!(
                            "⚡ Multi-core parallel encoding active ({cpus} CPU threads)"
                        ))
                        .size(11.0)
                        .color(Color32::from_rgb(125, 205, 160)),
                    );

                    ui.horizontal(|ui| {
                        ui.label("Rate Control:");
                        ui.selectable_value(
                            &mut self.export_settings.rate_control_mode,
                            VideoRateControlMode::Crf,
                            "CRF / Quality",
                        );
                        ui.selectable_value(
                            &mut self.export_settings.rate_control_mode,
                            VideoRateControlMode::TargetBitrateKbps,
                            "Target Bitrate",
                        );
                        ui.selectable_value(
                            &mut self.export_settings.rate_control_mode,
                            VideoRateControlMode::TargetFileSizeMb,
                            "Target Size (MB)",
                        );
                    });

                    match self.export_settings.rate_control_mode {
                        VideoRateControlMode::Crf => {
                            ui.add(
                                egui::Slider::new(&mut self.export_settings.crf, 0..=51)
                                    .text("CRF / Quality (lower = higher quality)"),
                            );
                        }
                        VideoRateControlMode::TargetBitrateKbps => {
                            ui.add(
                                egui::Slider::new(
                                    &mut self.export_settings.target_video_bitrate_kbps,
                                    250..=50_000,
                                )
                                .logarithmic(true)
                                .suffix(" kbps")
                                .text("Video Bitrate"),
                            );
                            ui.horizontal_wrapped(|ui| {
                                for (lbl, kbps) in [
                                    ("1M", 1000u32),
                                    ("2.5M", 2500),
                                    ("5M", 5000),
                                    ("8M", 8000),
                                    ("15M", 15_000),
                                    ("25M", 25_000),
                                ] {
                                    if ui.small_button(lbl).clicked() {
                                        self.export_settings.target_video_bitrate_kbps = kbps;
                                    }
                                }
                            });
                        }
                        VideoRateControlMode::TargetFileSizeMb => {
                            ui.add(
                                egui::Slider::new(
                                    &mut self.export_settings.target_file_size_mb,
                                    1.0..=500.0,
                                )
                                .logarithmic(true)
                                .suffix(" MB")
                                .text("Target Size"),
                            );
                            ui.horizontal_wrapped(|ui| {
                                for mb in [8.0f32, 10.0, 25.0, 50.0, 100.0, 250.0] {
                                    if ui.small_button(format!("{mb:.0} MB")).clicked() {
                                        self.export_settings.target_file_size_mb = mb;
                                    }
                                }
                            });
                            let dur_secs = self
                                .project
                                .total_duration()
                                .as_seconds_f64()
                                .max(0.5);
                            let act_aud = self
                                .project
                                .audio_configs
                                .iter()
                                .filter(|a| a.mode != AudioExportMode::Disabled)
                                .count();
                            if let Some(calc_kbps) = self
                                .export_settings
                                .effective_video_bitrate_kbps(dur_secs, act_aud)
                            {
                                ui.label(
                                    RichText::new(format!(
                                        "Auto-calculated video bitrate: ~{calc_kbps} kbps"
                                    ))
                                    .monospace()
                                    .size(11.0)
                                    .color(Color32::from_rgb(135, 205, 250)),
                                );
                            }
                        }
                    }

                    egui::ComboBox::from_id_salt("enc_preset_combo")
                        .selected_text(format!("Speed Preset: {}", self.export_settings.encoder_preset))
                        .show_ui(ui, |ui| {
                            for p in [
                                "ultrafast",
                                "superfast",
                                "veryfast",
                                "faster",
                                "fast",
                                "medium",
                                "slow",
                                "slower",
                                "veryslow",
                            ] {
                                ui.selectable_value(
                                    &mut self.export_settings.encoder_preset,
                                    p.to_string(),
                                    p,
                                );
                            }
                        });

                    ui.checkbox(
                        &mut self.export_settings.use_hwaccel_decode,
                        "Use hardware acceleration for decoding (-hwaccel auto)",
                    );
                });
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.label(RichText::new("Audio Processing & Encoding:").strong());
        self.render_audio_encoding_section(ui, "export_tab");

        ui.add_space(12.0);
        if let Some(prog) = &self.export_progress {
            ui.group(|ui| {
                ui.label(RichText::new("Exporting Video...").strong());
                ui.add(
                    egui::ProgressBar::new(prog.fraction)
                        .show_percentage()
                        .animate(true),
                );
                ui.label(
                    RichText::new(format!(
                        "{} / {}  |  {:.1} fps ({})  |  {:.2} MB",
                        prog.encoded_time,
                        prog.total_duration,
                        prog.fps,
                        prog.speed,
                        prog.size_bytes as f64 / (1024.0 * 1024.0)
                    ))
                    .monospace()
                    .size(11.5),
                );
                if let Some(job) = &self.export_job {
                    if ui
                        .button(
                            RichText::new("⏹ Cancel Export")
                                .color(Color32::from_rgb(255, 110, 110)),
                        )
                        .clicked()
                    {
                        job.cancel();
                    }
                }
            });
        } else if ui
            .add_enabled(
                self.project.is_loaded() && self.export_settings.encoders_available(&self.capabilities),
                egui::Button::new(
                    RichText::new("💾 Export Video Now... (Cmd/Ctrl+E)")
                        .size(14.5)
                        .strong(),
                )
                .min_size(Vec2::new(ui.available_width(), 34.0)),
            )
            .clicked()
        {
            self.trigger_export_dialog();
        }
    }

    fn render_audio_encoding_section(&mut self, ui: &mut Ui, id_prefix: &str) {
        let prev_mode = self.export_settings.audio_processing_mode;
        ui.radio_value(
            &mut self.export_settings.audio_processing_mode,
            AudioProcessingMode::DirectStreamCopy,
            "Direct Stream Copy (Lossless Audio Copy / Per-Track Mode)",
        );
        ui.radio_value(
            &mut self.export_settings.audio_processing_mode,
            AudioProcessingMode::FullProcessing,
            "Full Audio Processing (Re-encode + Gain / Resample / Normalize)",
        );
        ui.radio_value(
            &mut self.export_settings.audio_processing_mode,
            AudioProcessingMode::NoAudio,
            "No Audio (Mute / Strip All Audio Tracks)",
        );

        let primary_audio = self
            .project
            .audio_configs
            .first()
            .map(|c| (c.info.channels, c.info.sample_rate));
        let (source_channels, source_sr) = match primary_audio {
            Some((ch, sr)) => (Some(ch), Some(sr)),
            None => (None, None),
        };
        let global_profile = self
            .export_settings
            .audio_bitrate_profile(source_channels, source_sr);

        if self.export_settings.audio_processing_mode != prev_mode {
            match self.export_settings.audio_processing_mode {
                AudioProcessingMode::DirectStreamCopy => {
                    for cfg in &mut self.project.audio_configs {
                        cfg.mode = AudioExportMode::DirectStreamCopy;
                    }
                }
                AudioProcessingMode::FullProcessing => {
                    self.export_settings
                        .clamp_audio_bitrate(source_channels, source_sr);
                    let enc = self.export_settings.audio_encoder.clone();
                    let br = self.export_settings.audio_bitrate_kbps;
                    for cfg in &mut self.project.audio_configs {
                        if cfg.mode != AudioExportMode::Disabled {
                            cfg.mode = AudioExportMode::ReEncode {
                                codec: enc.clone(),
                                bitrate_kbps: br,
                            };
                        }
                    }
                }
                AudioProcessingMode::NoAudio => {}
            }
        }

        let audio_encoders = self.capabilities.preferred_audio_encoders();
        if self.export_settings.audio_processing_mode == AudioProcessingMode::FullProcessing {
            ui.add_space(6.0);
            ui.group(|ui| {
                ui.label(RichText::new("Audio Encoder, Format & Filters").strong());

                let mut picked_audio_enc: Option<String> = None;
                egui::ComboBox::from_id_salt(format!("{id_prefix}_global_aud_enc"))
                    .selected_text(if self.capabilities.has_encoder(&self.export_settings.audio_encoder) {
                        self.export_settings.audio_encoder.clone()
                    } else { "Select an available audio encoder...".into() })
                    .show_ui(ui, |ui| {
                        for enc in &audio_encoders {
                            let badge = if enc.is_hardware { " [HW]" } else { " [SW]" };
                            let selected = self.export_settings.audio_encoder == enc.name;
                            if ui
                                .selectable_label(
                                    selected,
                                    format!("{}{} — {}", enc.name, badge, enc.description),
                                )
                                .clicked()
                            {
                                picked_audio_enc = Some(enc.name.clone());
                            }
                        }
                    });
                if let Some(new_aud) = picked_audio_enc {
                    self.export_settings
                        .apply_recommended_defaults_for_audio_encoder_with_format(
                            &new_aud,
                            source_channels,
                            source_sr,
                        );
                    let br = self.export_settings.audio_bitrate_kbps;
                    for cfg in &mut self.project.audio_configs {
                        if matches!(cfg.mode, AudioExportMode::ReEncode { .. }) {
                            cfg.mode = AudioExportMode::ReEncode {
                                codec: new_aud.clone(),
                                bitrate_kbps: br,
                            };
                        }
                    }
                }

                if global_profile.is_lossless {
                    ui.label(
                        RichText::new("✔ Lossless / Uncompressed PCM (Bitrate not applicable)")
                            .size(11.0)
                            .color(Color32::from_rgb(125, 205, 160)),
                    );
                } else {
                    if ui
                        .add(
                            egui::Slider::new(
                                &mut self.export_settings.audio_bitrate_kbps,
                                global_profile.min_kbps..=global_profile.max_kbps,
                            )
                            .suffix(" kbps")
                            .text("Bitrate"),
                        )
                        .changed()
                    {
                        let enc = self.export_settings.audio_encoder.clone();
                        let br = self.export_settings.audio_bitrate_kbps;
                        for cfg in &mut self.project.audio_configs {
                            if matches!(cfg.mode, AudioExportMode::ReEncode { .. }) {
                                cfg.mode = AudioExportMode::ReEncode {
                                    codec: enc.clone(),
                                    bitrate_kbps: br,
                                };
                            }
                        }
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("Presets:").size(11.0).weak());
                        for &preset_br in &global_profile.presets {
                            let active = self.export_settings.audio_bitrate_kbps == preset_br;
                            if ui.selectable_label(active, format!("{preset_br}k")).clicked() {
                                self.export_settings.audio_bitrate_kbps = preset_br;
                                let enc = self.export_settings.audio_encoder.clone();
                                for cfg in &mut self.project.audio_configs {
                                    if matches!(cfg.mode, AudioExportMode::ReEncode { .. }) {
                                        cfg.mode = AudioExportMode::ReEncode {
                                            codec: enc.clone(),
                                            bitrate_kbps: preset_br,
                                        };
                                    }
                                }
                            }
                        }
                    });

                    if let Some(notice) = global_profile.notice {
                        ui.label(
                            RichText::new(format!("ℹ {notice}"))
                                .size(11.0)
                                .color(Color32::from_rgb(235, 185, 95)),
                        );
                    }
                }

                let prev_sr = self.export_settings.audio_sample_rate;
                ui.horizontal(|ui| {
                    ui.label("Sample Rate:");
                    let sr_label = match self.export_settings.audio_sample_rate {
                        None => "Keep Source".to_string(),
                        Some(sr) => format!("{sr} Hz"),
                    };
                    egui::ComboBox::from_id_salt(format!("{id_prefix}_aud_sr"))
                        .selected_text(sr_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.export_settings.audio_sample_rate,
                                None,
                                "Keep Source Sample Rate",
                            );
                            for (sr, desc) in [
                                (48_000u32, "48000 Hz (48 kHz Video Standard)"),
                                (44_100, "44100 Hz (44.1 kHz CD Audio)"),
                                (96_000, "96000 Hz (96 kHz Studio Hi-Res)"),
                                (32_000, "32000 Hz (32 kHz Broadcast)"),
                                (24_000, "24000 Hz (24 kHz Voice)"),
                            ] {
                                ui.selectable_value(
                                    &mut self.export_settings.audio_sample_rate,
                                    Some(sr),
                                    desc,
                                );
                            }
                        });
                });
                if self.export_settings.audio_sample_rate != prev_sr {
                    self.export_settings
                        .clamp_audio_bitrate(source_channels, source_sr);
                    let br = self.export_settings.audio_bitrate_kbps;
                    for cfg in &mut self.project.audio_configs {
                        if let AudioExportMode::ReEncode { bitrate_kbps, .. } = &mut cfg.mode {
                            *bitrate_kbps = br;
                        }
                    }
                }

                let prev_ch = self.export_settings.audio_channels;
                ui.horizontal(|ui| {
                    ui.label("Channels:");
                    egui::ComboBox::from_id_salt(format!("{id_prefix}_aud_ch"))
                        .selected_text(self.export_settings.audio_channels.label())
                        .show_ui(ui, |ui| {
                            for ch in AudioChannelMode::ALL {
                                ui.selectable_value(
                                    &mut self.export_settings.audio_channels,
                                    ch,
                                    ch.label(),
                                );
                            }
                        });
                });
                if self.export_settings.audio_channels != prev_ch {
                    self.export_settings
                        .clamp_audio_bitrate(source_channels, source_sr);
                    let br = self.export_settings.audio_bitrate_kbps;
                    for cfg in &mut self.project.audio_configs {
                        if let AudioExportMode::ReEncode { bitrate_kbps, .. } = &mut cfg.mode {
                            *bitrate_kbps = br;
                        }
                    }
                }

                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut self.export_settings.audio_volume_db, -20.0..=20.0)
                            .step_by(0.5)
                            .suffix(" dB")
                            .text("Gain"),
                    );
                    if self.export_settings.audio_volume_db.abs() > 0.05
                        && ui.small_button("0 dB").clicked()
                    {
                        self.export_settings.audio_volume_db = 0.0;
                    }
                });

                ui.checkbox(
                    &mut self.export_settings.audio_normalize_loudness,
                    "EBU R128 Loudness Normalization (-16 LUFS / -1.5 dBTP)",
                );
            });
        }

        if self.export_settings.audio_processing_mode != AudioProcessingMode::NoAudio {
            ui.add_space(4.0);
            if self.project.audio_configs.is_empty() {
                ui.label(RichText::new("No audio streams present in loaded source.").weak());
            } else {
                for (idx, cfg) in self.project.audio_configs.iter_mut().enumerate() {
                    ui.group(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Track #{idx} (Stream #{}) — {} {}Hz {}ch {}",
                                cfg.info.stream_index,
                                cfg.info.codec_name.to_uppercase(),
                                cfg.info.sample_rate,
                                cfg.info.channels,
                                cfg.info.language.as_deref().unwrap_or("")
                            ))
                            .strong(),
                        );

                        ui.horizontal(|ui| {
                            let is_copy = matches!(cfg.mode, AudioExportMode::DirectStreamCopy);
                            let is_reenc = matches!(cfg.mode, AudioExportMode::ReEncode { .. });
                            let is_dis = matches!(cfg.mode, AudioExportMode::Disabled);

                            if ui.selectable_label(is_copy, "Stream Copy").clicked() {
                                cfg.mode = AudioExportMode::DirectStreamCopy;
                            }
                            if ui.selectable_label(is_reenc, "Re-encode").clicked() {
                                cfg.mode = AudioExportMode::ReEncode {
                                    codec: self.export_settings.audio_encoder.clone(),
                                    bitrate_kbps: self.export_settings.audio_bitrate_kbps,
                                };
                            }
                            if ui.selectable_label(is_dis, "Disable").clicked() {
                                cfg.mode = AudioExportMode::Disabled;
                            }
                        });

                        if let AudioExportMode::ReEncode {
                            codec,
                            bitrate_kbps,
                        } = &mut cfg.mode
                        {
                            let track_ch = self
                                .export_settings
                                .audio_channels
                                .channel_count()
                                .unwrap_or(cfg.info.channels);
                            let track_sr = self
                                .export_settings
                                .audio_sample_rate
                                .unwrap_or(cfg.info.sample_rate);
                            let mut track_profile = redub_export::audio_bitrate_profile(
                                codec,
                                Some(track_sr),
                                track_ch,
                            );

                            ui.horizontal(|ui| {
                                ui.label("Codec:");
                                let mut track_enc_changed = false;
                                egui::ComboBox::from_id_salt(format!("{id_prefix}_aud_enc_{idx}"))
                                    .selected_text(codec.clone())
                                    .show_ui(ui, |ui| {
                                        for enc in &audio_encoders {
                                            if ui
                                                .selectable_value(
                                                    codec,
                                                    enc.name.clone(),
                                                    &enc.name,
                                                )
                                                .clicked()
                                            {
                                                track_enc_changed = true;
                                            }
                                        }
                                    });
                                if track_enc_changed {
                                    track_profile = redub_export::audio_bitrate_profile(
                                        codec,
                                        Some(track_sr),
                                        track_ch,
                                    );
                                    if !track_profile.is_lossless {
                                        *bitrate_kbps = track_profile.default_kbps;
                                    }
                                }

                                if is_lossless_or_pcm_audio_encoder(codec) {
                                    ui.label(RichText::new("Lossless / PCM").size(11.0).weak());
                                } else {
                                    *bitrate_kbps = (*bitrate_kbps)
                                        .clamp(track_profile.min_kbps, track_profile.max_kbps);
                                    ui.add(
                                        egui::DragValue::new(bitrate_kbps)
                                            .range(track_profile.min_kbps..=track_profile.max_kbps)
                                            .speed(8)
                                            .suffix(" kbps"),
                                    );
                                    for &p_br in &track_profile.presets {
                                        if ui
                                            .selectable_label(
                                                *bitrate_kbps == p_br,
                                                format!("{p_br}k"),
                                            )
                                            .clicked()
                                        {
                                            *bitrate_kbps = p_br;
                                        }
                                    }
                                }
                            });

                            if let Some(notice) = track_profile.notice {
                                ui.label(
                                    RichText::new(format!("ℹ {notice}"))
                                        .size(10.5)
                                        .color(Color32::from_rgb(235, 185, 95)),
                                );
                            }
                        }

                        ui.horizontal(|ui| {
                            ui.label("Sync Delay:");
                            ui.add(
                                egui::DragValue::new(&mut cfg.delay_ms)
                                    .range(-5000..=5000)
                                    .speed(5)
                                    .suffix(" ms"),
                            );
                            if cfg.delay_ms != 0 && ui.small_button("Reset").clicked() {
                                cfg.delay_ms = 0;
                            }
                        });
                    });
                }
            }

            ui.add_space(4.0);
            ui.group(|ui| {
                ui.label(RichText::new("External Audio Track (Audio from Other File)").strong());
                if let Some(ext_path) = self.project.external_audio_path.clone() {
                    let fname = ext_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("external_audio");
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("🎵 {fname}"))
                                .monospace()
                                .color(Color32::from_rgb(125, 215, 165)),
                        );
                        if ui.small_button("✖ Remove").clicked() {
                            self.project.external_audio_path = None;
                        }
                    });
                    ui.checkbox(
                        &mut self.project.replace_source_audio_with_external,
                        "Replace source audio track(s) (uncheck to mux as an additional track)",
                    );
                } else if ui.button("📂 Load External Audio File (.wav / .mp3 / .flac / .m4a)...").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter(
                            "Audio Files",
                            &["wav", "mp3", "flac", "m4a", "aac", "opus", "ogg", "ac3"],
                        )
                        .pick_file()
                    {
                        self.project.external_audio_path = Some(path);
                    }
                }
            });

            if self.project.is_loaded() && self.export_job.is_none() {
                ui.add_space(4.0);
                if ui
                    .button("🎧 Export Audio Only (.wav / .mp3 / .flac / .opus / .m4a)...")
                    .on_hover_text("Extract or transcode audio from the active selection/timeline without video")
                    .clicked()
                {
                    if let (Some(bins), Some(src)) =
                        (self.binaries.clone(), self.project.primary_source().cloned())
                    {
                        let stem = src
                            .path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("audio");
                        if let Some(out_path) = rfd::FileDialog::new()
                            .set_file_name(format!("{stem}_audio.mp3"))
                            .add_filter("MP3 Audio (.mp3)", &["mp3"])
                            .add_filter("WAV Uncompressed (.wav)", &["wav"])
                            .add_filter("FLAC Lossless (.flac)", &["flac"])
                            .add_filter("Opus Audio (.opus)", &["opus"])
                            .add_filter("AAC / M4A (.m4a)", &["m4a"])
                            .save_file()
                        {
                            match start_audio_only_export_job(
                                bins,
                                self.project.clone(),
                                self.filters.clone(),
                                self.export_settings.clone(),
                                out_path,
                            ) {
                                Ok(handle) => {
                                    self.export_job = Some(handle);
                                    self.set_status("Started audio-only export...", false);
                                }
                                Err(e) => {
                                    self.set_status(format!("Audio export failed: {e}"), true);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn render_batch_panel(&mut self, ui: &mut Ui) {
        ui.heading("Batch Processing Queue");
        ui.label(
            RichText::new(
                "Process multiple video files through the active Filter Pipeline and Export Settings.",
            )
            .weak()
            .size(12.0),
        );
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if ui.button("➕ Add Video Files...").clicked() {
                if let Some(files) = rfd::FileDialog::new()
                    .add_filter("Video Files", &["mp4", "mkv", "mov", "avi", "webm"])
                    .pick_files()
                {
                    let ext = self.export_settings.container.extension();
                    for path in files {
                        let out_dir = self
                            .batch_output_dir
                            .clone()
                            .or_else(|| path.parent().map(|p| p.to_path_buf()))
                            .unwrap_or_else(|| self.paths.cache_dir.clone());
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("video");
                        let out_path = out_dir.join(format!("{stem}_batch.{ext}"));
                        let next_id = self.batch_items.len() + 1;
                        self.batch_items.push(BatchJobItem {
                            id: next_id,
                            input_path: path,
                            output_path: out_path,
                            status: BatchItemStatus::Pending,
                        });
                    }
                }
            }
            if ui.button("📁 Output Folder...").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    let ext = self.export_settings.container.extension();
                    for item in &mut self.batch_items {
                        let stem = item
                            .input_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("video");
                        item.output_path = folder.join(format!("{stem}_batch.{ext}"));
                    }
                    self.batch_output_dir = Some(folder);
                }
            }
            if !self.batch_items.is_empty()
                && self.batch_job.is_none()
                && ui.button("🗑 Clear").clicked()
            {
                self.batch_items.clear();
            }
        });

        if let Some(dir) = &self.batch_output_dir {
            ui.label(RichText::new(format!("Output dir: {}", dir.display())).monospace().size(11.0));
        }

        ui.add_space(6.0);
        if self.batch_items.is_empty() {
            ui.label(RichText::new("Queue is empty. Add video files above to begin.").weak());
        } else {
            let mut remove_idx = None;
            for (idx, item) in self.batch_items.iter().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let name = item
                            .input_path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("file");
                        ui.label(RichText::new(format!("#{} {name}", idx + 1)).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if self.batch_job.is_none() && ui.small_button("✖").clicked() {
                                remove_idx = Some(idx);
                            }
                        });
                    });
                    match &item.status {
                        BatchItemStatus::Pending => {
                            ui.label(RichText::new("Status: Queued").weak());
                        }
                        BatchItemStatus::Running { fraction } => {
                            ui.add(egui::ProgressBar::new(*fraction).show_percentage());
                        }
                        BatchItemStatus::Completed { duration } => {
                            ui.label(
                                RichText::new(format!("✔ Completed ({duration})"))
                                    .color(Color32::from_rgb(110, 220, 140)),
                            );
                        }
                        BatchItemStatus::Failed(err) => {
                            ui.label(
                                RichText::new(format!("✖ Failed: {err}"))
                                    .color(Color32::from_rgb(255, 110, 110)),
                            );
                        }
                        BatchItemStatus::Cancelled => {
                            ui.label(RichText::new("Cancelled").weak());
                        }
                    }
                });
            }
            if let Some(i) = remove_idx {
                self.batch_items.remove(i);
            }

            ui.add_space(8.0);
            if let Some(job) = &self.batch_job {
                if ui
                    .button(
                        RichText::new("⏹ Cancel Batch Queue")
                            .color(Color32::from_rgb(255, 110, 110)),
                    )
                    .clicked()
                {
                    job.cancel();
                }
            } else if ui
                .button(
                    RichText::new(format!(
                        "▶ Start Batch Processing ({} file(s))",
                        self.batch_items.len()
                    ))
                    .strong(),
                )
                .clicked()
            {
                if let Some(bins) = self.binaries.clone() {
                    for item in &mut self.batch_items {
                        item.status = BatchItemStatus::Pending;
                    }
                    match start_batch_queue(
                        bins,
                        self.batch_items.clone(),
                        self.filters.clone(),
                        self.export_settings.clone(),
                    ) {
                        Ok(handle) => {
                            self.batch_job = Some(handle);
                            self.set_status("Started batch processing queue...", false);
                        }
                        Err(e) => {
                            self.set_status(format!("Failed to start batch queue: {e}"), true);
                        }
                    }
                }
            }
        }
    }

    fn render_edl_panel(&mut self, ui: &mut Ui) {
        ui.heading("Edit Decision List (EDL)");
        ui.label(
            RichText::new(
                "Non-destructive timeline segments referencing underlying source media.",
            )
            .weak()
            .size(12.0),
        );
        ui.add_space(6.0);

        if self.project.segments.is_empty() {
            ui.label("No active segments.");
            return;
        }

        let mut v_cursor = 0u64;
        let mut jump_to = None;
        for (idx, seg) in self.project.segments.iter().enumerate() {
            let count = seg.frame_count();
            let v_start = v_cursor;
            let v_end = v_cursor + count;
            v_cursor = v_end;

            let src_name = self
                .project
                .source_by_id(seg.source_id)
                .and_then(|s| s.path.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("source");

            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Segment #{idx}")).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Seek").clicked() {
                            jump_to = Some(v_start);
                        }
                    });
                });
                ui.label(format!("Source: {src_name} (ID #{})", seg.source_id));
                ui.label(
                    RichText::new(format!(
                        "Source Frames: {}..{} ({} frames)",
                        seg.start_frame,
                        seg.end_frame.saturating_sub(1),
                        count
                    ))
                    .monospace()
                    .size(11.5),
                );
                ui.label(
                    RichText::new(format!(
                        "Timeline Frames: {}..{}",
                        v_start,
                        v_end.saturating_sub(1)
                    ))
                    .monospace()
                    .size(11.5)
                    .weak(),
                );
            });
        }

        if let Some(target) = jump_to {
            self.playback.pause();
            self.project.current_frame = target;
        }

        ui.add_space(12.0);
        ui.separator();
        ui.heading("Timeline Markers & Split Export");
        ui.label(
            RichText::new(
                "Press 'M' on the timeline to place bookmarks, or split & export the timeline into numbered sub-clips at every marker.",
            )
            .weak()
            .size(11.5),
        );
        ui.horizontal_wrapped(|ui| {
            if ui.button("🔖 Toggle Marker Here (M)").clicked() {
                self.project.toggle_marker_at_current();
            }
            if !self.scene_markers.is_empty()
                && ui
                    .button("🎬 Convert Detected Scene Cuts → Markers")
                    .clicked()
            {
                for (i, sm) in self.scene_markers.iter().enumerate() {
                    let f = self.project.virtual_timestamp_to_frame(sm.timestamp);
                    self.project
                        .add_or_update_marker(f, format!("Scene_{:02}", i + 1));
                }
            }
            if !self.project.markers.is_empty() && ui.small_button("Clear Markers").clicked() {
                self.project.markers.clear();
            }
        });

        if !self.project.markers.is_empty() {
            let mut seek_m = None;
            let mut del_m = None;
            for (idx, bm) in self.project.markers.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("#{} (F:{})", idx + 1, bm.frame))
                            .monospace()
                            .color(Color32::from_rgb(80, 235, 195)),
                    );
                    ui.text_edit_singleline(&mut bm.label);
                    if ui.small_button("Seek").clicked() {
                        seek_m = Some(bm.frame);
                    }
                    if ui.small_button("✖").clicked() {
                        del_m = Some(idx);
                    }
                });
            }
            if let Some(f) = seek_m {
                self.playback.pause();
                self.project.current_frame = f;
            }
            if let Some(i) = del_m {
                self.project.markers.remove(i);
            }

            let clip_ranges = self.project.split_clip_ranges();
            if clip_ranges.len() > 1 && self.export_job.is_none() {
                ui.add_space(4.0);
                if ui
                    .button(
                        RichText::new(format!(
                            "✂ Split & Export {} Sub-Clips by Markers...",
                            clip_ranges.len()
                        ))
                        .strong()
                        .color(Color32::from_rgb(130, 225, 165)),
                    )
                    .clicked()
                {
                    if let (Some(bins), Some(src)) =
                        (self.binaries.clone(), self.project.primary_source().cloned())
                    {
                        let ext = self.export_settings.container.extension();
                        let stem = src
                            .path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("clip");
                        if let Some(base_out) = rfd::FileDialog::new()
                            .set_file_name(format!("{stem}.{ext}"))
                            .add_filter("Video", &[ext])
                            .save_file()
                        {
                            match start_split_by_markers_export_job(
                                bins,
                                self.project.clone(),
                                self.filters.clone(),
                                self.export_settings.clone(),
                                base_out,
                            ) {
                                Ok(handle) => {
                                    self.export_job = Some(handle);
                                    self.set_status(
                                        format!(
                                            "Splitting & exporting {} sub-clips by markers...",
                                            clip_ranges.len()
                                        ),
                                        false,
                                    );
                                }
                                Err(e) => {
                                    self.set_status(format!("Split export failed: {e}"), true);
                                }
                            }
                        }
                    }
                }
            }
        }

        ui.add_space(12.0);
        ui.separator();
        ui.heading("Scene Change & Black-Frame Detector");
        ui.label(
            RichText::new(
                "Uses FFmpeg scdet & blackdetect on a fast 160×90 proxy stream to locate cuts and commercial fades.",
            )
            .weak()
            .size(11.5),
        );
        ui.add_space(4.0);
        ui.add(
            egui::Slider::new(&mut self.scene_threshold, 5.0..=35.0)
                .text("Scene Cut Threshold"),
        );
        ui.add(
            egui::Slider::new(&mut self.min_black_duration, 0.10..=2.0)
                .suffix(" s")
                .text("Min Black Duration"),
        );

        ui.horizontal(|ui| {
            let can_scan = self.project.is_loaded() && !self.is_scanning_scenes;
            if ui
                .add_enabled(
                    can_scan,
                    egui::Button::new("🔍 Scan Selection / 60s Window"),
                )
                .on_hover_text("Scan the active selection (or a 60s window around the playhead)")
                .clicked()
            {
                if let (Some(bins), Some(clip)) = (
                    self.binaries.clone(),
                    self.project.primary_source().cloned(),
                ) {
                    let (start_pts, dur_secs) = if let Some((s, e)) = self.project.selection_range()
                    {
                        let sp = self
                            .project
                            .resolve_frame(s)
                            .map(|r| r.source_pts)
                            .unwrap_or(TimestampUs::ZERO);
                        let ep = self
                            .project
                            .resolve_frame(e.saturating_sub(1))
                            .map(|r| r.source_pts)
                            .unwrap_or(TimestampUs(sp.0 + 60_000_000));
                        (sp, ((ep.0 - sp.0).max(1_000_000) as f64) / 1_000_000.0)
                    } else {
                        let cur = self
                            .project
                            .resolve_frame(self.project.current_frame)
                            .map(|r| r.source_pts)
                            .unwrap_or(TimestampUs::ZERO);
                        (cur, 60.0)
                    };
                    let thresh = self.scene_threshold;
                    let b_dur = self.min_black_duration;
                    let (tx, rx) = mpsc::channel();
                    self.scene_scan_rx = Some(rx);
                    self.is_scanning_scenes = true;
                    self.set_status("Scanning for scene cuts and black frames...", false);
                    thread::Builder::new()
                        .name("cutdub-scene-detector".into())
                        .spawn(move || {
                            if let Ok(markers) = detect_scenes_and_black_frames(
                                &bins,
                                &clip.path,
                                clip.video_stream_index,
                                start_pts,
                                dur_secs,
                                thresh,
                                b_dur,
                            ) {
                                let _ = tx.send(markers);
                            } else {
                                let _ = tx.send(Vec::new());
                            }
                        })
                        .ok();
                }
            }

            if !self.scene_markers.is_empty() && ui.small_button("Clear").clicked() {
                self.scene_markers.clear();
            }
        });

        if self.is_scanning_scenes {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Analyzing video stream for scene changes & black frames...");
            });
        } else if !self.scene_markers.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "Detected {} marker(s) (shown on timeline):",
                    self.scene_markers.len()
                ))
                .strong()
                .size(12.0),
            );
            let mut seek_ts = None;
            let mut mark_in_ts = None;
            let mut mark_out_ts = None;
            for (m_idx, marker) in self.scene_markers.iter().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        match marker.kind {
                            SceneMarkerKind::SceneCut { score } => {
                                ui.label(
                                    RichText::new(format!(
                                        "🎬 #{} Scene Cut (Score {:.1})",
                                        m_idx + 1,
                                        score
                                    ))
                                    .color(Color32::from_rgb(235, 130, 230))
                                    .strong(),
                                );
                            }
                            SceneMarkerKind::BlackSegment { duration_secs } => {
                                ui.label(
                                    RichText::new(format!(
                                        "⬛ #{} Black Segment ({:.2}s)",
                                        m_idx + 1,
                                        duration_secs
                                    ))
                                    .color(Color32::from_rgb(100, 220, 240))
                                    .strong(),
                                );
                            }
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("Out ]").clicked() {
                                mark_out_ts = Some(marker.timestamp);
                            }
                            if ui.small_button("[ In").clicked() {
                                mark_in_ts = Some(marker.timestamp);
                            }
                            if ui.small_button("Seek").clicked() {
                                seek_ts = Some(marker.timestamp);
                            }
                        });
                    });
                    let f_num = self.project.virtual_timestamp_to_frame(marker.timestamp);
                    ui.label(
                        RichText::new(format!("Frame #{f_num}  |  {}", marker.timestamp))
                            .monospace()
                            .size(11.0)
                            .weak(),
                    );
                });
            }
            if let Some(ts) = seek_ts {
                self.playback.pause();
                self.project.current_frame = self.project.virtual_timestamp_to_frame(ts);
            }
            if let Some(ts) = mark_in_ts {
                self.playback.pause();
                self.project.current_frame = self.project.virtual_timestamp_to_frame(ts);
                self.project.set_mark_in();
            }
            if let Some(ts) = mark_out_ts {
                self.playback.pause();
                self.project.current_frame = self.project.virtual_timestamp_to_frame(ts);
                self.project.set_mark_out();
            }
        }
    }

    fn render_dialogs(&mut self, ctx: &Context) {
        // 1. Go to Frame / Timestamp modal
        if self.show_goto_dialog {
            let mut open = self.show_goto_dialog;
            egui::Window::new("Go to Frame / Timestamp")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Frame Number:");
                        ui.text_edit_singleline(&mut self.goto_frame_input);
                        if ui.button("Jump to Frame").clicked() {
                            if let Ok(f) = self.goto_frame_input.trim().parse::<u64>() {
                                self.playback.pause();
                                self.project.current_frame =
                                    f.min(self.project.total_frames().saturating_sub(1));
                                self.show_goto_dialog = false;
                            }
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label("Timestamp (HH:MM:SS.mmm):");
                        ui.text_edit_singleline(&mut self.goto_time_input);
                        if ui.button("Jump to Time").clicked() {
                            if let Some(ts) = TimestampUs::parse_str(&self.goto_time_input) {
                                self.playback.pause();
                                self.project.current_frame =
                                    self.project.virtual_timestamp_to_frame(ts);
                                self.show_goto_dialog = false;
                            }
                        }
                    });
                });
            if !open {
                self.show_goto_dialog = false;
            }
        }

        // 2. Keyboard Shortcuts Dialog
        if self.show_shortcuts_dialog {
            let mut open = self.show_shortcuts_dialog;
            egui::Window::new("Keyboard Shortcuts Reference")
                .open(&mut open)
                .resizable(false)
                .show(ctx, |ui| {
                    egui::Grid::new("shortcuts_grid")
                        .num_columns(2)
                        .spacing([24.0, 6.0])
                        .striped(true)
                        .show(ui, |ui| {
                            let pairs = [
                                ("Space", "Play / Pause"),
                                ("J / K / L", "Shuttle: Slow down (J), Pause/1x (K), Speed up 0.25x..4x (L)"),
                                ("Left / Right Arrow", "Step backward / forward 1 frame"),
                                ("Shift + Left / Right (or PgUp/PgDn)", "Jump to previous / next keyframe"),
                                ("Alt + Left / Right", "Step backward / forward 10 frames"),
                                ("Home / End", "Jump to beginning / end of timeline"),
                                ("[ or I", "Set selection start (Mark In)"),
                                ("] or O", "Set selection end (Mark Out)"),
                                ("Delete / Backspace", "Delete selected frames from timeline"),
                                ("Cmd/Ctrl + T", "Trim timeline to selection"),
                                ("Cmd/Ctrl + Z", "Undo last edit operation"),
                                ("Cmd/Ctrl + Y (or Cmd+Shift+Z)", "Redo edit operation"),
                                ("Cmd/Ctrl + O", "Open video file"),
                                ("Cmd/Ctrl + E", "Export video"),
                                ("Cmd/Ctrl + Shift + S", "Save full-resolution frame snapshot (PNG/JPG)"),
                                ("Cmd/Ctrl + G", "Go to exact frame or timestamp"),
                                ("Cmd/Ctrl + F", "Open Video Filters panel"),
                            ];
                            for (k, desc) in pairs {
                                ui.label(RichText::new(k).monospace().strong());
                                ui.label(desc);
                                ui.end_row();
                            }
                        });
                });
            self.show_shortcuts_dialog = open;
        }

        // 3. FFmpeg Capabilities & Paths Dialog
        if self.show_ffmpeg_dialog {
            let mut open = self.show_ffmpeg_dialog;
            egui::Window::new("FFmpeg Toolchain & Capabilities")
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    if let Some(bins) = &self.binaries {
                        ui.label(RichText::new("Discovered Binaries:").strong());
                        ui.monospace(format!("ffmpeg:   {}", bins.ffmpeg_path.display()));
                        ui.monospace(format!("version:  {}", bins.ffmpeg_version));
                        ui.monospace(format!("ffprobe:  {}", bins.ffprobe_path.display()));
                        if let Some(vvenc_path) = &bins.vvencapp_path {
                            ui.monospace(format!("vvencapp: {}", vvenc_path.display()));
                            if let Some(vvenc_ver) = &bins.vvencapp_version {
                                ui.monospace(format!("vvc ver:  {vvenc_ver}"));
                            }
                        }
                        ui.separator();
                        ui.label(
                            RichText::new(format!(
                                "Hardware Accelerators: {}",
                                self.capabilities.hwaccels.join(", ")
                            ))
                            .strong(),
                        );
                        ui.label(format!(
                            "Detected {} video encoders and {} audio encoders.",
                            self.capabilities.video_encoders.len(),
                            self.capabilities.audio_encoders.len()
                        ));
                        ui.separator();
                        ui.label(RichText::new("Preview Frame Cache & Decoding Telemetry:").strong());
                        if let Some(sample) = &self.preview_latency {
                            ui.monospace(format!("Preview ready: {:.1} ms | wait {:.1} | filters {:.1} | scope {:.1} | texture prep {:.1}",
                                sample.total_ms, sample.wait_ms, sample.filters_ms,
                                sample.histogram_ms, sample.texture_ms));
                            ui.label("CPU timings include polling and preparation; GPU upload/presentation is not measured.");
                        }
                        if let Some(engine) = &self.preview_engine {
                            let c = &engine.cache;
                            ui.monospace(format!(
                                "Cached frames: {} / {}  ({:.1} MB)",
                                c.len(),
                                c.capacity(),
                                c.memory_bytes() as f64 / (1024.0 * 1024.0)
                            ));
                            ui.monospace(format!(
                                "Cache hits:    {}  |  Misses: {}  |  Hit rate: {:.1}%",
                                c.hits,
                                c.misses,
                                c.hit_rate_percent()
                            ));
                        }
                        if ui
                            .checkbox(
                                &mut self.settings.prefer_hardware_decode,
                                "Prefer Hardware-Accelerated Decoding (-hwaccel auto)",
                            )
                            .changed()
                        {
                            let _ = self.settings.save(&self.paths);
                        }
                    } else {
                        ui.label(
                            RichText::new("FFmpeg setup required")
                                .color(Color32::from_rgb(255, 110, 110)),
                        );
                        if let Some(error) = &self.toolchain_error {
                            ui.label(error);
                        }
                        ui.add_space(8.0);
                        ui.label("Install FFmpeg, including ffprobe, and restart CutDub.");
                        #[cfg(target_os = "macos")]
                        ui.monospace("brew install ffmpeg");
                        #[cfg(target_os = "linux")]
                        ui.label("On Debian / Ubuntu: sudo apt install ffmpeg");
                        #[cfg(target_os = "windows")]
                        ui.monospace("winget install Gyan.FFmpeg");
                        ui.add_space(8.0);
                        ui.label("Already installed? Make sure both executables are on PATH, or set their full paths before launching CutDub:");
                        ui.monospace("CUTDUB_FFMPEG=/path/to/ffmpeg");
                        ui.monospace("CUTDUB_FFPROBE=/path/to/ffprobe");
                        ui.label("If an executable was found but could not run, check its permissions and required libraries.");
                    }
                });
            self.show_ffmpeg_dialog = open;
        }

        // 4. About Dialog
        if self.show_about_dialog {
            let mut open = self.show_about_dialog;
            egui::Window::new("About CutDub")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.heading("CutDub (ReDub)");
                    ui.label("Modern Cross-Platform Precision Video Processor & VirtualDub2 Successor");
                    ui.add_space(6.0);
                    ui.label("Built with Rust, egui/eframe (wgpu), and FFmpeg.");
                    ui.label("Features frame-accurate VFR/CFR navigation, lossless Direct Stream Copy with keyframe analysis, non-destructive EDL editing, real-time video filters, and asynchronous export.");
                });
            self.show_about_dialog = open;
        }
    }
}

impl eframe::App for CutDubApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(ctx);
        self.handle_keyboard_shortcuts(ctx);
        self.poll_background_tasks(ctx);

        egui::TopBottomPanel::top("cutdub_top_bar").show(ctx, |ui| {
            self.render_menu_bar(ui);
            ui.separator();
            self.render_toolbar(ui);
        });

        egui::TopBottomPanel::bottom("cutdub_status_bar")
            .exact_height(26.0)
            .show(ctx, |ui| {
                self.render_status_bar(ui);
            });

        egui::TopBottomPanel::bottom("cutdub_timeline_panel")
            .min_height(88.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                self.render_timeline_and_transport(ui);
                ui.add_space(2.0);
            });

        if self.show_right_sidebar {
            egui::SidePanel::right("cutdub_right_sidebar")
                .default_width(350.0)
                .width_range(280.0..=520.0)
                .show(ctx, |ui| {
                    self.render_right_sidebar(ui);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            self.render_preview_area(ui);
        });

        self.render_dialogs(ctx);
    }
}

fn configure_dark_theme(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = Color32::from_rgb(24, 27, 34);
    visuals.panel_fill = Color32::from_rgb(24, 27, 34);
    visuals.extreme_bg_color = Color32::from_rgb(14, 16, 20);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(32, 36, 44);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 43, 54);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(52, 60, 76);
    visuals.widgets.active.bg_fill = Color32::from_rgb(65, 130, 215);
    visuals.selection.bg_fill = Color32::from_rgb(55, 125, 210);
    visuals.window_corner_radius = CornerRadius::same(6);
    ctx.set_visuals(visuals);
}

// OS key repeat can outrun decoding by hundreds of frames per second when each
// event jumps ten frames or a GOP. Never queue repeats behind an unseen image.
fn navigation_repeat_ready(events: &[egui::Event], preview_ready: bool) -> bool {
    preview_ready || !events.iter().any(|event| matches!(event,
        egui::Event::Key {
            key: Key::ArrowLeft | Key::ArrowRight | Key::PageUp | Key::PageDown,
            pressed: true, repeat: true, ..
        }
    ))
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    fn arrow_event(repeat: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key: Key::ArrowRight, physical_key: None,
            pressed: true, repeat, modifiers,
        }
    }

    #[test]
    fn held_navigation_waits_for_visible_target_without_blocking_fresh_presses() {
        for modifiers in [Modifiers::NONE, Modifiers::ALT, Modifiers::SHIFT] {
            let repeat = [arrow_event(true, modifiers)];
            assert!(!navigation_repeat_ready(&repeat, false));
            assert!(navigation_repeat_ready(&repeat, true));
            let press = [arrow_event(false, modifiers)];
            assert!(navigation_repeat_ready(&press, false));
        }
    }
}
