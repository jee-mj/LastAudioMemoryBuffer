use lamb::export_wav::{export_snapshot_wav, ExportRequest};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::fs;

fn snapshot() -> lamb::sample_ring::Snapshot {
    let ring = SampleRing::new(RingConfig {
        channels: 2,
        sample_rate: 10,
        format: SampleFormat::F32Le,
        chunk_frames: 4,
        chunk_count: 4,
        max_active_snapshots: 1,
    })
    .unwrap();
    let samples: Vec<f32> = (0..32).map(|v| (v as f32) / 32.0).collect();
    ring.write_interleaved(&samples, 2).unwrap();
    ring.snapshot_last_frames(16).unwrap()
}

#[test]
fn exports_one_wav_per_channel_with_valid_headers() {
    let dir = tempfile::tempdir().unwrap();
    let result = export_snapshot_wav(ExportRequest {
        snapshot: &snapshot(),
        output_dir: dir.path(),
        timestamp: "20260630T073218",
        split_when_over_bytes: 3_900_000_000,
        channel_names: &[],
        simple_names: false,
    })
    .unwrap();
    assert_eq!(result.files.len(), 2);
    let first = fs::read(&result.files[0]).unwrap();
    assert_eq!(&first[0..4], b"RIFF");
    assert_eq!(&first[8..12], b"WAVE");
    assert_eq!(&first[12..16], b"fmt ");
    assert_eq!(&first[36..40], b"data");
}

#[test]
fn splits_on_frame_boundaries_when_threshold_is_small() {
    let dir = tempfile::tempdir().unwrap();
    let result = export_snapshot_wav(ExportRequest {
        snapshot: &snapshot(),
        output_dir: dir.path(),
        timestamp: "20260630T073218",
        split_when_over_bytes: 80,
        channel_names: &[],
        simple_names: false,
    })
    .unwrap();
    assert!(result.files.len() > 2);
    for path in result.files {
        assert!(path.extension().is_none_or(|ext| ext != "partial"));
        assert!(path.exists());
    }
}
