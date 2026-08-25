use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use lamb::activity::{
    classify_frozen_epoch, ActivityDetectorKind, ChannelExportMode, DetectorWorkspace,
    FrozenExportDecision,
};
use lamb::app_config::ActivityThresholdConfig;
use lamb::capture_arena::{CaptureArena, CaptureIngress, CaptureRuntimeConfig};
use lamb::dump::{
    DecisionPreparation, DumpCoordinator, DumpOutcome, FrameRange, LossBreakdown, PublishedOutput,
    SampleSnapshot,
};
use lamb::error::{LambError, Result};
use lamb::export_policy::{ChannelActivityPolicy, ResolvedActivityPolicy};
use lamb::export_wav::{
    publish_dump, publish_prepared, publish_prepared_with_hook, DumpPublishRequest,
    PreparedPublication, PreparedPublicationHook, PublicationCheckpoint,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::persistence_workspace::{
    CleanupIo, PersistenceWorkspace, PersistenceWorkspaceConfig, PrepareRequest,
};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};

const DEADLINE: Duration = Duration::from_secs(2);
const TIMESTAMP_A: &str = "20260818T120000";
const TIMESTAMP_B: &str = "20260818T120001";

struct FrozenFixture {
    arena: CaptureArena,
    ingress: CaptureIngress,
    workspace: PersistenceWorkspace,
    coordinator: DumpCoordinator,
    root: tempfile::TempDir,
    names: Vec<String>,
    plan: SessionMemoryPlan,
}

impl FrozenFixture {
    fn new(retention_frames: u64, queue_slots: u32, slot_frames: u32) -> Self {
        Self::new_with_split_and_cleanup(
            retention_frames,
            queue_slots,
            slot_frames,
            1_000_000,
            None,
        )
    }

    fn new_with_split_and_cleanup(
        retention_frames: u64,
        queue_slots: u32,
        slot_frames: u32,
        split_when_over_bytes: u64,
        cleanup_io: Option<Box<dyn CleanupIo>>,
    ) -> Self {
        let channels = 1;
        let chunk_frames = 2;
        let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames,
            channels,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: queue_slots,
            capture_slot_frames: slot_frames,
            capture_worker_stack_bytes: 256 * 1024,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap();
        let (arena, ingress) = CaptureArena::new(
            &plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels,
                    sample_rate: 48_000,
                    format: SampleFormat::F32Le,
                    chunk_frames,
                    chunk_count: retention_frames.div_ceil(u64::from(chunk_frames)) as u32,
                    max_active_snapshots: 1,
                },
                queue_slots,
                slot_frames,
                sample_bytes: 4,
                worker_stack_bytes: 256 * 1024,
            },
        )
        .unwrap();
        let workspace_config = PersistenceWorkspaceConfig {
            retention_frames,
            channels,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames,
            sample_bytes: 4,
            split_when_over_bytes,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
        };
        let workspace = match cleanup_io {
            Some(cleanup_io) => {
                PersistenceWorkspace::new_with_cleanup_io(&plan, workspace_config, cleanup_io)
            }
            None => PersistenceWorkspace::new(&plan, workspace_config),
        }
        .unwrap();
        Self {
            arena,
            ingress,
            workspace,
            coordinator: DumpCoordinator::with_frozen_decision(
                FrozenExportDecision::new(&plan).unwrap(),
            ),
            root: tempfile::tempdir().unwrap(),
            names: vec!["mic".to_string()],
            plan,
        }
    }

    fn push(&self, samples: &[f32]) -> lamb::capture_arena::CapturePushOutcome {
        self.ingress.try_push_interleaved(samples, 1).unwrap()
    }

    fn recall(&mut self, timestamp: &str) -> Result<DumpOutcome> {
        self.coordinator.persist(
            &self.arena,
            &mut self.workspace,
            PrepareRequest::Recall {
                staging_root: &self.root.path().join("recall-staging"),
                output_dir: &self.root.path().join("recall"),
                timestamp,
                channel_names: &self.names,
            },
            DEADLINE,
        )
    }

    fn dump_preallocated(&mut self, timestamp: &str) -> Result<DumpOutcome> {
        self.coordinator.persist(
            &self.arena,
            &mut self.workspace,
            PrepareRequest::Dump {
                output_parent: &self.root.path().join("dumps"),
                timestamp,
                channel_names: &self.names,
            },
            DEADLINE,
        )
    }
}

fn exact_zero_policy(mode: ChannelExportMode) -> ResolvedActivityPolicy {
    ResolvedActivityPolicy {
        detector: ActivityDetectorKind::ExactZero,
        channels: vec![ChannelActivityPolicy {
            name: "mic".to_string(),
            mode,
            threshold: Some(ActivityThresholdConfig {
                threshold_dbfs: -3.0,
                threshold_source: lamb::activity::ThresholdSource::Manual,
                updated_at_unix_seconds: 0,
                input_id: "test".to_string(),
                calibration_id: None,
            }),
        }],
        whole_export_exact_zero_gate: false,
        trim_leading_silence: true,
    }
}

struct SecondRenameCollision;

impl PreparedPublicationHook for SecondRenameCollision {
    fn before_rename(&mut self, index: usize, final_path: &std::path::Path) -> Result<()> {
        if index == 1 {
            std::fs::write(final_path, b"foreign collision")
                .map_err(|error| LambError::Export(error.to_string()))?;
        }
        Ok(())
    }
}

struct InterruptAt {
    checkpoint: PublicationCheckpoint,
}

