use lamb::activity::{ActivityDetectorKind, ChannelExportMode, FrozenExportDecision};
use lamb::capture_arena::{CaptureArena, CaptureRuntimeConfig, FrozenCaptureEpoch};
use lamb::dump::{FrameRange, SampleSnapshot};
use lamb::error::LambError;
use lamb::export_policy::{
    ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
    ResolvedLayout, ValidatedPattern,
};
use lamb::export_wav::{
    export_snapshot_wav, publish_dump, publish_prepared, publish_prepared_with_hook,
    publish_recall, DumpPublishRequest, ExportRequest, PreparedPublication,
    PreparedPublicationHook, PublicationCheckpoint, RecallPublishRequest,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::persistence_workspace::{
    PersistenceWorkspace, PersistenceWorkspaceConfig, PrepareRequest,
};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TIMESTAMP: &str = "20260630T073218";
const SPLIT_LIMIT: u64 = 3_900_000_000;
type SetupFn = Box<dyn FnOnce(&Path)>;
type MutationFn = Box<dyn FnMut(&Path)>;

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

fn wav_s24(path: &Path) -> Vec<i32> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
    assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 10);
    assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 24);
    assert_eq!(&bytes[36..40], b"data");
    bytes[44..]
        .chunks_exact(3)
        .map(|sample| {
            let sign = if sample[2] & 0x80 == 0 { 0 } else { 0xff };
            i32::from_le_bytes([sample[0], sample[1], sample[2], sign])
        })
        .collect()
}

