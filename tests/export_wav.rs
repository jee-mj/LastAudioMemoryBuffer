use lamb::dump::{FrameRange, SampleSnapshot};
use lamb::error::LambError;
use lamb::export_wav::{
    export_snapshot_wav, publish_dump, publish_recall, DumpPublishRequest, ExportRequest,
    RecallPublishRequest,
};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

const TIMESTAMP: &str = "20260630T073218";
const SPLIT_LIMIT: u64 = 3_900_000_000;

fn snapshot() -> SampleSnapshot {
    let ring = SampleRing::new(RingConfig {
        channels: 2,
        sample_rate: 10,
        format: SampleFormat::F32Le,
        chunk_frames: 4,
        chunk_count: 4,
        max_active_snapshots: 1,
    })
    .unwrap();
    let samples: Vec<f32> = (0..32).map(|value| (value as f32) / 32.0).collect();
    ring.write_interleaved(&samples, 2).unwrap();
    SampleSnapshot::from_ring_range(&ring, FrameRange { start: 0, end: 16 }).unwrap()
}

fn legacy_snapshot() -> lamb::sample_ring::Snapshot {
    let ring = SampleRing::new(RingConfig {
        channels: 2,
        sample_rate: 10,
        format: SampleFormat::F32Le,
        chunk_frames: 4,
        chunk_count: 4,
        max_active_snapshots: 1,
    })
    .unwrap();
    let samples: Vec<f32> = (0..32).map(|value| (value as f32) / 32.0).collect();
    ring.write_interleaved(&samples, 2).unwrap();
    ring.snapshot_last_frames(16).unwrap()
}

fn recall_name(channel: &str) -> String {
    recall_part_name(channel, 0, 16, 1)
}

fn recall_part_name(channel: &str, start: u64, end: u64, part: usize) -> String {
    format!("lamb-{TIMESTAMP}-{channel}-10Hz-{start:09}-{end:09}-part{part:03}.wav")
}

fn wav_frames(path: &Path) -> u32 {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 24);
    assert_eq!(&bytes[36..40], b"data");
    u32::from_le_bytes(bytes[40..44].try_into().unwrap()) / 3
}

fn assert_empty(path: &Path) {
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}

fn assert_export_error(error: LambError, expected: &str) {
    match error {
        LambError::Export(message) => assert!(
            message.contains(expected),
            "expected export error containing {expected:?}, got {message:?}"
        ),
        other => panic!("expected export error containing {expected:?}, got {other}"),
    }
}

#[test]
fn recall_publishes_flat_detailed_24_bit_wavs() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("recall");
    let staging = root.path().join("staging");
    fs::create_dir_all(&staging).unwrap();
    let names = vec!["left".to_string(), "right".to_string()];

    let published = publish_recall(RecallPublishRequest {
        snapshot: &snapshot(),
        output_dir: &output,
        staging_root: &staging,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap();

    let expected = vec![
        output.join(recall_name("left")),
        output.join(recall_name("right")),
    ];
    assert_eq!(published.output_directory, output);
    assert_eq!(published.files, expected);
    assert_eq!(wav_frames(&published.files[0]), 16);
    assert_eq!(wav_frames(&published.files[1]), 16);
    assert_empty(&staging);
}

#[test]
fn recall_splits_every_channel_on_the_same_frame_boundaries() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("recall");
    let staging = root.path().join("staging");
    fs::create_dir_all(&staging).unwrap();

    let published = publish_recall(RecallPublishRequest {
        snapshot: &snapshot(),
        output_dir: &output,
        staging_root: &staging,
        timestamp: TIMESTAMP,
        split_when_over_bytes: 80,
        channel_names: &[],
    })
    .unwrap();

    let frame_counts: Vec<_> = published
        .files
        .chunks_exact(2)
        .map(|channel_parts| {
            channel_parts
                .iter()
                .map(|path| wav_frames(path))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(frame_counts, vec![vec![12, 4], vec![12, 4]]);
    let basenames: Vec<_> = published
        .files
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap())
        .collect();
    assert_eq!(
        basenames,
        vec![
            "lamb-20260630T073218-ch01-10Hz-000000000-000000012-part001.wav",
            "lamb-20260630T073218-ch01-10Hz-000000012-000000016-part002.wav",
            "lamb-20260630T073218-ch02-10Hz-000000000-000000012-part001.wav",
            "lamb-20260630T073218-ch02-10Hz-000000012-000000016-part002.wav",
        ]
    );
    assert!(published
        .files
        .iter()
        .all(|path| path.extension().is_some_and(|ext| ext == "wav")));
}

#[test]
fn recall_collision_rolls_back_only_files_created_by_this_transaction() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("recall");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    fs::create_dir_all(&staging).unwrap();
    let names = vec!["left".to_string(), "right".to_string()];
    let first = output.join(recall_part_name("left", 0, 12, 1));
    let second = output.join(recall_part_name("left", 12, 16, 2));
    let collision = output.join(recall_part_name("right", 0, 12, 1));
    fs::write(&collision, b"pre-existing").unwrap();

    let error = publish_recall(RecallPublishRequest {
        snapshot: &snapshot(),
        output_dir: &output,
        staging_root: &staging,
        timestamp: TIMESTAMP,
        split_when_over_bytes: 80,
        channel_names: &names,
    })
    .unwrap_err();

    assert!(error.to_string().contains("I/O error"));
    assert!(!first.exists());
    assert!(!second.exists());
    assert_eq!(fs::read(&collision).unwrap(), b"pre-existing");
    assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    assert_empty(&staging);
}