impl PreparedPublicationHook for InterruptAt {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> Result<()> {
        if checkpoint == self.checkpoint {
            return Err(LambError::Export(
                "injected publication interruption".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PublicationEvent {
    Checkpoint(PublicationCheckpoint),
    DirectorySync(PathBuf),
}

#[derive(Default)]
struct RecordingPublicationHook {
    events: Vec<PublicationEvent>,
}

impl PreparedPublicationHook for RecordingPublicationHook {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> Result<()> {
        self.events.push(PublicationEvent::Checkpoint(checkpoint));
        Ok(())
    }

    fn sync_directory(&mut self, path: &std::path::Path) -> Result<()> {
        lamb::recovery::sync_directory(path)?;
        self.events
            .push(PublicationEvent::DirectorySync(path.to_path_buf()));
        Ok(())
    }
}

struct FailDirectorySyncAt {
    call: usize,
    fail_at: usize,
}

impl PreparedPublicationHook for FailDirectorySyncAt {
    fn sync_directory(&mut self, path: &std::path::Path) -> Result<()> {
        self.call += 1;
        if self.call == self.fail_at {
            return Err(LambError::Export(
                "injected directory sync failure".to_string(),
            ));
        }
        lamb::recovery::sync_directory(path)
    }
}

struct ReplaceStagedAtComplete {
    staging_parent: PathBuf,
    replaced_path: Option<PathBuf>,
}

impl PreparedPublicationHook for ReplaceStagedAtComplete {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> Result<()> {
        if checkpoint != PublicationCheckpoint::RecallCompleteRecorded {
            return Ok(());
        }
        let transaction_root = std::fs::read_dir(&self.staging_parent)
            .map_err(|error| LambError::Export(error.to_string()))?
            .next()
            .ok_or_else(|| LambError::Export("missing recall transaction".to_string()))?
            .map_err(|error| LambError::Export(error.to_string()))?
            .path();
        let staged = std::fs::read_dir(&transaction_root)
            .map_err(|error| LambError::Export(error.to_string()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| LambError::Export(error.to_string()))?
            .into_iter()
            .find(|path| path.extension().is_some_and(|extension| extension == "wav"))
            .ok_or_else(|| LambError::Export("missing staged WAV".to_string()))?;
        let replacement = transaction_root.join("foreign-replacement");
        std::fs::write(&replacement, b"foreign staged inode")
            .map_err(|error| LambError::Export(error.to_string()))?;
        std::fs::rename(&replacement, &staged)
            .map_err(|error| LambError::Export(error.to_string()))?;
        self.replaced_path = Some(staged);
        Ok(())
    }
}

struct ReplaceDumpManifestAtComplete {
    output_parent: PathBuf,
    replacement_path: Option<PathBuf>,
}

impl PreparedPublicationHook for ReplaceDumpManifestAtComplete {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> Result<()> {
        if checkpoint != PublicationCheckpoint::DumpCompleteRecorded {
            return Ok(());
        }
        let manifest = std::fs::read_dir(&self.output_parent)
            .map_err(|error| LambError::Export(error.to_string()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|error| LambError::Export(error.to_string()))?
            .into_iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".manifest.json"))
            })
            .ok_or_else(|| LambError::Export("missing dump manifest".to_string()))?;
        let replacement = self.output_parent.join("foreign-manifest-replacement");
        std::fs::write(&replacement, b"foreign manifest inode")
            .map_err(|error| LambError::Export(error.to_string()))?;
        std::fs::rename(&replacement, &manifest)
            .map_err(|error| LambError::Export(error.to_string()))?;
        self.replacement_path = Some(manifest);
        Ok(())
    }
}

struct PublicationCleanupIo {
    fail_remove_file: Arc<AtomicBool>,
}

impl CleanupIo for PublicationCleanupIo {
    fn symlink_metadata(&mut self, path: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(path)
    }

    fn remove_dir_all(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn remove_file(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        if self.fail_remove_file.load(Ordering::Acquire) {
            return Err(std::io::Error::other(
                "injected publication rollback failure",
            ));
        }
        std::fs::remove_file(path)
    }
}

fn ring(channels: u32, chunk_frames: u32, chunk_count: u32) -> SampleRing {
    SampleRing::new(RingConfig {
        channels,
        sample_rate: 48_000,
        format: SampleFormat::F32Le,
        chunk_frames,
        chunk_count,
        max_active_snapshots: 1,
    })
    .unwrap()
}

fn owned_snapshot(samples: &[f32], channels: u32) -> SampleSnapshot {
    let frames = samples.len() as u64 / u64::from(channels);
    let ring = ring(channels, 2, 2);
    ring.write_interleaved(samples, channels).unwrap();
    SampleSnapshot::from_ring_range(
        &ring,
        FrameRange {
            start: 0,
            end: frames,
        },
    )
    .unwrap()
}

fn fake_publication(name: &str) -> PublishedOutput {
    PublishedOutput {
        output_directory: PathBuf::from(format!("/tmp/{name}")),
        files: vec![PathBuf::from(format!("/tmp/{name}/audio.wav"))],
    }
}

fn wav_s24_samples(path: &std::path::Path) -> Vec<i32> {
    let bytes = std::fs::read(path).unwrap();
    bytes[44..]
        .as_chunks::<3>()
        .0
        .iter()
        .map(|sample| {
            i32::from_le_bytes([
                sample[0],
                sample[1],
                sample[2],
                if sample[2] & 0x80 == 0 { 0 } else { 0xff },
            ])
        })
        .collect()
}

#[test]
fn all_zero_samples_are_digital_silence() {
    let snapshot = owned_snapshot(&[0.0, 0.0, 0.0, 0.0], 2);

    assert!(snapshot.is_digital_silence());
}

#[test]
fn negative_zero_is_digital_silence_but_one_nonzero_sample_is_not() {
    let silent = owned_snapshot(&[0.0, -0.0, 0.0, -0.0], 2);
    assert!(silent.is_digital_silence());

    let active = owned_snapshot(&[0.0, 0.0, 0.0, f32::MIN_POSITIVE], 2);
    assert!(!active.is_digital_silence());
}

#[test]
fn one_active_channel_makes_silent_peer_channels_non_silent() {
    let snapshot = owned_snapshot(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0], 3);

    assert!(!snapshot.is_digital_silence());
    assert_eq!(
        snapshot.channel_samples(),
        &[vec![0.0, 0.0], vec![1.0, 0.0], vec![0.0, 0.0]]
    );
}

#[test]
fn owned_snapshot_keeps_exact_samples_after_ring_wrap() {
    let ring = ring(2, 2, 2);
    let original = [1.0, 11.0, 2.0, 12.0, 3.0, 13.0, 4.0, 14.0];
    ring.write_interleaved(&original, 2).unwrap();

    let snapshot = SampleSnapshot::from_ring_range(&ring, FrameRange { start: 0, end: 4 }).unwrap();

    assert_eq!(snapshot.range(), FrameRange { start: 0, end: 4 });
    assert_eq!(snapshot.frames(), 4);
    assert_eq!(snapshot.channels(), 2);
    assert_eq!(snapshot.sample_rate(), 48_000);
    assert_eq!(
        snapshot.channel_samples(),
        &[vec![1.0, 2.0, 3.0, 4.0], vec![11.0, 12.0, 13.0, 14.0]]
    );

    let replacement = [21.0, 31.0, 22.0, 32.0, 23.0, 33.0, 24.0, 34.0];
    ring.write_interleaved(&replacement, 2).unwrap();

    assert_eq!(ring.status().dropped_frames, 0);
    assert_eq!(
        snapshot.channel_samples(),
        &[vec![1.0, 2.0, 3.0, 4.0], vec![11.0, 12.0, 13.0, 14.0]]
    );
}

#[test]
fn incremental_dumps_write_each_frame_once_then_report_no_new_audio() {
    let ring = ring(1, 2, 4);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0, 2.0, 3.0], 1).unwrap();

    let first = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 3 });
            Ok(fake_publication("a"))
        })
        .unwrap();
    assert_eq!(
        first,
        DumpOutcome::Written {
            range: FrameRange { start: 0, end: 3 },
            frames: 3,
            export_start_frame: 0,
            export_frames: 3,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            output_directory: PathBuf::from("/tmp/a"),
            files: vec![PathBuf::from("/tmp/a/audio.wav")],
        }
    );

    ring.write_interleaved(&[4.0, 5.0], 1).unwrap();
    let second = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 3, end: 5 });
            Ok(fake_publication("b"))
        })
        .unwrap();
    assert_eq!(
        second,
        DumpOutcome::Written {
            range: FrameRange { start: 3, end: 5 },
            frames: 2,
            export_start_frame: 3,
            export_frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            output_directory: PathBuf::from("/tmp/b"),
            files: vec![PathBuf::from("/tmp/b/audio.wav")],
        }
    );

    let no_new = coordinator
        .dump(&ring, |_| {
            panic!("publisher must not run without new audio")
        })
        .unwrap();
    assert_eq!(
        no_new,
        DumpOutcome::NoNewAudio {
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
        }
    );
}