fn collect_files(root: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

#[derive(Default)]
struct SyncTrace {
    checkpoints: Vec<PublicationCheckpoint>,
    directories: Vec<std::path::PathBuf>,
    events: Vec<SyncEvent>,
    setup: Option<SetupFn>,
}

trait TestPublicationHook: PreparedPublicationHook {
    fn setup(&mut self, _output: &Path) {}
}

impl TestPublicationHook for SyncTrace {
    fn setup(&mut self, output: &Path) {
        if let Some(setup) = self.setup.take() {
            setup(output);
        }
    }
}

struct FailAtCheckpoint {
    target: PublicationCheckpoint,
    mutation: Option<MutationFn>,
    mutation_parent: Option<PathBuf>,
}

impl PreparedPublicationHook for FailAtCheckpoint {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> lamb::error::Result<()> {
        if checkpoint == self.target {
            if let (Some(mutation), Some(parent)) =
                (self.mutation.as_mut(), self.mutation_parent.as_deref())
            {
                mutation(parent);
            }
            return Err(LambError::Export(
                "injected Task 6 interruption".to_string(),
            ));
        }
        Ok(())
    }

    fn sync_directory(&mut self, path: &Path) -> lamb::error::Result<()> {
        lamb::recovery::sync_directory(path)
    }
}

impl TestPublicationHook for FailAtCheckpoint {
    fn setup(&mut self, output: &Path) {
        self.mutation_parent = Some(output.join(TIMESTAMP).join("nested"));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SyncEvent {
    Checkpoint(PublicationCheckpoint),
    Directory(std::path::PathBuf),
}

impl PreparedPublicationHook for SyncTrace {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> lamb::error::Result<()> {
        self.checkpoints.push(checkpoint);
        self.events.push(SyncEvent::Checkpoint(checkpoint));
        Ok(())
    }

    fn sync_directory(&mut self, path: &Path) -> lamb::error::Result<()> {
        self.directories.push(path.to_path_buf());
        self.events.push(SyncEvent::Directory(path.to_path_buf()));
        lamb::recovery::sync_directory(path)
    }
}

fn real_policy(root: &Path, layout: ResolvedLayout) -> ResolvedExportPolicy {
    ResolvedExportPolicy::new(
        root.to_path_buf(),
        layout,
        ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: vec![ChannelActivityPolicy {
                name: "mic".to_string(),
                mode: ChannelExportMode::Always,
                threshold: None,
            }],
            whole_export_exact_zero_gate: false,
            trim_leading_silence: false,
        },
    )
    .unwrap()
}

fn with_real_capture(
    command: ExportCommand,
    layout: ResolvedLayout,
    hook: &mut impl TestPublicationHook,
    test: impl FnOnce(
        &mut PersistenceWorkspace,
        &FrozenCaptureEpoch,
        &SessionMemoryPlan,
        &Path,
        &Path,
        &mut FrozenExportDecision,
        PreparedPublication,
    ),
) {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 2,
        channels: 1,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 2,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 4,
        capture_slot_frames: 2,
        capture_worker_stack_bytes: 64 * 1024,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
        headroom: 1.0,
    })
    .unwrap();
    let runtime_config = CaptureRuntimeConfig {
        ring: RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 2,
            chunk_count: 1,
            max_active_snapshots: 1,
        },
        queue_slots: 4,
        slot_frames: 2,
        sample_bytes: 4,
        worker_stack_bytes: 64 * 1024,
    };
    let (mut arena, ingress) = CaptureArena::new(&plan, runtime_config).unwrap();
    let mut workspace = PersistenceWorkspace::new(
        &plan,
        PersistenceWorkspaceConfig {
            retention_frames: 2,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            sample_bytes: 4,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 6,
            maximum_path_bytes: 512,
        },
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    hook.setup(&output);
    ingress.try_push_interleaved(&[0.5], 1).unwrap();
    let frozen = arena
        .freeze_since(None, Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let policy = real_policy(&output, layout);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command,
                policy: &policy,
                profile: "profile",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();
    let result = publish_prepared_with_hook(prepared, hook);
    test(
        &mut workspace,
        &frozen,
        &plan,
        &output,
        &staging,
        &mut decision,
        result,
    );
    arena.shutdown(Duration::from_secs(2)).unwrap();
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
fn recall_timestamp_directory_uses_atomic_publication_on_real_capture() {
    let mut trace = SyncTrace::default();
    with_real_capture(
        ExportCommand::Recall,
        ResolvedLayout::TimestampDirectory,
        &mut trace,
        |_, _, _, output, staging, _, result| {
            let final_dir = output.join(TIMESTAMP);
            let published = match result {
                PreparedPublication::Published(output) => output,
                _ => panic!("real recall timestamp publication failed"),
            };
            assert_eq!(published.files, vec![final_dir.join("mic.wav")]);
            assert!(published.files[0].is_file());
            let visible_directories: Vec<_> = fs::read_dir(output)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect();
            assert_eq!(visible_directories, vec![final_dir.clone()]);
            assert!(fs::read_dir(staging).unwrap().next().is_none());
            let mut files = Vec::new();
            collect_files(output, &mut files);
            assert!(files
                .iter()
                .all(|path| !path.to_string_lossy().contains(".partial")));
        },
    );
    assert!(trace
        .checkpoints
        .contains(&PublicationCheckpoint::DumpAfterRename));
    assert!(!trace.checkpoints.iter().any(|checkpoint| matches!(
        checkpoint,
        PublicationCheckpoint::RecallParentCreatedBeforeOwnedManifest { .. }
            | PublicationCheckpoint::RecallPartialCreatedBeforeManifest { .. }
    )));
}

#[test]
fn dump_timestamp_directory_uses_atomic_publication_on_real_capture() {
    let mut trace = SyncTrace::default();
    with_real_capture(
        ExportCommand::Dump,
        ResolvedLayout::TimestampDirectory,
        &mut trace,
        |_, _, _, output, staging, _, result| {
            let final_dir = output.join(TIMESTAMP);
            let published = match result {
                PreparedPublication::Published(output) => output,
                _ => panic!("real dump timestamp publication failed"),
            };
            assert_eq!(published.files, vec![final_dir.join("mic.wav")]);
            assert!(published.files[0].is_file());
            let visible_directories: Vec<_> = fs::read_dir(output)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.is_dir())
                .collect();
            assert_eq!(visible_directories, vec![final_dir.clone()]);
            assert!(fs::read_dir(staging).unwrap().next().is_none());
        },
    );
    assert!(trace
        .checkpoints
        .contains(&PublicationCheckpoint::DumpAfterRename));
    assert!(!trace.checkpoints.iter().any(|checkpoint| matches!(
        checkpoint,
        PublicationCheckpoint::RecallPartialCreatedBeforeManifest { .. }
    )));
}

fn custom_timestamp_layout() -> ResolvedLayout {
    custom_directory_layout("{timestamp}")
}

fn custom_directory_layout(directory_pattern: &str) -> ResolvedLayout {
    ResolvedLayout::Custom {
        directory_pattern: ValidatedPattern::parse(directory_pattern).unwrap(),
        filename_pattern: ValidatedPattern::parse("{channel}.wav").unwrap(),
    }
}

#[test]
fn recall_parent_created_before_owned_manifest_recovers_and_retries_exact_epoch() {
    let mut hook = FailAtCheckpoint {
        target: PublicationCheckpoint::RecallParentCreatedBeforeOwnedManifest { index: 1 },
        mutation: None,
        mutation_parent: None,
    };
    with_real_capture(
        ExportCommand::Recall,
        custom_directory_layout("{timestamp}/nested"),
        &mut hook,
        |workspace, frozen, plan, output, staging, decision, result| {
            let mut cleanup = match result {
                PreparedPublication::Indeterminate { cleanup, .. } => cleanup,
                _ => panic!("parent-created interruption was not indeterminate"),
            };
            let recovery = workspace
                .recover_indeterminate_publication(&mut cleanup)
                .unwrap();
            assert!(matches!(
                recovery,
                lamb::persistence_workspace::PublicationRecovery::RolledBack
            ));
            assert!(output.join(TIMESTAMP).join("nested").exists());
            let corrected = real_policy(output, custom_directory_layout("{timestamp}/corrected"));
            let retried = workspace
                .prepare(
                    frozen,
                    PrepareRequest::Policy {
                        command: ExportCommand::Recall,
                        policy: &corrected,
                        profile: "profile",
                        staging_root: staging,
                        timestamp: TIMESTAMP,
                        decision,
                    },
                )
                .unwrap();
            assert!(matches!(
                publish_prepared(retried),
                PreparedPublication::Published(_)
            ));
            assert!(output.join(TIMESTAMP).join("corrected/mic.wav").is_file());
            let _ = plan;
        },
    );
}

#[test]
fn recall_parent_owned_manifest_recorded_recovers_deepest_first_and_retries() {
    let mut hook = FailAtCheckpoint {
        target: PublicationCheckpoint::RecallParentOwnedManifestRecorded { index: 1 },
        mutation: None,
        mutation_parent: None,
    };
    with_real_capture(
        ExportCommand::Recall,
        custom_directory_layout("{timestamp}/nested"),
        &mut hook,
        |workspace, frozen, _, output, staging, decision, result| {
            let mut cleanup = match result {
                PreparedPublication::Indeterminate { cleanup, .. } => cleanup,
                _ => panic!("parent-owned interruption was not indeterminate"),
            };
            assert!(matches!(
                workspace.recover_indeterminate_publication(&mut cleanup),
                Ok(lamb::persistence_workspace::PublicationRecovery::RolledBack)
            ));
            assert!(!output.join(TIMESTAMP).join("nested").exists());
            let corrected = real_policy(output, custom_directory_layout("{timestamp}/corrected"));
            let retried = workspace
                .prepare(
                    frozen,
                    PrepareRequest::Policy {
                        command: ExportCommand::Recall,
                        policy: &corrected,
                        profile: "profile",
                        staging_root: staging,
                        timestamp: TIMESTAMP,
                        decision,
                    },
                )
                .unwrap();
            assert!(matches!(
                publish_prepared(retried),
                PreparedPublication::Published(_)
            ));
            assert!(output.join(TIMESTAMP).join("corrected/mic.wav").is_file());
        },
    );
}

#[test]
fn recall_parent_owned_manifest_recorded_foreign_sibling_finishes_rollback_and_retries() {
    let mut hook = FailAtCheckpoint {
        target: PublicationCheckpoint::RecallParentOwnedManifestRecorded { index: 1 },
        mutation: Some(Box::new(|parent| {
            fs::write(parent.join("foreign-sibling"), b"foreign").unwrap();
        })),
        mutation_parent: None,
    };
    with_real_capture(
        ExportCommand::Recall,
        custom_directory_layout("{timestamp}/nested"),
        &mut hook,
        |workspace, frozen, _, output, staging, decision, result| {
            let mut cleanup = match result {
                PreparedPublication::Indeterminate { cleanup, .. } => cleanup,
                _ => panic!("foreign-sibling interruption was not indeterminate"),
            };
            assert!(matches!(
                workspace.recover_indeterminate_publication(&mut cleanup),
                Ok(lamb::persistence_workspace::PublicationRecovery::RolledBack)
            ));
            let parent = output.join(TIMESTAMP).join("nested");
            assert_eq!(
                fs::read(parent.join("foreign-sibling")).unwrap(),
                b"foreign"
            );
            let corrected = real_policy(output, custom_directory_layout("{timestamp}/corrected"));
            let retried = workspace
                .prepare(
                    frozen,
                    PrepareRequest::Policy {
                        command: ExportCommand::Recall,
                        policy: &corrected,
                        profile: "profile",
                        staging_root: staging,
                        timestamp: TIMESTAMP,
                        decision,
                    },
                )
                .unwrap();
            assert!(matches!(
                publish_prepared(retried),
                PreparedPublication::Published(_)
            ));
            assert!(output.join(TIMESTAMP).join("corrected/mic.wav").is_file());
        },
    );
}

#[test]
fn recall_custom_timestamp_directory_uses_fileset_publication_on_real_capture() {
    let mut trace = SyncTrace::default();
    with_real_capture(
        ExportCommand::Recall,
        custom_timestamp_layout(),
        &mut trace,
        |workspace, _, _, output, staging, _, result| {
            let final_path = output.join(TIMESTAMP).join("mic.wav");
            let published = match result {
                PreparedPublication::Published(output) => output,
                _ => panic!("real recall custom publication failed"),
            };
            assert_eq!(published.files, vec![final_path.clone()]);
            assert!(final_path.is_file());
            assert!(!output.join(format!(".{TIMESTAMP}.manifest.json")).exists());
            let recovered = workspace.recover_recall_staging(staging, output);
            assert_eq!(recovered.completed, 1);
            assert!(fs::read_dir(staging).unwrap().next().is_none());
        },
    );
    assert!(!trace
        .checkpoints
        .contains(&PublicationCheckpoint::DumpAfterRename));
    assert!(trace
        .checkpoints
        .contains(&PublicationCheckpoint::RecallParentOwnedManifestRecorded { index: 0 }));
}

#[test]
fn dump_custom_timestamp_directory_uses_fileset_publication_on_real_capture() {
    let mut trace = SyncTrace::default();
    with_real_capture(
        ExportCommand::Dump,
        custom_timestamp_layout(),
        &mut trace,
        |workspace, _, _, output, staging, _, result| {
            let final_path = output.join(TIMESTAMP).join("mic.wav");
            let published = match result {
                PreparedPublication::Published(output) => output,
                _ => panic!("real dump custom publication failed"),
            };
            assert_eq!(published.files, vec![final_path.clone()]);
            assert!(final_path.is_file());
            assert!(!output.join(format!(".{TIMESTAMP}.manifest.json")).exists());
            let recovered = workspace.recover_recall_staging(staging, output);
            assert_eq!(recovered.completed, 1);
            assert!(fs::read_dir(staging).unwrap().next().is_none());
        },
    );
    assert!(!trace
        .checkpoints
        .contains(&PublicationCheckpoint::DumpAfterRename));
    assert!(trace
        .checkpoints
        .contains(&PublicationCheckpoint::RecallParentOwnedManifestRecorded { index: 0 }));
}

#[test]
fn nested_ancestor_symlink_fails_before_mutation_and_corrected_retry_succeeds() {
    let mut trace = SyncTrace {
        setup: Some(Box::new(|output| {
            let target = output.parent().unwrap().join("symlink-target");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("marker"), b"target marker").unwrap();
            symlink(&target, output.join("link")).unwrap();
        })),
        ..Default::default()
    };
    with_real_capture(
        ExportCommand::Recall,
        custom_directory_layout("link/{timestamp}"),
        &mut trace,
        |workspace, frozen, _, output, staging, decision, result| {
            assert!(matches!(result, PreparedPublication::RetryableFailure(_)));
            let target = output.parent().unwrap().join("symlink-target");
            assert_eq!(fs::read(target.join("marker")).unwrap(), b"target marker");
            assert!(!target.join(TIMESTAMP).join("mic.wav").exists());
            let corrected = real_policy(output, custom_directory_layout("corrected/{timestamp}"));
            let retried = workspace
                .prepare(
                    frozen,
                    PrepareRequest::Policy {
                        command: ExportCommand::Recall,
                        policy: &corrected,
                        profile: "profile",
                        staging_root: staging,
                        timestamp: TIMESTAMP,
                        decision,
                    },
                )
                .unwrap();
            assert!(matches!(
                publish_prepared(retried),
                PreparedPublication::Published(_)
            ));
            assert!(output
                .join("corrected")
                .join(TIMESTAMP)
                .join("mic.wav")
                .is_file());
        },
    );
}