#[test]
fn recall_rejects_traversal_channel_name_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("recall");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    fs::create_dir_all(&staging).unwrap();
    let names = vec!["x/../../recording".to_string(), "right".to_string()];

    let error = publish_recall(RecallPublishRequest {
        snapshot: &snapshot(),
        output_dir: &output,
        staging_root: &staging,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap_err();

    assert_export_error(error, "safe filename");
    assert_empty(&output);
    assert_empty(&staging);
}

#[test]
fn recall_rejects_duplicate_channel_names_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("recall");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    fs::create_dir_all(&staging).unwrap();
    let names = vec!["same".to_string(), "same".to_string()];

    let error = publish_recall(RecallPublishRequest {
        snapshot: &snapshot(),
        output_dir: &output,
        staging_root: &staging,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap_err();

    assert_export_error(error, "duplicate WAV filename");
    assert_empty(&output);
    assert_empty(&staging);
}

#[test]
fn dump_publishes_one_complete_timestamp_directory() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let names = vec!["left".to_string(), "right".to_string()];

    let published = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap();

    let final_dir = parent.join(TIMESTAMP);
    assert_eq!(published.output_directory, final_dir);
    assert_eq!(
        published.files,
        vec![final_dir.join("left.wav"), final_dir.join("right.wav")]
    );
    assert_eq!(wav_frames(&published.files[0]), 16);
    assert_eq!(wav_frames(&published.files[1]), 16);
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
}

#[test]
fn dump_collision_preserves_existing_directory_and_cleans_staging() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let final_dir = parent.join(TIMESTAMP);
    fs::create_dir_all(&final_dir).unwrap();
    fs::write(final_dir.join("marker"), b"pre-existing").unwrap();

    publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &[],
    })
    .unwrap_err();

    assert_eq!(fs::read(final_dir.join("marker")).unwrap(), b"pre-existing");
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
}

#[test]
fn dump_write_failure_exposes_no_final_or_temporary_directory() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let oversized_name = "x".repeat(300);
    let names = vec!["left".to_string(), oversized_name.clone()];

    let error = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap_err();

    match error {
        LambError::Io { path, source } => {
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                format!("{oversized_name}.wav")
            );
            assert_eq!(source.raw_os_error(), Some(libc::ENAMETOOLONG));
        }
        other => panic!("expected a later WAV create failure, got {other}"),
    }
    assert!(!parent.join(TIMESTAMP).exists());
    assert_empty(&parent);
}

#[test]
fn dump_rejects_traversal_timestamp_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let outside = root.path().join("outside");
    fs::create_dir_all(&parent).unwrap();

    let error = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: "../outside",
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &[],
    })
    .unwrap_err();

    assert_export_error(error, "safe filename");
    assert_empty(&parent);
    assert!(!outside.exists());
}

#[test]
fn dump_rejects_empty_timestamp_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    fs::create_dir_all(&parent).unwrap();

    let error = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: "",
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &[],
    })
    .unwrap_err();

    assert_export_error(error, "safe filename");
    assert_empty(&parent);
}

#[test]
fn dump_rejects_traversal_channel_name_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    fs::create_dir_all(&parent).unwrap();
    let names = vec!["../recording".to_string(), "right".to_string()];

    let error = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap_err();

    assert_export_error(error, "safe filename");
    assert_empty(&parent);
}

#[test]
fn dump_rejects_duplicate_channel_names_without_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    fs::create_dir_all(&parent).unwrap();
    let names = vec!["same".to_string(), "same".to_string()];

    let error = publish_dump(DumpPublishRequest {
        snapshot: &snapshot(),
        output_parent: &parent,
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
    })
    .unwrap_err();

    assert_export_error(error, "duplicate WAV filename");
    assert_empty(&parent);
}

#[test]
fn legacy_export_collision_preserves_existing_final_and_cleans_partial() {
    let output = tempfile::tempdir().unwrap();
    let names = vec!["left".to_string(), "right".to_string()];
    let final_path = output.path().join(recall_name("left"));
    let partial_path = final_path.with_extension("wav.partial");
    fs::write(&final_path, b"pre-existing").unwrap();

    export_snapshot_wav(ExportRequest {
        snapshot: &legacy_snapshot(),
        output_dir: output.path(),
        timestamp: TIMESTAMP,
        split_when_over_bytes: SPLIT_LIMIT,
        channel_names: &names,
        simple_names: false,
    })
    .unwrap_err();

    assert_eq!(fs::read(&final_path).unwrap(), b"pre-existing");
    assert!(!partial_path.exists());
    assert_eq!(fs::read_dir(output.path()).unwrap().count(), 1);
}

#[test]
fn concurrent_recalls_use_distinct_staging_transactions() {
    let root = tempfile::tempdir().unwrap();
    let staging = Arc::new(root.path().join("staging"));
    fs::create_dir_all(staging.as_ref()).unwrap();
    let root_path = root.path().to_path_buf();
    let handles: Vec<_> = (0..8)
        .map(|index| {
            let staging = Arc::clone(&staging);
            let output = root_path.join(format!("recall-{index}"));
            thread::spawn(move || {
                publish_recall(RecallPublishRequest {
                    snapshot: &snapshot(),
                    output_dir: &output,
                    staging_root: staging.as_ref(),
                    timestamp: TIMESTAMP,
                    split_when_over_bytes: SPLIT_LIMIT,
                    channel_names: &[],
                })
                .unwrap()
            })
        })
        .collect();

    for handle in handles {
        assert_eq!(handle.join().unwrap().files.len(), 2);
    }
    assert_empty(staging.as_ref());
}