#[test]
fn silence_is_committed_without_publication_and_sound_starts_after_it() {
    let ring = ring(1, 2, 4);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[0.0, -0.0], 1).unwrap();

    let silent = coordinator
        .dump(&ring, |_| panic!("publisher must not run for silence"))
        .unwrap();
    assert_eq!(
        silent,
        DumpOutcome::SkippedSilent {
            range: FrameRange { start: 0, end: 2 },
            frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
        }
    );

    ring.write_interleaved(&[1.0, 2.0], 1).unwrap();
    let active = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 2, end: 4 });
            Ok(fake_publication("active"))
        })
        .unwrap();
    assert!(matches!(
        active,
        DumpOutcome::Written {
            range: FrameRange { start: 2, end: 4 },
            frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn failed_publisher_retries_the_identical_range() {
    let ring = ring(1, 2, 4);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0, 2.0], 1).unwrap();
    coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 2 });
            assert_eq!(snapshot.channel_samples(), &[vec![1.0, 2.0]]);
            Ok(fake_publication("a"))
        })
        .unwrap();

    ring.write_interleaved(&[3.0, 4.0, 5.0], 1).unwrap();

    let error = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 2, end: 5 });
            assert_eq!(snapshot.channel_samples(), &[vec![3.0, 4.0, 5.0]]);
            Err(LambError::Export("publication failed".to_string()))
        })
        .unwrap_err();
    assert!(matches!(error, LambError::Export(message) if message == "publication failed"));

    let retry = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 2, end: 5 });
            assert_eq!(snapshot.channel_samples(), &[vec![3.0, 4.0, 5.0]]);
            Ok(fake_publication("retry"))
        })
        .unwrap();
    assert!(matches!(
        retry,
        DumpOutcome::Written {
            range: FrameRange { start: 2, end: 5 },
            frames: 3,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn snapshot_error_leaves_the_complete_range_retryable() {
    let ring = ring(1, 2, 4);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0, 2.0], 1).unwrap();
    coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 2 });
            assert_eq!(snapshot.channel_samples(), &[vec![1.0, 2.0]]);
            Ok(fake_publication("a"))
        })
        .unwrap();

    ring.write_interleaved(&[3.0, 4.0], 1).unwrap();
    let held_snapshot = ring.snapshot_range(2..4).unwrap();

    assert!(coordinator
        .dump(&ring, |_| panic!(
            "publisher must not run after snapshot error"
        ))
        .is_err());
    drop(held_snapshot);

    let retry = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 2, end: 4 });
            assert_eq!(snapshot.channel_samples(), &[vec![3.0, 4.0]]);
            Ok(fake_publication("snapshot-retry"))
        })
        .unwrap();
    assert!(matches!(
        retry,
        DumpOutcome::Written {
            range: FrameRange { start: 2, end: 4 },
            frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn capture_during_publish_belongs_to_the_next_transaction() {
    let ring = ring(1, 2, 4);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0, 2.0, 3.0], 1).unwrap();

    let first = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 3 });
            ring.write_interleaved(&[4.0, 5.0], 1).unwrap();
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 3 });
            Ok(fake_publication("a"))
        })
        .unwrap();
    assert!(matches!(
        first,
        DumpOutcome::Written {
            range: FrameRange { start: 0, end: 3 },
            ..
        }
    ));

    let second = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 3, end: 5 });
            Ok(fake_publication("b"))
        })
        .unwrap();
    assert!(matches!(
        second,
        DumpOutcome::Written {
            range: FrameRange { start: 3, end: 5 },
            frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn concurrent_dumps_serialize_selection_through_cursor_commit() {
    let ring = Arc::new(ring(1, 2, 4));
    let coordinator = Arc::new(DumpCoordinator::new());
    ring.write_interleaved(&[1.0, 2.0], 1).unwrap();
    let release_first = Arc::new(Barrier::new(2));
    let second_pre_call = Arc::new(Barrier::new(2));
    let publisher_calls = Arc::new(AtomicUsize::new(0));
    let (first_entered_tx, first_entered_rx) = mpsc::channel();

    let first_ring = Arc::clone(&ring);
    let first_coordinator = Arc::clone(&coordinator);
    let first_release = Arc::clone(&release_first);
    let first_publisher_calls = Arc::clone(&publisher_calls);
    let first = thread::spawn(move || {
        first_coordinator.dump(&first_ring, |snapshot| {
            first_publisher_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(snapshot.range(), FrameRange { start: 0, end: 2 });
            first_entered_tx.send(()).unwrap();
            first_release.wait();
            Ok(fake_publication("first"))
        })
    });
    first_entered_rx.recv().unwrap();

    let second_ring = Arc::clone(&ring);
    let second_coordinator = Arc::clone(&coordinator);
    let second_pre_call_thread = Arc::clone(&second_pre_call);
    let second_publisher_calls = Arc::clone(&publisher_calls);
    let (second_publisher_entered_tx, second_publisher_entered_rx) = mpsc::channel();
    let second = thread::spawn(move || {
        second_pre_call_thread.wait();
        second_coordinator.dump(&second_ring, |_| {
            second_publisher_entered_tx.send(()).unwrap();
            second_publisher_calls.fetch_add(1, Ordering::SeqCst);
            Ok(fake_publication("second"))
        })
    });

    second_pre_call.wait();
    let second_publisher_while_first_blocked =
        second_publisher_entered_rx.recv_timeout(Duration::from_millis(100));
    release_first.wait();
    let first_outcome = first.join().unwrap().unwrap();
    let second_outcome = second.join().unwrap().unwrap();
    assert!(matches!(
        second_publisher_while_first_blocked,
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected)
    ));
    assert!(matches!(first_outcome, DumpOutcome::Written { .. }));
    assert_eq!(
        second_outcome,
        DumpOutcome::NoNewAudio {
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
        }
    );
    assert_eq!(publisher_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn retention_loss_restarts_at_oldest_frame_and_reports_lost_count() {
    let ring = ring(1, 2, 2);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0, 1.0], 1).unwrap();
    coordinator
        .dump(&ring, |_| Ok(fake_publication("initial")))
        .unwrap();

    ring.write_interleaved(&[2.0, 3.0, 4.0, 5.0, 6.0, 7.0], 1)
        .unwrap();
    assert_eq!(ring.oldest_frame(), 4);
    let outcome = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 4, end: 8 });
            assert_eq!(snapshot.channel_samples(), &[vec![4.0, 5.0, 6.0, 7.0]]);
            Ok(fake_publication("after-loss"))
        })
        .unwrap();

    assert!(matches!(
        outcome,
        DumpOutcome::Written {
            range: FrameRange { start: 4, end: 8 },
            frames: 4,
            losses: LossBreakdown {
                retention_lost_frames: 2,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn partial_recycle_persists_the_contiguous_suffix_and_reports_chunk_loss() {
    let ring = ring(1, 4, 3);
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0], 1).unwrap();
    coordinator
        .dump(&ring, |_| Ok(fake_publication("initial")))
        .unwrap();

    ring.write_interleaved(
        &[
            2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0,
        ],
        1,
    )
    .unwrap();

    let outcome = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 4, end: 13 });
            assert_eq!(
                snapshot.channel_samples(),
                &[vec![5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0]]
            );
            Ok(fake_publication("contiguous-suffix"))
        })
        .unwrap();

    assert!(matches!(
        outcome,
        DumpOutcome::Written {
            range: FrameRange { start: 4, end: 13 },
            frames: 9,
            losses: LossBreakdown {
                retention_lost_frames: 3,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
}

#[test]
fn concurrent_capture_never_omits_or_duplicates_handled_ranges() {
    let ring = Arc::new(ring(1, 4, 3));
    let coordinator = DumpCoordinator::new();
    ring.write_interleaved(&[1.0], 1).unwrap();
    coordinator
        .dump(&ring, |_| Ok(fake_publication("seed")))
        .unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let writer_ring = Arc::clone(&ring);
    let writer_done = Arc::clone(&done);
    let writer = thread::spawn(move || {
        for _ in 0..100_000 {
            writer_ring.write_interleaved(&[1.0], 1).unwrap();
        }
        writer_done.store(true, Ordering::Release);
    });

    let mut handled_until = 1;
    loop {
        let outcome = coordinator
            .dump(&ring, |_| Ok(fake_publication("concurrent")))
            .unwrap();
        match outcome {
            DumpOutcome::Written { range, losses, .. } => {
                assert_eq!(range.start, handled_until + losses.lost_frames());
                handled_until = range.end;
            }
            DumpOutcome::NoNewAudio { .. } if done.load(Ordering::Acquire) => break,
            DumpOutcome::NoNewAudio { .. } => thread::yield_now(),
            DumpOutcome::SkippedSilent { .. } | DumpOutcome::SkippedByPolicy { .. } => {
                panic!("writer only captures nonzero samples")
            }
        }
    }

    writer.join().unwrap();
    assert_eq!(handled_until, ring.write_head_frame());
}

#[test]
fn real_publisher_collision_keeps_b_retryable_without_rewriting_a() {
    let output = tempfile::tempdir().unwrap();
    let ring = ring(1, 4, 3);
    let coordinator = DumpCoordinator::new();
    let channel_names = vec!["mic".to_string()];
    let timestamp_a = "20260818T120000";
    let timestamp_b = "20260818T120001";

    ring.write_interleaved(&[0.25, 0.25], 1).unwrap();
    coordinator
        .dump(&ring, |snapshot| {
            publish_dump(DumpPublishRequest {
                snapshot,
                output_parent: output.path(),
                timestamp: timestamp_a,
                split_when_over_bytes: u64::MAX,
                channel_names: &channel_names,
            })
        })
        .unwrap();
    let a_path = output.path().join(timestamp_a).join("mic.wav");
    let a_samples = wav_s24_samples(&a_path);

    ring.write_interleaved(&[0.5, -0.5], 1).unwrap();
    let collision = coordinator.dump(&ring, |snapshot| {
        assert_eq!(snapshot.range(), FrameRange { start: 2, end: 4 });
        publish_dump(DumpPublishRequest {
            snapshot,
            output_parent: output.path(),
            timestamp: timestamp_a,
            split_when_over_bytes: u64::MAX,
            channel_names: &channel_names,
        })
    });
    assert!(collision.is_err());
    assert_eq!(wav_s24_samples(&a_path), a_samples);

    let retry = coordinator
        .dump(&ring, |snapshot| {
            assert_eq!(snapshot.range(), FrameRange { start: 2, end: 4 });
            assert_eq!(snapshot.channel_samples(), &[vec![0.5, -0.5]]);
            publish_dump(DumpPublishRequest {
                snapshot,
                output_parent: output.path(),
                timestamp: timestamp_b,
                split_when_over_bytes: u64::MAX,
                channel_names: &channel_names,
            })
        })
        .unwrap();

    assert!(matches!(
        retry,
        DumpOutcome::Written {
            range: FrameRange { start: 2, end: 4 },
            frames: 2,
            losses: LossBreakdown {
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            },
            ..
        }
    ));
    assert_eq!(
        wav_s24_samples(&output.path().join(timestamp_b).join("mic.wav")),
        vec![4_194_304, -4_194_304]
    );
}

#[test]
fn frozen_recall_and_dump_share_one_cursor_for_a_b_then_no_new_audio() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.25, 0.5]);
    let a = fixture.recall(TIMESTAMP_A).unwrap();
    assert_eq!(a.range(), Some(FrameRange { start: 0, end: 2 }));

    fixture.push(&[0.75, 1.0]);
    let b = fixture.dump_preallocated(TIMESTAMP_B).unwrap();
    assert_eq!(b.range(), Some(FrameRange { start: 2, end: 4 }));
    assert_eq!(fixture.recall("20260818T120002").unwrap().range(), None);
}