#[test]
fn nested_final_collision_preserves_existing_file_and_corrected_retry_succeeds() {
    let mut trace = SyncTrace {
        setup: Some(Box::new(|output| {
            let final_dir = output.join(TIMESTAMP);
            fs::create_dir_all(&final_dir).unwrap();
            fs::write(final_dir.join("mic.wav"), b"foreign collision").unwrap();
        })),
        ..Default::default()
    };
    with_real_capture(
        ExportCommand::Recall,
        custom_timestamp_layout(),
        &mut trace,
        |workspace, frozen, _, output, staging, decision, result| {
            assert!(matches!(result, PreparedPublication::RetryableFailure(_)));
            let collision = output.join(TIMESTAMP).join("mic.wav");
            assert_eq!(fs::read(&collision).unwrap(), b"foreign collision");
            let corrected = real_policy(output, custom_directory_layout("corrected"));
            let retried = workspace
                .prepare(
                    frozen,
                    PrepareRequest::Policy {
                        command: ExportCommand::Recall,
                        policy: &corrected,
                        profile: "profile",
                        staging_root: staging,
                        timestamp: TIMESTAMP,
                        decision,
                    },
                )
                .unwrap();
            assert!(matches!(
                publish_prepared(retried),
                PreparedPublication::Published(_)
            ));
            assert!(output.join("corrected").join("mic.wav").is_file());
        },
    );
}

