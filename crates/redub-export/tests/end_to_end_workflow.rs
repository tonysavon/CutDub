use redub_core::Project;
use redub_export::{
    start_export_job, ContainerFormat, ExportEvent, ExportSettings, VideoProcessingMode,
};
use redub_filters::{FilterKind, FilterPipeline};
use redub_media::{generate_synthetic_video, probe_media_file};
use redub_platform::FfmpegBinaries;
use tempfile::TempDir;

#[test]
fn test_end_to_end_multi_segment_edit_and_export() {
    let bins = FfmpegBinaries::discover(None, None).expect("FFmpeg must be installed");
    let tmp = TempDir::new().unwrap();

    let clip_a_path = tmp.path().join("clip_a.mp4");
    let clip_b_path = tmp.path().join("clip_b.mp4");
    generate_synthetic_video(&bins, &clip_a_path, 320, 240, 25, 1.0, 10).unwrap();
    generate_synthetic_video(&bins, &clip_b_path, 320, 240, 25, 1.0, 10).unwrap();

    let clip_a = probe_media_file(&bins, &clip_a_path, true).unwrap();
    let clip_b = probe_media_file(&bins, &clip_b_path, true).unwrap();

    let mut project = Project::from_primary_source(clip_a);
    assert_eq!(project.total_frames(), 25);

    project.append_source(clip_b).unwrap();
    assert_eq!(project.total_frames(), 50);

    // Delete 10 frames spanning the boundary between clip A and clip B (frames 20..=29)
    project.current_frame = 20;
    project.set_mark_in();
    project.current_frame = 29;
    project.set_mark_out();
    project.delete_selection().unwrap();
    assert_eq!(project.total_frames(), 40);

    // Verify project file save & load round-trip
    let proj_file = tmp.path().join("test.cutdub.json");
    project.save_to_file(&proj_file).unwrap();
    let reloaded = Project::load_from_file(&proj_file).unwrap();
    assert_eq!(reloaded.total_frames(), 40);
    assert_eq!(reloaded.segments.len(), 2);

    // Export edited multi-segment timeline with a brightness/contrast filter
    let mut filters = FilterPipeline::new();
    filters.add(FilterKind::BrightnessContrast {
        brightness: 0.05,
        contrast: 1.1,
        gamma: 1.0,
    });

    let out_path = tmp.path().join("edited_output.mp4");
    let settings = ExportSettings {
        processing_mode: VideoProcessingMode::FullProcessing,
        export_selection_only: false,
        container: ContainerFormat::Mp4,
        video_encoder: "libx264".into(),
        crf: 22,
        encoder_preset: "ultrafast".into(),
        ..ExportSettings::default()
    };

    let handle = start_export_job(bins.clone(), reloaded, filters, settings, out_path.clone()).unwrap();
    let mut completed = false;
    while let Ok(ev) = handle.receiver.recv() {
        match ev {
            ExportEvent::Completed { .. } => {
                completed = true;
                break;
            }
            ExportEvent::Failed(e) => panic!("Multi-segment export failed: {e}"),
            _ => {}
        }
    }
    assert!(completed);

    let exported_clip = probe_media_file(&bins, &out_path, true).unwrap();
    assert_eq!(exported_clip.width, 320);
    assert_eq!(exported_clip.height, 240);
    // Should be ~40 frames
    assert!((38..=42).contains(&exported_clip.timing.total_frames()));
}