#[test]
fn first_persistence_after_wrap_reports_loss_from_absolute_origin_once() {
    let mut fixture = FrozenFixture::new(4, 8, 8);
    fixture.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    let first = fixture.recall(TIMESTAMP_A).unwrap();

    assert_eq!(first.range(), Some(FrameRange { start: 2, end: 6 }));
    assert_eq!(first.losses().retention_lost_frames, 2);
    assert_eq!(
        fixture.recall(TIMESTAMP_B).unwrap().losses().lost_frames(),
        0
    );
}

#[test]
fn first_clear_after_wrap_reports_retention_and_recoverable_frames_once() {
    let mut fixture = FrozenFixture::new(4, 8, 8);
    fixture.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    fixture
        .coordinator
        .clear_in_order(&fixture.arena, DEADLINE)
        .unwrap();

    let first = fixture.recall(TIMESTAMP_A).unwrap();
    assert_eq!(first.range(), None);
    assert_eq!(first.losses().retention_lost_frames, 2);
    assert_eq!(first.losses().cleared_frames, 4);
    assert_eq!(
        fixture.recall(TIMESTAMP_B).unwrap().losses().lost_frames(),
        0
    );
}

#[test]
fn different_arena_is_rejected_before_preparation_or_publication() {
    let mut first = FrozenFixture::new(8, 1, 4);
    let pushed = first.push(&[0.25; 7]);
    assert_eq!(pushed.dropped_frames, 3);
    assert_eq!(
        first
            .recall(TIMESTAMP_A)
            .unwrap()
            .losses()
            .capture_dropped_frames,
        3
    );

    let mut other = FrozenFixture::new(8, 8, 4);
    assert_ne!(first.arena.runtime_id(), other.arena.runtime_id());
    other.push(&[0.5]);
    let output_parent = other.root.path().join("must-not-publish");
    let publisher_called = AtomicBool::new(false);

    let error = first
        .coordinator
        .persist_with_publisher(
            &other.arena,
            &mut other.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_B,
                channel_names: &other.names,
            },
            DEADLINE,
            DEADLINE,
            |_| {
                publisher_called.store(true, Ordering::Release);
                PreparedPublication::Published(fake_publication("wrong-arena"))
            },
        )
        .unwrap_err();

    assert!(matches!(
        error,
        LambError::ControlInvariant("dump coordinator belongs to a different capture runtime")
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
    assert!(!output_parent.exists());
}

#[test]
fn exact_silent_frozen_epoch_commits_without_publication() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.0, -0.0]);

    let silent = fixture.dump_preallocated(TIMESTAMP_A).unwrap();

    assert!(matches!(
        silent,
        DumpOutcome::SkippedSilent {
            range: FrameRange { start: 0, end: 2 },
            ..
        }
    ));
    assert!(!fixture.root.path().join("dumps").join(TIMESTAMP_A).exists());
    assert_eq!(
        fixture.dump_preallocated(TIMESTAMP_B).unwrap().range(),
        None
    );
}

#[test]
fn retry_preserves_the_classified_frozen_decision_when_policy_changes() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.0, 0.75, 0.5]);
    let first_policy = exact_zero_policy(ChannelExportMode::Always);
    let changed_policy = exact_zero_policy(ChannelExportMode::Never);
    assert_ne!(first_policy, changed_policy);
    let mut detector = DetectorWorkspace::new(&fixture.plan).unwrap();
    let mut first_decision = None;

    let error = fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                assert!(!decision.valid());
                classify_frozen_epoch(frozen, &first_policy, &mut detector, decision)?;
                first_decision = Some((
                    decision.export_range(),
                    decision.channels().to_vec(),
                    decision.storage_id(),
                ));
                Ok(DecisionPreparation::Continue)
            },
            |_| PreparedPublication::RetryableFailure(LambError::Export("retry".to_string())),
        )
        .unwrap_err();
    assert!(matches!(error, LambError::Export(message) if message == "retry"));

    let mut retry_decision = None;
    let retry = fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_B,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |_, decision| {
                assert!(decision.valid());
                retry_decision = Some((
                    decision.export_range(),
                    decision.channels().to_vec(),
                    decision.storage_id(),
                ));
                // A changed retry policy must not classify an already frozen decision.
                assert_eq!(changed_policy.channels[0].mode, ChannelExportMode::Never);
                Ok(DecisionPreparation::Continue)
            },
            |_| PreparedPublication::Published(fake_publication("retry")),
        )
        .unwrap();

    assert_eq!(retry.range(), Some(FrameRange { start: 0, end: 3 }));
    assert_eq!(retry_decision, first_decision);
}

#[test]
fn successful_completion_recycles_the_same_reset_decision_for_the_next_range() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.5]);
    let mut first_storage = None;
    fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                assert!(!decision.valid());
                decision.finalize(
                    frozen.absolute_range(),
                    &[lamb::activity::FrozenChannelDecision::retained(
                        ChannelExportMode::Always,
                        lamb::activity::ActivityResult::Active,
                        Some(frozen.absolute_range().start),
                    )],
                    false,
                    false,
                )?;
                first_storage = Some((decision.storage_id(), decision.channels().len()));
                Ok(DecisionPreparation::Continue)
            },
            |_| PreparedPublication::Published(fake_publication("first")),
        )
        .unwrap();

    fixture.push(&[0.25]);
    let mut second_storage = None;
    let second = fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_B,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                assert!(!decision.valid());
                assert_eq!(decision.export_range(), 0..0);
                second_storage = Some((decision.storage_id(), decision.channels().len()));
                decision.finalize(
                    frozen.absolute_range(),
                    &[lamb::activity::FrozenChannelDecision::retained(
                        ChannelExportMode::Always,
                        lamb::activity::ActivityResult::Active,
                        Some(frozen.absolute_range().start),
                    )],
                    false,
                    false,
                )?;
                Ok(DecisionPreparation::Continue)
            },
            |_| PreparedPublication::Published(fake_publication("second")),
        )
        .unwrap();

    assert_eq!(second.range(), Some(FrameRange { start: 1, end: 2 }));
    assert_eq!(second_storage, first_storage);
}