#[test]
fn prepared_timestamp_directory_publishes_canonical_filenames() {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 2,
        channels: 1,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 2,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 4,
        capture_slot_frames: 2,
        capture_worker_stack_bytes: 64 * 1024,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
        headroom: 1.0,
    })
    .unwrap();
    let runtime_config = CaptureRuntimeConfig {
        ring: RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 2,
            chunk_count: 1,
            max_active_snapshots: 1,
        },
        queue_slots: 4,
        slot_frames: 2,
        sample_bytes: 4,
        worker_stack_bytes: 64 * 1024,
    };
    let (mut arena, ingress) = CaptureArena::new(&plan, runtime_config).unwrap();
    let mut workspace = PersistenceWorkspace::new(
        &plan,
        PersistenceWorkspaceConfig {
            retention_frames: 2,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            sample_bytes: 4,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 6,
            maximum_path_bytes: 512,
        },
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    let policy = ResolvedExportPolicy::new(
        output.clone(),
        ResolvedLayout::TimestampDirectory,
        ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: vec![ChannelActivityPolicy {
                name: "mic".to_string(),
                mode: ChannelExportMode::Always,
                threshold: None,
            }],
            whole_export_exact_zero_gate: false,
            trim_leading_silence: false,
        },
    )
    .unwrap();
    ingress.try_push_interleaved(&[0.5, 0.25], 1).unwrap();
    let frozen = arena
        .freeze_since(None, Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Dump,
                policy: &policy,
                profile: "canonical",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();

    let published = match publish_prepared(prepared) {
        PreparedPublication::Published(published) => published,
        PreparedPublication::RetryableFailure(error) => panic!("publication failed: {error}"),
        PreparedPublication::Indeterminate { operation, .. } => {
            panic!("publication became indeterminate: {operation}")
        }
    };

    assert_eq!(published.files, [output.join(TIMESTAMP).join("mic.wav")]);
    assert!(published.files.iter().all(|path| path.exists()));
    assert!(fs::read_dir(output.join(TIMESTAMP))
        .unwrap()
        .all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("output-")));
    arena.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn nested_sparse_file_set_publishes_exact_dense_wavs_and_syncs_each_parent() {
    let split_limit = 50;
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 6,
        channels: 4,
        sample_rate: 10,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 3,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: split_limit,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 4,
        capture_slot_frames: 3,
        capture_worker_stack_bytes: 64 * 1024,
        io_buffer_bytes_per_channel: 9,
        maximum_path_bytes: 512,
        headroom: 1.0,
    })
    .unwrap();
    let runtime = CaptureRuntimeConfig {
        ring: RingConfig {
            channels: 4,
            sample_rate: 10,
            format: SampleFormat::F32Le,
            chunk_frames: 3,
            chunk_count: 2,
            max_active_snapshots: 1,
        },
        queue_slots: 4,
        slot_frames: 3,
        sample_bytes: 4,
        worker_stack_bytes: 64 * 1024,
    };
    let (mut arena, ingress) = CaptureArena::new(&plan, runtime).unwrap();
    let mut workspace = PersistenceWorkspace::new(
        &plan,
        PersistenceWorkspaceConfig {
            retention_frames: 6,
            channels: 4,
            sample_rate: 10,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 3,
            sample_bytes: 4,
            split_when_over_bytes: split_limit,
            io_buffer_bytes_per_channel: 9,
            maximum_path_bytes: 512,
        },
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    fs::create_dir_all(&output).unwrap();
    let modes = [
        ChannelExportMode::Auto,
        ChannelExportMode::Never,
        ChannelExportMode::Auto,
        ChannelExportMode::Auto,
    ];
    let names = ["front", "never", "rear", "silent"];
    let policy = ResolvedExportPolicy::new(
        output.clone(),
        ResolvedLayout::Custom {
            directory_pattern: ValidatedPattern::parse("{profile}/{channel}/part-{part}").unwrap(),
            filename_pattern: ValidatedPattern::parse(
                "{timestamp}-{channel}-{part}-{startFrame}-{endFrame}.wav",
            )
            .unwrap(),
        },
        ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: names
                .iter()
                .zip(modes)
                .map(|(name, mode)| ChannelActivityPolicy {
                    name: (*name).to_string(),
                    mode,
                    threshold: None,
                })
                .collect(),
            whole_export_exact_zero_gate: false,
            trim_leading_silence: false,
        },
    )
    .unwrap();
    let samples = [
        0.0, 1.0, -1.0, 0.0, 0.5, 1.0, -0.5, 0.0, 0.25, 1.0, 0.0, 0.0, -0.5, 1.0, 0.5, 0.0, 0.75,
        1.0, 0.25, 0.0, -0.25, 1.0, -0.25, 0.0,
    ];
    ingress.try_push_interleaved(&samples, 4).unwrap();
    let frozen = arena
        .freeze_since(None, Duration::from_secs(2))
        .unwrap()
        .unwrap();
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "live",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();
    let files = prepared.files().unwrap();
    let dense_plan: Vec<_> = (0..files.len())
        .map(|index| {
            let file = files.get(index).unwrap();
            (
                file.final_path().to_path_buf(),
                file.start_frame(),
                file.frame_count(),
                file.channel(),
                file.part(),
            )
        })
        .collect();
    let expected_paths: Vec<_> = ["front", "rear"]
        .into_iter()
        .flat_map(|channel| {
            (1_u64..=3).map({
                let output = output.clone();
                move |part| {
                    let start = (part - 1) * 2;
                    output.join(format!(
                        "live/{channel}/part-{part}/{TIMESTAMP}-{channel}-{part}-{start}-{}.wav",
                        start + 2
                    ))
                }
            })
        })
        .collect();
    assert_eq!(
        dense_plan
            .iter()
            .map(|(path, start, count, channel, part)| (path, *start, *count, *channel, *part))
            .collect::<Vec<_>>(),
        expected_paths
            .iter()
            .enumerate()
            .map(|(index, path)| (
                path,
                ((index % 3) * 2) as u64,
                2,
                if index < 3 { 0 } else { 2 },
                (index % 3 + 1) as u32
            ))
            .collect::<Vec<_>>()
    );

    let mut trace = SyncTrace::default();
    let published = match publish_prepared_with_hook(prepared, &mut trace) {
        PreparedPublication::Published(output) => output,
        PreparedPublication::RetryableFailure(error) => panic!("publication failed: {error}"),
        PreparedPublication::Indeterminate { operation, .. } => {
            panic!("publication became indeterminate: {operation}")
        }
    };
    assert_eq!(published.output_directory, output);
    assert_eq!(published.files, expected_paths);
    assert!(published.files.iter().all(|path| path.is_file()));
    let expected_samples = [
        vec![0, 4_194_304],
        vec![2_097_152, -4_194_304],
        vec![6_291_455, -2_097_152],
        vec![-8_388_608, -4_194_304],
        vec![0, 4_194_304],
        vec![2_097_152, -2_097_152],
    ];
    for (path, samples) in published.files.iter().zip(expected_samples) {
        assert_eq!(wav_frames(path), 2);
        assert_eq!(wav_s24(path), samples);
    }
    let files_synced = trace
        .checkpoints
        .iter()
        .position(|event| *event == PublicationCheckpoint::RecallFilesSynced)
        .unwrap();
    let output_synced = trace
        .checkpoints
        .iter()
        .position(|event| *event == PublicationCheckpoint::RecallOutputSynced)
        .unwrap();
    assert!(files_synced < output_synced);
    let synced_parents: Vec<_> = trace
        .events
        .iter()
        .skip_while(|event| {
            *event != &SyncEvent::Checkpoint(PublicationCheckpoint::RecallFilesSynced)
        })
        .skip(1)
        .take_while(|event| {
            *event != &SyncEvent::Checkpoint(PublicationCheckpoint::RecallOutputSynced)
        })
        .map(|event| match event {
            SyncEvent::Directory(path) => path.clone(),
            SyncEvent::Checkpoint(other) => {
                panic!("unexpected checkpoint before output sync: {other:?}")
            }
        })
        .collect();
    assert_eq!(
        synced_parents,
        vec![
            output.clone(),
            output.join("live"),
            output.join("live/front"),
            output.join("live/front/part-1"),
            output.join("live/front/part-2"),
            output.join("live/front/part-3"),
            output.join("live/rear"),
            output.join("live/rear/part-1"),
            output.join("live/rear/part-2"),
            output.join("live/rear/part-3"),
        ]
    );
    let mut actual_files = Vec::new();
    collect_files(&output, &mut actual_files);
    actual_files.sort();
    let mut expected_sorted = published.files.clone();
    expected_sorted.sort();
    assert_eq!(actual_files, expected_sorted);
    assert!(actual_files.iter().all(|path| {
        let text = path.to_string_lossy();
        !text.contains("never")
            && !text.contains("silent")
            && !text.contains(".partial")
            && !text.contains("output-")
    }));
    let recovered = workspace.recover_recall_staging(&staging, &output);
    assert_eq!(recovered.completed, 1);
    assert_eq!(recovered.failed, 0);
    assert_eq!(recovered.pending, 0);
    assert!(!staging.exists() || fs::read_dir(&staging).unwrap().next().is_none());
    arena.shutdown(Duration::from_secs(2)).unwrap();
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
        .as_chunks::<2>()
        .0
        .iter()
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