#[test]
fn only_all_never_decisions_skip_by_policy_without_preparing_or_publishing() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.75]);
    let publisher_called = AtomicBool::new(false);

    let outcome = fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                decision.finalize(
                    frozen.absolute_range(),
                    &[lamb::activity::FrozenChannelDecision {
                        mode: ChannelExportMode::Never,
                        result: lamb::activity::ActivityResult::Inactive,
                        disposition: lamb::activity::ChannelDisposition::Omit,
                        first_evidence_frame: None,
                    }],
                    false,
                    false,
                )?;
                Ok(DecisionPreparation::Continue)
            },
            |_| {
                publisher_called.store(true, Ordering::Release);
                PreparedPublication::Published(fake_publication("must-not-publish"))
            },
        )
        .unwrap();

    assert!(matches!(
        outcome,
        DumpOutcome::SkippedByPolicy {
            range: FrameRange { start: 0, end: 1 },
            ..
        }
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
}

#[test]
fn inactive_auto_decision_is_a_silent_skip_not_a_policy_skip() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.75]);
    let publisher_called = AtomicBool::new(false);

    let outcome = fixture
        .coordinator
        .persist_with_decision_preparation_and_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                decision.finalize(
                    frozen.absolute_range(),
                    &[lamb::activity::FrozenChannelDecision {
                        mode: ChannelExportMode::Auto,
                        result: lamb::activity::ActivityResult::Inactive,
                        disposition: lamb::activity::ChannelDisposition::Omit,
                        first_evidence_frame: None,
                    }],
                    false,
                    false,
                )?;
                Ok(DecisionPreparation::SkippedSilent)
            },
            |_| {
                publisher_called.store(true, Ordering::Release);
                PreparedPublication::Published(fake_publication("must-not-publish"))
            },
        )
        .unwrap();

    assert!(matches!(
        outcome,
        DumpOutcome::SkippedSilent {
            range: FrameRange { start: 0, end: 1 },
            ..
        }
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
}

#[test]
fn never_plus_inactive_auto_decision_is_a_silent_skip_not_a_policy_skip() {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 8,
        channels: 2,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 2,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 8,
        capture_slot_frames: 4,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: 4 * 1024,
        maximum_path_bytes: 512,
        maximum_calibration_seconds: 0,
        headroom: 1.0,
    })
    .unwrap();
    let (arena, ingress) = CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: 2,
                sample_rate: 48_000,
                format: SampleFormat::F32Le,
                chunk_frames: 2,
                chunk_count: 4,
                max_active_snapshots: 1,
            },
            queue_slots: 8,
            slot_frames: 4,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        },
    )
    .unwrap();
    let mut workspace = PersistenceWorkspace::new(
        &plan,
        PersistenceWorkspaceConfig {
            retention_frames: 8,
            channels: 2,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
        },
    )
    .unwrap();
    ingress.try_push_interleaved(&[0.75, 0.5], 2).unwrap();
    let coordinator =
        DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
    let publisher_called = AtomicBool::new(false);
    let root = tempfile::tempdir().unwrap();
    let names = vec!["never".to_string(), "auto".to_string()];

    let outcome = coordinator
        .persist_with_decision_preparation_and_publisher(
            &arena,
            &mut workspace,
            PrepareRequest::Dump {
                output_parent: &root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |frozen, decision| {
                decision.finalize(
                    frozen.absolute_range(),
                    &[
                        lamb::activity::FrozenChannelDecision {
                            mode: ChannelExportMode::Never,
                            result: lamb::activity::ActivityResult::Inactive,
                            disposition: lamb::activity::ChannelDisposition::Omit,
                            first_evidence_frame: None,
                        },
                        lamb::activity::FrozenChannelDecision {
                            mode: ChannelExportMode::Auto,
                            result: lamb::activity::ActivityResult::Inactive,
                            disposition: lamb::activity::ChannelDisposition::Omit,
                            first_evidence_frame: None,
                        },
                    ],
                    false,
                    false,
                )?;
                Ok(DecisionPreparation::SkippedSilent)
            },
            |_| {
                publisher_called.store(true, Ordering::Release);
                PreparedPublication::Published(fake_publication("must-not-publish"))
            },
        )
        .unwrap();

    assert!(matches!(
        outcome,
        DumpOutcome::SkippedSilent {
            range: FrameRange { start: 0, end: 1 },
            ..
        }
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
}

#[test]
fn decisionless_persistence_rejects_before_freezing_and_preserves_audio_for_a_planned_coordinator()
{
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.75]);
    let decisionless = DumpCoordinator::new();
    let error = decisionless
        .persist(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &fixture.root.path().join("dumps"),
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        LambError::ControlInvariant("persistence coordinator has no reusable frozen decision")
    ));
    assert!(!fixture.arena.status(DEADLINE).unwrap().frozen_pending);

    let recovered = fixture.dump_preallocated(TIMESTAMP_B).unwrap();
    assert_eq!(recovered.range(), Some(FrameRange { start: 0, end: 1 }));
}

#[test]
fn failed_publication_retains_exact_frozen_range_while_active_capture_wraps() {
    let mut fixture = FrozenFixture::new(4, 16, 4);
    fixture.push(&[0.1, 0.2]);
    fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    fixture.push(&[0.3, 0.4]);
    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_err());

    fixture.push(&[1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7]);
    let retry = fixture.dump_preallocated(TIMESTAMP_B).unwrap();
    assert_eq!(retry.range(), Some(FrameRange { start: 2, end: 4 }));

    let newer = fixture.recall("20260818T120002").unwrap();
    assert_eq!(newer.range(), Some(FrameRange { start: 8, end: 12 }));
    assert_eq!(newer.losses().retention_lost_frames, 4);
}

#[test]
fn successful_publication_with_release_timeout_is_not_republished() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.25, 0.5]);
    let output_parent = fixture.root.path().join("dumps");
    let first = fixture
        .coordinator
        .persist_with_release_timeout(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            Duration::ZERO,
        )
        .unwrap();
    assert_eq!(first.range(), Some(FrameRange { start: 0, end: 2 }));

    let next = fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    assert_eq!(next.range(), None);
    assert_eq!(std::fs::read_dir(output_parent).unwrap().count(), 1);
}

#[test]
fn committed_clear_with_release_timeout_never_reclassifies_cleared_frozen_frames() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.25, 0.5]);
    fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    fixture.push(&[0.75, 1.0]);
    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_err());

    fixture
        .coordinator
        .clear_in_order_with_release_timeout(&fixture.arena, DEADLINE, Duration::ZERO)
        .unwrap();

    let outcome = fixture.recall(TIMESTAMP_B).unwrap();
    assert_eq!(outcome.range(), None);
    assert_eq!(outcome.losses().cleared_frames, 2);
    assert_eq!(
        fixture
            .recall("20260818T120002")
            .unwrap()
            .losses()
            .lost_frames(),
        0
    );
}

#[test]
fn retention_clear_and_capture_drop_causes_are_separate_and_total_saturates() {
    let mut retention = FrozenFixture::new(4, 8, 8);
    retention.push(&[1.0, 1.0]);
    retention.recall(TIMESTAMP_A).unwrap();
    retention.push(&[2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
    let retention_outcome = retention.recall(TIMESTAMP_B).unwrap();
    assert_eq!(retention_outcome.losses().retention_lost_frames, 2);
    assert_eq!(retention_outcome.losses().cleared_frames, 0);
    assert_eq!(retention_outcome.losses().capture_dropped_frames, 0);

    let mut cleared = FrozenFixture::new(8, 8, 4);
    cleared.push(&[1.0, 2.0, 3.0]);
    cleared
        .coordinator
        .clear_in_order(&cleared.arena, DEADLINE)
        .unwrap();
    assert_eq!(
        cleared.recall(TIMESTAMP_A).unwrap().losses().cleared_frames,
        3
    );

    let mut dropped = FrozenFixture::new(8, 1, 4);
    let pushed = dropped.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
    assert_eq!(pushed.dropped_frames, 3);
    assert_eq!(dropped.arena.cumulative_capture_dropped_frames(), 3);
    assert_eq!(
        dropped
            .recall(TIMESTAMP_A)
            .unwrap()
            .losses()
            .capture_dropped_frames,
        3
    );

    let saturated = LossBreakdown {
        retention_lost_frames: u64::MAX,
        cleared_frames: 1,
        capture_dropped_frames: 1,
    };
    assert_eq!(saturated.lost_frames(), u64::MAX);
}

#[test]
fn no_new_audio_acknowledges_clear_and_capture_drops_once() {
    let mut fixture = FrozenFixture::new(8, 1, 5);
    let pushed = fixture.push(&[1.0; 12]);
    assert_eq!(pushed.enqueued_frames, 5);
    assert_eq!(pushed.dropped_frames, 7);
    fixture
        .coordinator
        .clear_in_order(&fixture.arena, DEADLINE)
        .unwrap();

    let first = fixture.recall(TIMESTAMP_A).unwrap();
    assert_eq!(first.range(), None);
    assert_eq!(first.losses().cleared_frames, 5);
    assert_eq!(first.losses().capture_dropped_frames, 7);
    assert_eq!(first.losses().lost_frames(), 12);
    assert_eq!(
        fixture.recall(TIMESTAMP_B).unwrap().losses().lost_frames(),
        0
    );
}

#[test]
fn failed_publication_acknowledges_no_loss() {
    let mut fixture = FrozenFixture::new(8, 1, 2);
    fixture.push(&[0.25]);
    fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    let pushed = fixture.push(&[0.5, 0.75, 1.0]);
    assert_eq!(pushed.dropped_frames, 1);
    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_err());

    let retry = fixture.dump_preallocated(TIMESTAMP_B).unwrap();
    assert_eq!(retry.range(), Some(FrameRange { start: 1, end: 3 }));
    assert_eq!(retry.losses().capture_dropped_frames, 1);
}

#[test]
fn clear_counts_frozen_and_queue_boundary_audio_without_reporting_early() {
    let mut fixture = FrozenFixture::new(16, 8, 4);
    fixture.push(&[0.25, 0.5]);
    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_ok());
    fixture.push(&[0.75, 1.0]);
    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_err());
    fixture.push(&[1.25, 1.5, 1.75]);

    fixture
        .coordinator
        .clear_in_order(&fixture.arena, DEADLINE)
        .unwrap();
    let after_clear = fixture.recall(TIMESTAMP_B).unwrap();
    assert_eq!(after_clear.range(), None);
    assert_eq!(after_clear.losses().cleared_frames, 5);
}

#[test]
fn clear_without_frozen_processes_exactly_through_submitted_queue_boundary() {
    let mut fixture = FrozenFixture::new(16, 8, 4);
    fixture.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);

    fixture
        .coordinator
        .clear_in_order(&fixture.arena, DEADLINE)
        .unwrap();

    let outcome = fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    assert_eq!(outcome.range(), None);
    assert_eq!(outcome.losses().cleared_frames, 5);
}

#[test]
fn older_persistence_json_defaults_new_loss_causes() {
    let old_written = serde_json::json!({
        "kind": "written",
        "start_frame": 1,
        "end_frame": 3,
        "frames": 2,
        "duration_seconds": 0.5,
        "lost_frames": 9,
        "output_directory": "/tmp/out",
        "files": ["/tmp/out/mic.wav"]
    });
    let written: lamb::control::PersistenceOutcomeResponse =
        serde_json::from_value(old_written).unwrap();
    assert!(matches!(
        written,
        lamb::control::PersistenceOutcomeResponse::Written {
            lost_frames: 9,
            retention_lost_frames: 0,
            cleared_frames: 0,
            capture_dropped_frames: 0,
            ..
        }
    ));

    let no_new: lamb::control::PersistenceOutcomeResponse =
        serde_json::from_value(serde_json::json!({ "kind": "no_new_audio" })).unwrap();
    assert!(matches!(
        no_new,
        lamb::control::PersistenceOutcomeResponse::NoNewAudio {
            lost_frames: 0,
            retention_lost_frames: 0,
            cleared_frames: 0,
            capture_dropped_frames: 0,
        }
    ));
}

#[test]
fn same_second_collision_retries_old_frozen_range_with_new_timestamp() {
    let mut fixture = FrozenFixture::new(8, 8, 4);
    fixture.push(&[0.25]);
    fixture.dump_preallocated(TIMESTAMP_A).unwrap();
    fixture.push(&[0.5, 0.75]);

    assert!(fixture.dump_preallocated(TIMESTAMP_A).is_err());
    let retry = fixture.dump_preallocated(TIMESTAMP_B).unwrap();

    assert_eq!(retry.range(), Some(FrameRange { start: 1, end: 3 }));
    assert!(fixture
        .root
        .path()
        .join("dumps")
        .join(TIMESTAMP_B)
        .join("mic.wav")
        .exists());
}

#[test]
fn later_recall_rename_collision_rolls_back_and_allows_exact_retry() {
    let mut fixture = FrozenFixture::new_with_split_and_cleanup(8, 8, 4, 50, None);
    fixture.push(&[0.25, 0.5, 0.75, 1.0]);
    let output = fixture.root.path().join("recall");
    let staging = fixture.root.path().join("recall-staging");
    let first_final =
        output.join("lamb-20260818T120000-mic-48000Hz-000000000-000000002-part001.wav");
    let second_final =
        output.join("lamb-20260818T120000-mic-48000Hz-000000002-000000004-part002.wav");
    let mut hook = SecondRenameCollision;

    let first = fixture.coordinator.persist_with_publisher(
        &fixture.arena,
        &mut fixture.workspace,
        PrepareRequest::Recall {
            staging_root: &staging,
            output_dir: &output,
            timestamp: TIMESTAMP_A,
            channel_names: &fixture.names,
        },
        DEADLINE,
        DEADLINE,
        |prepared| publish_prepared_with_hook(prepared, &mut hook),
    );

    assert!(first.is_err());
    assert!(!first_final.exists());
    assert_eq!(std::fs::read(&second_final).unwrap(), b"foreign collision");
    std::fs::remove_file(&second_final).unwrap();
    let retry = fixture.recall(TIMESTAMP_A).unwrap();
    assert_eq!(retry.range(), Some(FrameRange { start: 0, end: 4 }));
    assert!(first_final.exists());
    assert!(second_final.exists());
}

#[test]
fn uncertain_recall_rollback_blocks_reencoding_until_cleanup_recovers() {
    let fail_remove_file = Arc::new(AtomicBool::new(true));
    let cleanup_io = PublicationCleanupIo {
        fail_remove_file: Arc::clone(&fail_remove_file),
    };
    let mut fixture =
        FrozenFixture::new_with_split_and_cleanup(8, 8, 4, 50, Some(Box::new(cleanup_io)));
    fixture.push(&[0.25, 0.5, 0.75, 1.0]);
    let output = fixture.root.path().join("recall");
    let staging = fixture.root.path().join("recall-staging");
    let first_final =
        output.join("lamb-20260818T120000-mic-48000Hz-000000000-000000002-part001.wav");
    let second_final =
        output.join("lamb-20260818T120000-mic-48000Hz-000000002-000000004-part002.wav");
    let mut hook = SecondRenameCollision;

    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP_A,
                channel_names: &fixture.names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut hook),
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));
    assert!(first_final.exists());
    assert!(second_final.exists());
    let transaction_roots: Vec<_> = std::fs::read_dir(&staging)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(transaction_roots.len(), 1);
    assert!(transaction_roots[0].join("manifest.json").exists());
    assert!(matches!(
        fixture.coordinator.clear_in_order(&fixture.arena, DEADLINE),
        Err(LambError::IndeterminatePublication { .. })
    ));
    assert!(fixture.arena.status(DEADLINE).unwrap().frozen_pending);

    let publisher_called = AtomicBool::new(false);
    let blocked = fixture.coordinator.persist_with_publisher(
        &fixture.arena,
        &mut fixture.workspace,
        PrepareRequest::Recall {
            staging_root: &staging,
            output_dir: &output,
            timestamp: TIMESTAMP_A,
            channel_names: &fixture.names,
        },
        DEADLINE,
        DEADLINE,
        |prepared| {
            publisher_called.store(true, Ordering::Release);
            publish_prepared(prepared)
        },
    );
    assert!(matches!(
        blocked,
        Err(LambError::IndeterminatePublication { .. })
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
    assert!(first_final.exists());

    fail_remove_file.store(false, Ordering::Release);
    std::fs::remove_file(&second_final).unwrap();
    let retry = fixture.recall(TIMESTAMP_A).unwrap();
    assert_eq!(retry.range(), Some(FrameRange { start: 0, end: 4 }));
    assert!(first_final.exists());
    assert!(second_final.exists());
}

#[test]
fn capture_drops_during_blocked_publication_belong_to_successful_outcome_once() {
    let fixture = FrozenFixture::new(8, 1, 4);
    fixture.push(&[0.25, 0.5]);
    let FrozenFixture {
        arena,
        ingress,
        mut workspace,
        coordinator,
        root,
        names,
        ..
    } = fixture;
    let publisher_entered = Arc::new(Barrier::new(2));
    let capture_finished = Arc::new(Barrier::new(2));
    let output_parent = root.path().join("dumps");

    let first = thread::scope(|scope| {
        let capture_entered = Arc::clone(&publisher_entered);
        let capture_done = Arc::clone(&capture_finished);
        scope.spawn(move || {
            capture_entered.wait();
            let pushed = ingress.try_push_interleaved(&[1.0; 11], 1).unwrap();
            assert_eq!(pushed.enqueued_frames, 4);
            assert_eq!(pushed.dropped_frames, 7);
            capture_done.wait();
        });

        coordinator.persist_with_publisher(
            &arena,
            &mut workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publisher_entered.wait();
                capture_finished.wait();
                publish_prepared(prepared)
            },
        )
    })
    .unwrap();

    assert_eq!(first.range(), Some(FrameRange { start: 0, end: 2 }));
    assert_eq!(first.losses().capture_dropped_frames, 7);
    let second = coordinator
        .persist(
            &arena,
            &mut workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_B,
                channel_names: &names,
            },
            DEADLINE,
        )
        .unwrap();
    assert_eq!(second.range(), Some(FrameRange { start: 2, end: 6 }));
    assert_eq!(second.losses().capture_dropped_frames, 0);
}

#[test]
fn complete_interrupted_recall_commits_without_reencoding() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let staging_root = fixture.root.path().join("recall-staging");
    let output_dir = fixture.root.path().join("recall");
    let names = fixture.names.clone();
    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publish_prepared_with_hook(
                    prepared,
                    &mut InterruptAt {
                        checkpoint: PublicationCheckpoint::RecallAfterFinalRename { index: 0 },
                    },
                )
            },
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));
    assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);

    let publisher_called = AtomicBool::new(false);
    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |_| {
                publisher_called.store(true, Ordering::Release);
                panic!("complete manifest recovery must not re-encode or republish")
            },
        )
        .unwrap();

    assert!(!publisher_called.load(Ordering::Acquire));
    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
}

#[test]
fn partial_interrupted_recall_rolls_back_then_retries_exact_frozen_range() {
    let mut fixture = FrozenFixture::new_with_split_and_cleanup(8, 8, 2, 47, None);
    fixture.push(&[0.25, 0.5]);
    let staging_root = fixture.root.path().join("recall-staging");
    let output_dir = fixture.root.path().join("recall");
    let names = fixture.names.clone();
    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publish_prepared_with_hook(
                    prepared,
                    &mut InterruptAt {
                        checkpoint: PublicationCheckpoint::RecallAfterFinalRename { index: 0 },
                    },
                )
            },
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));

    let publisher_calls = AtomicUsize::new(0);
    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publisher_calls.fetch_add(1, Ordering::SeqCst);
                publish_prepared(prepared)
            },
        )
        .unwrap();

    assert_eq!(publisher_calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
    let DumpOutcome::Written { files, .. } = outcome else {
        panic!("expected written retry")
    };
    assert_eq!(files.len(), 2);
}

#[test]
fn rename_before_manifest_update_rolls_back_partial_set_by_adjacent_identity() {
    let mut fixture = FrozenFixture::new_with_split_and_cleanup(8, 8, 2, 47, None);
    fixture.push(&[0.25, 0.5]);
    let staging_root = fixture.root.path().join("recall-staging");
    let output_dir = fixture.root.path().join("recall");
    let names = fixture.names.clone();
    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publish_prepared_with_hook(
                    prepared,
                    &mut InterruptAt {
                        checkpoint: PublicationCheckpoint::RecallRenamedBeforeManifest { index: 0 },
                    },
                )
            },
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));

    let calls = AtomicUsize::new(0);
    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                calls.fetch_add(1, Ordering::SeqCst);
                publish_prepared(prepared)
            },
        )
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
}

#[test]
fn complete_interrupted_dump_commits_without_republishing_visible_directory() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let output_parent = fixture.root.path().join("dumps");
    let names = fixture.names.clone();
    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publish_prepared_with_hook(
                    prepared,
                    &mut InterruptAt {
                        checkpoint: PublicationCheckpoint::DumpAfterRename,
                    },
                )
            },
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));
    assert!(output_parent.join(TIMESTAMP_A).is_dir());

    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |_| panic!("complete dump recovery must not republish"),
        )
        .unwrap();

    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
    assert_eq!(std::fs::read_dir(&output_parent).unwrap().count(), 1);
}

#[test]
fn manifest_and_publication_directory_syncs_follow_crash_safe_order() {
    let mut recall = FrozenFixture::new(8, 8, 2);
    recall.push(&[0.25, 0.5]);
    let recall_staging = recall.root.path().join("recall-staging");
    let recall_output = recall.root.path().join("recall");
    let recall_names = recall.names.clone();
    let mut recall_hook = RecordingPublicationHook::default();
    recall
        .coordinator
        .persist_with_publisher(
            &recall.arena,
            &mut recall.workspace,
            PrepareRequest::Recall {
                staging_root: &recall_staging,
                output_dir: &recall_output,
                timestamp: TIMESTAMP_A,
                channel_names: &recall_names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut recall_hook),
        )
        .unwrap();
    let transaction_root = recall_hook
        .events
        .iter()
        .find_map(|event| match event {
            PublicationEvent::DirectorySync(path) if path.starts_with(&recall_staging) => {
                Some(path.clone())
            }
            _ => None,
        })
        .expect("manifest updates must sync the recall transaction directory");
    assert_eq!(
        recall_hook.events,
        vec![
            PublicationEvent::DirectorySync(recall_output.parent().unwrap().to_path_buf()),
            PublicationEvent::DirectorySync(transaction_root.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallManifestPrepared),
            PublicationEvent::DirectorySync(transaction_root.clone()),
            PublicationEvent::Checkpoint(
                PublicationCheckpoint::RecallPartialCreatedBeforeManifest { index: 0 },
            ),
            PublicationEvent::DirectorySync(transaction_root.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallPartialSynced { index: 0 }),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallBeforeFinalRename {
                index: 0,
            }),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallRenamedBeforeManifest {
                index: 0
            },),
            PublicationEvent::DirectorySync(transaction_root.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallAfterFinalRename {
                index: 0,
            }),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallFilesSynced),
            PublicationEvent::DirectorySync(recall_output.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallOutputSynced),
            PublicationEvent::DirectorySync(transaction_root),
            PublicationEvent::Checkpoint(PublicationCheckpoint::RecallCompleteRecorded),
        ]
    );

    let mut dump = FrozenFixture::new(8, 8, 2);
    dump.push(&[0.25, 0.5]);
    let dump_parent = dump.root.path().join("dumps");
    let dump_names = dump.names.clone();
    let mut dump_hook = RecordingPublicationHook::default();
    dump.coordinator
        .persist_with_publisher(
            &dump.arena,
            &mut dump.workspace,
            PrepareRequest::Dump {
                output_parent: &dump_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &dump_names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut dump_hook),
        )
        .unwrap();
    let hidden = dump_hook
        .events
        .iter()
        .find_map(|event| match event {
            PublicationEvent::DirectorySync(path) if path != &dump_parent => Some(path.clone()),
            _ => None,
        })
        .expect("dump hidden directory must be synced");
    assert_eq!(
        dump_hook.events,
        vec![
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpFilesSynced),
            PublicationEvent::DirectorySync(hidden),
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpDirectorySynced),
            PublicationEvent::DirectorySync(dump_parent.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpManifestPrepared),
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpAfterRename),
            PublicationEvent::DirectorySync(dump_parent.clone()),
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpParentSynced),
            PublicationEvent::DirectorySync(dump_parent),
            PublicationEvent::Checkpoint(PublicationCheckpoint::DumpCompleteRecorded),
        ]
    );
}

#[test]
fn visible_dump_manifest_with_failed_parent_sync_is_recovered_before_retry() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let output_parent = fixture.root.path().join("dumps");
    let names = fixture.names.clone();
    let first = fixture.coordinator.persist_with_publisher(
        &fixture.arena,
        &mut fixture.workspace,
        PrepareRequest::Dump {
            output_parent: &output_parent,
            timestamp: TIMESTAMP_A,
            channel_names: &names,
        },
        DEADLINE,
        DEADLINE,
        |prepared| {
            publish_prepared_with_hook(
                prepared,
                &mut FailDirectorySyncAt {
                    call: 0,
                    fail_at: 2,
                },
            )
        },
    );
    assert!(matches!(
        first,
        Err(LambError::IndeterminatePublication { .. })
    ));

    let calls = AtomicUsize::new(0);
    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                calls.fetch_add(1, Ordering::SeqCst);
                publish_prepared(prepared)
            },
        )
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
}

#[test]
fn post_commit_recall_cleanup_preserves_replaced_staged_inode() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let staging_root = fixture.root.path().join("recall-staging");
    let output_dir = fixture.root.path().join("recall");
    let names = fixture.names.clone();
    let mut hook = ReplaceStagedAtComplete {
        staging_parent: staging_root.clone(),
        replaced_path: None,
    };

    let outcome = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut hook),
        )
        .unwrap();

    assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
    let replaced = hook.replaced_path.expect("hook replaced a staged WAV");
    assert_eq!(std::fs::read(replaced).unwrap(), b"foreign staged inode");
    assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);
}

#[test]
fn post_commit_dump_cleanup_preserves_replaced_manifest_inode() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let output_parent = fixture.root.path().join("dumps");
    let names = fixture.names.clone();
    let mut hook = ReplaceDumpManifestAtComplete {
        output_parent: output_parent.clone(),
        replacement_path: None,
    };

    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut hook),
        )
        .unwrap_err();

    assert!(matches!(error, LambError::IndeterminatePublication { .. }));
    let replacement = hook
        .replacement_path
        .expect("hook replaced the dump manifest");
    assert_eq!(
        std::fs::read(replacement).unwrap(),
        b"foreign manifest inode"
    );
    assert!(output_parent.join(TIMESTAMP_A).join("mic.wav").exists());
}

#[test]
fn every_recall_checkpoint_recovers_without_duplicate_publication() {
    let checkpoints = [
        (PublicationCheckpoint::RecallManifestPrepared, true),
        (
            PublicationCheckpoint::RecallPartialSynced { index: 0 },
            true,
        ),
        (
            PublicationCheckpoint::RecallBeforeFinalRename { index: 0 },
            true,
        ),
        (
            PublicationCheckpoint::RecallAfterFinalRename { index: 0 },
            false,
        ),
        (
            PublicationCheckpoint::RecallRenamedBeforeManifest { index: 0 },
            false,
        ),
        (PublicationCheckpoint::RecallFilesSynced, false),
        (PublicationCheckpoint::RecallOutputSynced, false),
        (PublicationCheckpoint::RecallCompleteRecorded, false),
    ];
    for (checkpoint, expects_retry) in checkpoints {
        let mut fixture = FrozenFixture::new(8, 8, 2);
        fixture.push(&[0.25, 0.5]);
        let staging_root = fixture.root.path().join("recall-staging");
        let output_dir = fixture.root.path().join("recall");
        let names = fixture.names.clone();
        let error = fixture
            .coordinator
            .persist_with_publisher(
                &fixture.arena,
                &mut fixture.workspace,
                PrepareRequest::Recall {
                    staging_root: &staging_root,
                    output_dir: &output_dir,
                    timestamp: TIMESTAMP_A,
                    channel_names: &names,
                },
                DEADLINE,
                DEADLINE,
                |prepared| publish_prepared_with_hook(prepared, &mut InterruptAt { checkpoint }),
            )
            .unwrap_err();
        assert!(matches!(error, LambError::IndeterminatePublication { .. }));

        let calls = AtomicUsize::new(0);
        let outcome = fixture
            .coordinator
            .persist_with_publisher(
                &fixture.arena,
                &mut fixture.workspace,
                PrepareRequest::Recall {
                    staging_root: &staging_root,
                    output_dir: &output_dir,
                    timestamp: TIMESTAMP_A,
                    channel_names: &names,
                },
                DEADLINE,
                DEADLINE,
                |prepared| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    publish_prepared(prepared)
                },
            )
            .unwrap();
        assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(expects_retry));
        assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);
    }
}

#[test]
fn unrecorded_partial_identity_remains_pending_and_blocks_retry() {
    let mut fixture = FrozenFixture::new(8, 8, 2);
    fixture.push(&[0.25, 0.5]);
    let staging_root = fixture.root.path().join("recall-staging");
    let output_dir = fixture.root.path().join("recall");
    let names = fixture.names.clone();
    let error = fixture
        .coordinator
        .persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output_dir,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| {
                publish_prepared_with_hook(
                    prepared,
                    &mut InterruptAt {
                        checkpoint: PublicationCheckpoint::RecallPartialCreatedBeforeManifest {
                            index: 0,
                        },
                    },
                )
            },
        )
        .unwrap_err();
    assert!(matches!(error, LambError::IndeterminatePublication { .. }));

    let publisher_called = AtomicBool::new(false);
    let retry = fixture.coordinator.persist_with_publisher(
        &fixture.arena,
        &mut fixture.workspace,
        PrepareRequest::Recall {
            staging_root: &staging_root,
            output_dir: &output_dir,
            timestamp: TIMESTAMP_A,
            channel_names: &names,
        },
        DEADLINE,
        DEADLINE,
        |_| {
            publisher_called.store(true, Ordering::Release);
            panic!("pending manifest recovery must block retry")
        },
    );

    assert!(matches!(
        retry,
        Err(LambError::IndeterminatePublication { .. })
    ));
    assert!(!publisher_called.load(Ordering::Acquire));
}

#[test]
fn every_dump_checkpoint_rolls_back_or_completes_without_duplicate_directory() {
    let checkpoints = [
        (PublicationCheckpoint::DumpFilesSynced, true, false),
        (PublicationCheckpoint::DumpDirectorySynced, true, false),
        (PublicationCheckpoint::DumpManifestPrepared, true, true),
        (PublicationCheckpoint::DumpAfterRename, false, true),
        (PublicationCheckpoint::DumpParentSynced, false, true),
        (PublicationCheckpoint::DumpCompleteRecorded, false, true),
    ];
    for (checkpoint, expects_retry, expects_indeterminate) in checkpoints {
        let mut fixture = FrozenFixture::new(8, 8, 2);
        fixture.push(&[0.25, 0.5]);
        let output_parent = fixture.root.path().join("dumps");
        let names = fixture.names.clone();
        let first = fixture.coordinator.persist_with_publisher(
            &fixture.arena,
            &mut fixture.workspace,
            PrepareRequest::Dump {
                output_parent: &output_parent,
                timestamp: TIMESTAMP_A,
                channel_names: &names,
            },
            DEADLINE,
            DEADLINE,
            |prepared| publish_prepared_with_hook(prepared, &mut InterruptAt { checkpoint }),
        );
        assert!(first.is_err());
        assert_eq!(
            matches!(first, Err(LambError::IndeterminatePublication { .. })),
            expects_indeterminate
        );

        let calls = AtomicUsize::new(0);
        let outcome = fixture
            .coordinator
            .persist_with_publisher(
                &fixture.arena,
                &mut fixture.workspace,
                PrepareRequest::Dump {
                    output_parent: &output_parent,
                    timestamp: TIMESTAMP_A,
                    channel_names: &names,
                },
                DEADLINE,
                DEADLINE,
                |prepared| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    publish_prepared(prepared)
                },
            )
            .unwrap();
        assert_eq!(outcome.range(), Some(FrameRange { start: 0, end: 2 }));
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(expects_retry));
        assert_eq!(std::fs::read_dir(&output_parent).unwrap().count(), 1);
    }
}
