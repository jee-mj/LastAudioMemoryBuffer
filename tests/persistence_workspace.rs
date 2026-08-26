use lamb::activity::{
    ActivityDetectorKind, ActivityResult, ChannelDisposition, ChannelExportMode,
    FrozenExportDecision,
};
use lamb::capture_arena::{CaptureArena, CaptureIngress, CaptureRuntimeConfig, FrozenCaptureEpoch};
use lamb::error::LambError;
use lamb::export_policy::{
    ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
    ResolvedLayout, ValidatedPattern,
};
use lamb::export_wav::{
    publish_prepared, publish_prepared_with_hook, PreparedPublication, PreparedPublicationHook,
    PublicationCheckpoint,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::persistence_workspace::{
    CleanupIo, PendingCleanup, PersistenceWorkspace, PersistenceWorkspaceConfig, PrepareRequest,
    PreparedPersistence, PublicationRecovery, WavIo,
};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStrExt, os::unix::ffi::OsStringExt};

const DEADLINE: Duration = Duration::from_secs(2);
const TIMESTAMP: &str = "20260818T120000";

struct ThreadCountingAllocator;

thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static ALLOCATION_BYTES: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for ThreadCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT_ALLOCATIONS.with(|enabled| {
            if enabled.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                ALLOCATION_BYTES.with(|bytes| bytes.set(bytes.get() + layout.size()));
            }
        });
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
    }
}

#[global_allocator]
static ALLOCATOR: ThreadCountingAllocator = ThreadCountingAllocator;

fn allocation_count_during<T>(operation: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOCATION_COUNT.with(|count| count.set(0));
    ALLOCATION_BYTES.with(|bytes| bytes.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let result = operation();
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
    let count = ALLOCATION_COUNT.with(Cell::get);
    let bytes = ALLOCATION_BYTES.with(Cell::get);
    (result, count, bytes)
}

struct InterruptPublicationAt(PublicationCheckpoint);

impl PreparedPublicationHook for InterruptPublicationAt {
    fn checkpoint(&mut self, checkpoint: PublicationCheckpoint) -> lamb::error::Result<()> {
        if checkpoint == self.0 {
            return Err(LambError::Export(
                "injected prepared publication interruption".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct CleanupRaceState {
    replaced_path: Option<PathBuf>,
}

struct ReplaceAtCleanupCheckRemoveBoundary {
    state: Arc<Mutex<CleanupRaceState>>,
}

impl CleanupIo for ReplaceAtCleanupCheckRemoveBoundary {
    fn symlink_metadata(&mut self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn after_current_artifact_handoff(&mut self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if state.replaced_path.is_none() {
            fs::write(path, b"foreign cleanup race inode")?;
            state.replaced_path = Some(path.to_path_buf());
        }
        Ok(())
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

#[derive(Clone, Copy)]
struct Geometry {
    retention_frames: u64,
    channels: u32,
    chunk_frames: u32,
    split_when_over_bytes: u64,
    io_buffer_bytes_per_channel: u64,
    maximum_path_bytes: u64,
}

fn plan(geometry: Geometry) -> SessionMemoryPlan {
    SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: geometry.retention_frames,
        channels: geometry.channels,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: geometry.chunk_frames,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: geometry.split_when_over_bytes,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 32,
        capture_slot_frames: geometry.chunk_frames,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: geometry.io_buffer_bytes_per_channel,
        maximum_path_bytes: geometry.maximum_path_bytes,
        maximum_calibration_seconds: 0,
        headroom: 1.0,
    })
    .unwrap()
}

fn workspace_config(geometry: Geometry) -> PersistenceWorkspaceConfig {
    PersistenceWorkspaceConfig {
        retention_frames: geometry.retention_frames,
        channels: geometry.channels,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: geometry.chunk_frames,
        sample_bytes: 4,
        split_when_over_bytes: geometry.split_when_over_bytes,
        io_buffer_bytes_per_channel: geometry.io_buffer_bytes_per_channel,
        maximum_path_bytes: geometry.maximum_path_bytes,
    }
}

fn runtime(geometry: Geometry) -> (CaptureArena, CaptureIngress, SessionMemoryPlan) {
    let plan = plan(geometry);
    let runtime = CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: geometry.channels,
                sample_rate: 48_000,
                format: SampleFormat::F32Le,
                chunk_frames: geometry.chunk_frames,
                chunk_count: geometry
                    .retention_frames
                    .div_ceil(u64::from(geometry.chunk_frames)) as u32,
                max_active_snapshots: 1,
            },
            queue_slots: 32,
            slot_frames: geometry.chunk_frames,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        },
    )
    .unwrap();
    (runtime.0, runtime.1, plan)
}

fn freeze(
    arena: &mut CaptureArena,
    ingress: &CaptureIngress,
    samples: &[f32],
    channels: u32,
) -> FrozenCaptureEpoch {
    ingress.try_push_interleaved(samples, channels).unwrap();
    arena.freeze_since(None, DEADLINE).unwrap().unwrap()
}

fn wav_bytes(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    assert_eq!(&bytes[36..40], b"data");
    bytes
}

fn s24(bytes: &[u8]) -> i32 {
    let sign = if bytes[2] & 0x80 == 0 { 0 } else { 0xff };
    i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign])
}

fn exact_zero_policy(output_dir: &Path, modes: &[ChannelExportMode]) -> ResolvedExportPolicy {
    activity_policy(output_dir, modes, ResolvedLayout::FlatDetailed, false)
}

fn activity_policy(
    output_dir: &Path,
    modes: &[ChannelExportMode],
    layout: ResolvedLayout,
    trim_leading_silence: bool,
) -> ResolvedExportPolicy {
    ResolvedExportPolicy::new(
        output_dir.to_path_buf(),
        layout,
        ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: modes
                .iter()
                .enumerate()
                .map(|(index, &mode)| ChannelActivityPolicy {
                    name: format!("channel-{index}"),
                    mode,
                    threshold: None,
                })
                .collect(),
            whole_export_exact_zero_gate: false,
            trim_leading_silence,
        },
    )
    .unwrap()
}

fn custom_layout(directory: &str, filename: &str) -> ResolvedLayout {
    ResolvedLayout::Custom {
        directory_pattern: ValidatedPattern::parse(directory).unwrap(),
        filename_pattern: ValidatedPattern::parse(filename).unwrap(),
    }
}

#[test]
fn policy_prepare_retains_only_active_channels_in_original_order() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 4,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let policy = exact_zero_policy(root.path(), &[ChannelExportMode::Auto; 4]);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let frozen = freeze(
        &mut arena,
        &ingress,
        &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.25, 0.0],
        4,
    );

    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "test",
                staging_root: &root.path().join("staging"),
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();

    let files = prepared.files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files.get(0).unwrap().channel(), 2);
}

#[cfg(unix)]
#[test]
fn canonical_prepare_rejects_non_utf8_staging_root_before_filesystem_mutation() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("output");
    let policy = exact_zero_policy(&output, &[ChannelExportMode::Always]);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut staging_bytes = temporary.path().as_os_str().as_bytes().to_vec();
    staging_bytes.extend_from_slice(b"/staging-\xff");
    let staging_root = PathBuf::from(OsString::from_vec(staging_bytes));
    let mut frozen = freeze(&mut arena, &ingress, &[0.25, 0.5], 1);

    let error = match workspace.prepare(
        &frozen,
        PrepareRequest::Policy {
            command: ExportCommand::Recall,
            policy: &policy,
            profile: "non-utf8",
            staging_root: &staging_root,
            timestamp: TIMESTAMP,
            decision: &mut decision,
        },
    ) {
        Err(error) => error,
        Ok(prepared) => {
            drop(prepared);
            panic!("canonical prepare accepted a non-UTF-8 staging root")
        }
    };

    assert!(error.to_string().contains("UTF-8"));
    assert!(!staging_root.exists());
    assert!(!output.exists());
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn policy_prepare_distinguishes_policy_and_silent_skips_and_keeps_non_finite() {
    for (modes, samples, expected_policy_skip) in [
        (
            [ChannelExportMode::Never, ChannelExportMode::Never],
            [0.0, 0.0, 0.0, 0.0],
            true,
        ),
        (
            [ChannelExportMode::Auto, ChannelExportMode::Auto],
            [0.0, -0.0, 0.0, -0.0],
            false,
        ),
        (
            [ChannelExportMode::Auto, ChannelExportMode::Auto],
            [f32::NAN, 0.0, 0.0, f32::INFINITY],
            false,
        ),
    ] {
        let geometry = Geometry {
            retention_frames: 2,
            channels: 2,
            chunk_frames: 2,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 6,
            maximum_path_bytes: 512,
        };
        let (mut arena, ingress, plan) = runtime(geometry);
        let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = exact_zero_policy(root.path(), &modes);
        let mut decision = FrozenExportDecision::new(&plan).unwrap();
        let frozen = freeze(&mut arena, &ingress, &samples, 2);
        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "test",
                    staging_root: &root.path().join("staging"),
                    timestamp: TIMESTAMP,
                    decision: &mut decision,
                },
            )
            .unwrap();
        if expected_policy_skip {
            assert!(matches!(prepared, PreparedPersistence::SkippedByPolicy));
        } else if samples.iter().all(|sample| *sample == 0.0) {
            assert!(matches!(prepared, PreparedPersistence::SkippedSilent));
        } else {
            assert_eq!(prepared.files().unwrap().len(), 2);
        }
    }
}

#[test]
fn real_exact_zero_retention_matrix_preserves_original_channel_order_and_ambiguity() {
    struct Case {
        modes: [ChannelExportMode; 4],
        samples: [f32; 8],
        expected_channels: &'static [u32],
        expected_skip: Option<bool>,
    }
    let cases = [
        Case {
            modes: [ChannelExportMode::Auto; 4],
            samples: [0.5, 0.000_001, 0.0, -0.0, 0.25, 0.0, 0.0, -0.0],
            expected_channels: &[0, 1],
            expected_skip: None,
        },
        Case {
            modes: [ChannelExportMode::Auto; 4],
            samples: [0.0, 0.0, 0.0, 0.25, 0.0, -0.0, 0.0, 0.0],
            expected_channels: &[3],
            expected_skip: None,
        },
        Case {
            modes: [ChannelExportMode::Auto; 4],
            samples: [0.0, -0.0, 0.0, -0.0, 0.0, -0.0, 0.0, -0.0],
            expected_channels: &[],
            expected_skip: Some(false),
        },
        Case {
            modes: [ChannelExportMode::Never; 4],
            samples: [1.0; 8],
            expected_channels: &[],
            expected_skip: Some(true),
        },
        Case {
            modes: [
                ChannelExportMode::Never,
                ChannelExportMode::Auto,
                ChannelExportMode::Never,
                ChannelExportMode::Auto,
            ],
            samples: [1.0, 0.0, 1.0, -0.0, 1.0, 0.0, 1.0, -0.0],
            expected_channels: &[],
            expected_skip: Some(false),
        },
        Case {
            modes: [
                ChannelExportMode::Never,
                ChannelExportMode::Always,
                ChannelExportMode::Never,
                ChannelExportMode::Never,
            ],
            samples: [1.0, 0.0, 1.0, 1.0, 1.0, -0.0, 1.0, 1.0],
            expected_channels: &[1],
            expected_skip: None,
        },
    ];

    for case in cases {
        let geometry = Geometry {
            retention_frames: 2,
            channels: 4,
            chunk_frames: 2,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 6,
            maximum_path_bytes: 512,
        };
        let (mut arena, ingress, plan) = runtime(geometry);
        let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = exact_zero_policy(root.path(), &case.modes);
        let mut decision = FrozenExportDecision::new(&plan).unwrap();
        let frozen = freeze(&mut arena, &ingress, &case.samples, 4);
        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "matrix",
                    staging_root: &root.path().join("staging"),
                    timestamp: TIMESTAMP,
                    decision: &mut decision,
                },
            )
            .unwrap();
        match case.expected_skip {
            Some(true) => assert!(matches!(prepared, PreparedPersistence::SkippedByPolicy)),
            Some(false) => assert!(matches!(prepared, PreparedPersistence::SkippedSilent)),
            None => {
                let files = prepared.files().unwrap();
                let actual: Vec<_> = (0..files.len())
                    .map(|index| files.get(index).unwrap().channel())
                    .collect();
                assert_eq!(actual, case.expected_channels);
            }
        }
        drop(prepared);
        arena.shutdown(DEADLINE).unwrap();
    }

    for non_finite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let geometry = Geometry {
            retention_frames: 1,
            channels: 1,
            chunk_frames: 1,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 3,
            maximum_path_bytes: 512,
        };
        let (mut arena, ingress, plan) = runtime(geometry);
        let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
        let root = tempfile::tempdir().unwrap();
        let policy = exact_zero_policy(root.path(), &[ChannelExportMode::Auto]);
        let mut decision = FrozenExportDecision::new(&plan).unwrap();
        let frozen = freeze(&mut arena, &ingress, &[non_finite], 1);
        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "matrix",
                    staging_root: &root.path().join("staging"),
                    timestamp: TIMESTAMP,
                    decision: &mut decision,
                },
            )
            .unwrap();
        assert_eq!(prepared.files().unwrap().get(0).unwrap().channel(), 0);
        assert_eq!(decision.channels()[0].result, ActivityResult::Ambiguous);
        assert_eq!(
            decision.channels()[0].disposition,
            ChannelDisposition::Retain
        );
        drop(prepared);
        arena.shutdown(DEADLINE).unwrap();
    }
}

#[test]
fn maximum_range_reuses_all_startup_workspace_allocations() {
    let geometry = Geometry {
        retention_frames: 12,
        channels: 2,
        chunk_frames: 3,
        split_when_over_bytes: 80,
        io_buffer_bytes_per_channel: 9,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let addresses = workspace.allocation_addresses();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let names = ["left".to_string(), "right".to_string()];

    let mut frozen = freeze(&mut arena, &ingress, &[0.25; 6], 2);
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &names,
            },
        )
        .unwrap();
    assert_eq!(prepared.files().unwrap().len(), 2);
    drop(prepared);
    assert_eq!(workspace.allocation_addresses(), addresses);
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();

    let samples: Vec<f32> = (0..24).map(|value| value as f32 / 24.0).collect();
    let frozen = freeze(&mut arena, &ingress, &samples, 2);
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &names,
            },
        )
        .unwrap();
    assert!(matches!(&prepared, PreparedPersistence::Recall { .. }));
    assert_eq!(prepared.files().unwrap().len(), 2);
    drop(prepared);

    assert_eq!(workspace.allocation_addresses(), addresses);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn every_workspace_geometry_mismatch_rejects_before_allocation_callback() {
    let geometry = Geometry {
        retention_frames: 8,
        channels: 2,
        chunk_frames: 4,
        split_when_over_bytes: 80,
        io_buffer_bytes_per_channel: 12,
        maximum_path_bytes: 256,
    };
    let plan = plan(geometry);
    let expected = workspace_config(geometry);
    let mismatches = [
        PersistenceWorkspaceConfig {
            retention_frames: 9,
            ..expected
        },
        PersistenceWorkspaceConfig {
            channels: 1,
            ..expected
        },
        PersistenceWorkspaceConfig {
            sample_rate: 44_100,
            ..expected
        },
        PersistenceWorkspaceConfig {
            sample_format: SampleFormat::F32Le,
            sample_bytes: 8,
            ..expected
        },
        PersistenceWorkspaceConfig {
            chunk_frames: 2,
            ..expected
        },
        PersistenceWorkspaceConfig {
            split_when_over_bytes: 79,
            ..expected
        },
        PersistenceWorkspaceConfig {
            io_buffer_bytes_per_channel: 9,
            ..expected
        },
        PersistenceWorkspaceConfig {
            maximum_path_bytes: 255,
            ..expected
        },
    ];

    for mismatch in mismatches {
        let called = Cell::new(false);
        let result: Result<(), _> =
            PersistenceWorkspace::allocate_validated(&plan, &mismatch, || {
                called.set(true);
                Ok(())
            });
        assert!(result.is_err());
        assert!(!called.get());
    }
}

#[test]
fn unrepresentable_part_and_path_indexes_reject_before_allocation_callback() {
    let geometry = Geometry {
        retention_frames: u64::from(u32::MAX) + 1,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 47,
        io_buffer_bytes_per_channel: 3,
        maximum_path_bytes: 1,
    };
    let plan = plan(geometry);
    let called = Cell::new(false);

    let result: Result<(), _> =
        PersistenceWorkspace::allocate_validated(&plan, &workspace_config(geometry), || {
            called.set(true);
            Ok(())
        });

    assert!(result.is_err());
    assert!(!called.get());
}

#[test]
fn multichannel_split_wavs_have_exact_headers_boundaries_names_and_samples() {
    let geometry = Geometry {
        retention_frames: 5,
        channels: 2,
        chunk_frames: 2,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let samples = [0.0, -1.0, 0.5, -0.5, 1.0, 0.25, -0.25, 0.75, 0.125, -0.125];
    let frozen = freeze(&mut arena, &ingress, &samples, 2);
    let root = tempfile::tempdir().unwrap();
    let staging_root = root.path().join("staging");
    let output = root.path().join("output");
    let names = ["left".to_string(), "right".to_string()];

    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging_root,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &names,
            },
        )
        .unwrap();
    let files = prepared.files().unwrap();
    assert_eq!(files.len(), 6);
    let expected_names = [
        "lamb-20260818T120000-left-48000Hz-000000000-000000002-part001.wav",
        "lamb-20260818T120000-left-48000Hz-000000002-000000004-part002.wav",
        "lamb-20260818T120000-left-48000Hz-000000004-000000005-part003.wav",
        "lamb-20260818T120000-right-48000Hz-000000000-000000002-part001.wav",
        "lamb-20260818T120000-right-48000Hz-000000002-000000004-part002.wav",
        "lamb-20260818T120000-right-48000Hz-000000004-000000005-part003.wav",
    ];
    for (index, expected) in expected_names.iter().enumerate() {
        let file = files.get(index).unwrap();
        assert_eq!(file.staged_path().file_name().unwrap(), *expected);
        assert_eq!(file.final_path(), output.join(expected));
        let bytes = wav_bytes(file.staged_path());
        let frames = if index % 3 == 2 { 1 } else { 2 };
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            36 + frames * 3
        );
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            48_000
        );
        assert_eq!(
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            144_000
        );
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 3);
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 24);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            frames * 3
        );
    }
    let left_first = wav_bytes(files.get(0).unwrap().staged_path());
    let right_first = wav_bytes(files.get(3).unwrap().staged_path());
    assert_eq!(s24(&left_first[44..47]), 0);
    assert_eq!(s24(&left_first[47..50]), 4_194_304);
    assert_eq!(s24(&right_first[44..47]), -8_388_608);
    assert_eq!(s24(&right_first[47..50]), -4_194_304);
    drop(prepared);

    assert!(fs::read_dir(&staging_root).unwrap().next().is_none());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn maximum_split_slot_count_is_materialized_and_usable() {
    let geometry = Geometry {
        retention_frames: 8,
        channels: 2,
        chunk_frames: 4,
        split_when_over_bytes: 47,
        io_buffer_bytes_per_channel: 3,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 16], 2);
    let root = tempfile::tempdir().unwrap();
    let names = ["left".to_string(), "right".to_string()];

    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Dump {
                output_parent: root.path(),
                timestamp: TIMESTAMP,
                channel_names: &names,
            },
        )
        .unwrap();
    let files = prepared.files().unwrap();
    assert_eq!(files.len(), 16);
    assert_eq!(files.capacity(), 16);
    assert_eq!(
        files.get(15).unwrap().final_path(),
        root.path().join(TIMESTAMP).join("right-part008.wav")
    );
    drop(prepared);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn exact_zero_and_negative_zero_are_silent_but_nan_is_active() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let mut frozen = freeze(&mut arena, &ingress, &[0.0, -0.0, 0.0, -0.0], 1);

    let silent = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    assert!(matches!(silent, PreparedPersistence::Silent));
    assert!(!output.exists());
    assert!(fs::read_dir(&staging).unwrap().next().is_none());
    drop(silent);

    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.0, f32::NAN], 1);
    let active = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    assert!(matches!(&active, PreparedPersistence::Recall { .. }));
    assert_eq!(active.files().unwrap().len(), 1);
    drop(active);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn traversal_duplicates_and_path_capacity_fail_before_any_wav_is_opened() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 2,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 128,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.5; 8], 2);
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");

    for names in [
        vec!["../left".to_string(), "right".to_string()],
        vec!["same".to_string(), "same".to_string()],
        vec!["x".repeat(200), "right".to_string()],
    ] {
        assert!(workspace
            .prepare(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &names,
                },
            )
            .is_err());
        assert!(!output.exists());
        assert!(!staging.exists());
    }
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn recall_preserves_legacy_safe_punctuation_in_detailed_basename() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    let names = [".".to_string()];

    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &root.path().join("staging"),
                output_dir: &root.path().join("output"),
                timestamp: "",
                channel_names: &names,
            },
        )
        .unwrap();

    assert_eq!(
        prepared
            .files()
            .unwrap()
            .get(0)
            .unwrap()
            .final_path()
            .file_name()
            .unwrap(),
        "lamb--.-48000Hz-000000000-000000002-part001.wav"
    );
    drop(prepared);
    arena.shutdown(DEADLINE).unwrap();
}

struct FailingIo {
    fail_write_once: bool,
    fail_sync_once: bool,
}

#[derive(Default)]
struct CleanupFaultState {
    fail_metadata_from_call: Option<usize>,
    fail_remove: bool,
    metadata_calls: usize,
    remove_calls: usize,
}

struct InjectedCleanupIo {
    state: Arc<Mutex<CleanupFaultState>>,
}

impl CleanupIo for InjectedCleanupIo {
    fn symlink_metadata(&mut self, path: &Path) -> io::Result<fs::Metadata> {
        let mut state = self.state.lock().unwrap();
        state.metadata_calls += 1;
        if state
            .fail_metadata_from_call
            .is_some_and(|first| state.metadata_calls >= first)
        {
            return Err(io::Error::other("injected cleanup metadata failure"));
        }
        drop(state);
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.remove_calls += 1;
        if state.fail_remove {
            return Err(io::Error::other("injected cleanup remove failure"));
        }
        drop(state);
        fs::remove_dir_all(path)
    }
}

#[derive(Default)]
struct CountingIo {
    writes: usize,
    headers: usize,
    opened: Vec<PathBuf>,
}

impl WavIo for CountingIo {
    fn open(&mut self, path: &Path) -> io::Result<File> {
        self.opened.push(path.to_path_buf());
        File::options().write(true).create_new(true).open(path)
    }

    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        self.writes += 1;
        self.headers += usize::from(bytes.len() == 44 && bytes.starts_with(b"RIFF"));
        file.write_all(bytes)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        file.flush()
    }

    fn sync_all(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
}

#[test]
fn sparse_split_opens_only_dense_retained_channel_part_outputs() {
    let geometry = Geometry {
        retention_frames: 6,
        channels: 4,
        chunk_frames: 3,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 9,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let policy = activity_policy(
        &root.path().join("output"),
        &[
            ChannelExportMode::Auto,
            ChannelExportMode::Never,
            ChannelExportMode::Auto,
            ChannelExportMode::Auto,
        ],
        custom_layout("nested/{channel}", "part-{part}.wav"),
        false,
    );
    let samples = [
        0.5, 1.0, 0.25, 0.0, 0.4, 1.0, 0.2, -0.0, 0.3, 1.0, 0.1, 0.0, 0.2, 1.0, 0.05, -0.0, 0.1,
        1.0, 0.025, 0.0, 0.05, 1.0, 0.0125, -0.0,
    ];
    let frozen = freeze(&mut arena, &ingress, &samples, 4);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut io = CountingIo::default();
    let prepared = workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "split",
                staging_root: &root.path().join("staging"),
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
            &mut io,
        )
        .unwrap();

    let files = prepared.files().unwrap();
    assert_eq!(files.len(), 6);
    let channels: Vec<_> = (0..files.len())
        .map(|index| files.get(index).unwrap().channel())
        .collect();
    let parts: Vec<_> = (0..files.len())
        .map(|index| files.get(index).unwrap().part())
        .collect();
    assert_eq!(channels, [0, 0, 0, 2, 2, 2]);
    assert_eq!(parts, [1, 2, 3, 1, 2, 3]);
    assert_eq!(io.opened.len(), files.len());
    assert_eq!(io.headers, files.len());
    let mut planned_paths: Vec<_> = (0..files.len())
        .map(|index| files.get(index).unwrap().staged_path().to_path_buf())
        .collect();
    planned_paths.sort();
    io.opened.sort();
    assert_eq!(io.opened, planned_paths);
    for index in 0..files.len() {
        let file = files.get(index).unwrap();
        assert_eq!(file.start_frame(), u64::from((index % 3) as u32 * 2));
        assert_eq!(file.frame_count(), 2);
        assert!(file.staged_path().exists());
    }
    assert_eq!(
        decision
            .channels()
            .iter()
            .filter(|channel| channel.disposition == ChannelDisposition::Omit)
            .count(),
        2
    );
    drop(prepared);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn frozen_decision_cannot_be_reused_for_a_same_geometry_epoch_from_another_runtime() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut first_arena, first_ingress, plan) = runtime(geometry);
    let (mut second_arena, second_ingress, _) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let policy = exact_zero_policy(&root.path().join("output"), &[ChannelExportMode::Auto]);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let first = freeze(&mut first_arena, &first_ingress, &[0.5, 0.25], 1);
    let second = freeze(&mut second_arena, &second_ingress, &[0.5, 0.25], 1);

    let prepared = workspace
        .prepare(
            &first,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "identity",
                staging_root: &root.path().join("first-staging"),
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();
    drop(prepared);
    assert!(decision.valid());

    let second_staging = root.path().join("second-staging");
    let mut io = CountingIo::default();
    let error = match workspace.prepare_with_io(
        &second,
        PrepareRequest::Policy {
            command: ExportCommand::Recall,
            policy: &policy,
            profile: "identity",
            staging_root: &second_staging,
            timestamp: TIMESTAMP,
            decision: &mut decision,
        },
        &mut io,
    ) {
        Ok(_) => panic!("decision from another runtime was accepted"),
        Err(error) => error,
    };

    assert!(matches!(error, LambError::ExportInvariant(_)));
    assert!(decision.valid());
    assert!(io.opened.is_empty());
    assert!(!second_staging.exists());
    first_arena.shutdown(DEADLINE).unwrap();
    second_arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn common_crop_wavs_preserve_staggered_onsets_and_untrimmed_tail() {
    const SAMPLE_RATE: usize = 48_000;
    const PREROLL: usize = 2 * SAMPLE_RATE;
    const TOTAL_FRAMES: usize = PREROLL + 10;
    const MIC_ONSET: usize = PREROLL + 3;
    const GUITAR_ONSET: usize = PREROLL + 6;
    const CROP_START: u64 = 3;
    const ENCODED_FRAMES: u64 = TOTAL_FRAMES as u64 - CROP_START;

    let geometry = Geometry {
        retention_frames: TOTAL_FRAMES as u64,
        channels: 2,
        chunk_frames: TOTAL_FRAMES as u32,
        split_when_over_bytes: 1_000_000,
        io_buffer_bytes_per_channel: 12_288,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let policy = activity_policy(
        &root.path().join("output"),
        &[ChannelExportMode::Auto; 2],
        custom_layout("crop", "{channel}.wav"),
        true,
    );
    let mut samples = vec![0.0; TOTAL_FRAMES * 2];
    samples[MIC_ONSET * 2] = 0.5;
    samples[GUITAR_ONSET * 2 + 1] = -0.5;
    let frozen = freeze(&mut arena, &ingress, &samples, 2);
    let consumed_range = frozen.absolute_range();
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "crop",
                staging_root: &root.path().join("staging"),
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();

    assert_eq!(consumed_range, 0..TOTAL_FRAMES as u64);
    assert_eq!(decision.export_range(), CROP_START..TOTAL_FRAMES as u64);
    assert!(consumed_range.end - consumed_range.start > ENCODED_FRAMES);
    let files = prepared.files().unwrap();
    assert_eq!(files.len(), 2);
    let expected_onsets = [PREROLL as u64, (PREROLL + 3) as u64];
    for (index, expected_onset) in expected_onsets.into_iter().enumerate() {
        let file = files.get(index).unwrap();
        assert_eq!(file.start_frame(), CROP_START);
        assert_eq!(file.frame_count(), ENCODED_FRAMES);
        let bytes = wav_bytes(file.staged_path());
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            ENCODED_FRAMES as u32 * 3
        );
        let decoded: Vec<_> = bytes[44..].chunks_exact(3).map(s24).collect();
        assert_eq!(decoded.len(), ENCODED_FRAMES as usize);
        assert!(decoded[..expected_onset as usize]
            .iter()
            .all(|sample| *sample == 0));
        assert_ne!(decoded[expected_onset as usize], 0);
        assert!(decoded[expected_onset as usize + 1..]
            .iter()
            .all(|sample| *sample == 0));
        assert_eq!(decoded.last(), Some(&0), "tail after evidence was trimmed");
    }
    assert_eq!(expected_onsets[1] - expected_onsets[0], 3);
    drop(prepared);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn canonical_preflight_blocks_staging_and_open_then_reuses_frozen_decision_on_retry() {
    assert!(ValidatedPattern::parse("{channel").is_err());
    assert!(ValidatedPattern::parse("{unknown}.wav").is_err());

    for failure in 0..3 {
        let split = failure != 1;
        let geometry = Geometry {
            retention_frames: 4,
            channels: 2,
            chunk_frames: 4,
            split_when_over_bytes: if split { 50 } else { 1_000 },
            io_buffer_bytes_per_channel: 12,
            maximum_path_bytes: 128,
        };
        let (mut arena, ingress, plan) = runtime(geometry);
        let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let staging = root.path().join("staging");
        let failing_layout = match failure {
            0 => custom_layout("", "same.wav"),
            1 => custom_layout("", "{part}.wav"),
            2 => custom_layout("", &format!("{}.wav", "x".repeat(120))),
            _ => unreachable!(),
        };
        let failing_policy =
            activity_policy(&output, &[ChannelExportMode::Auto; 2], failing_layout, true);
        let samples = [0.0, 0.0, 0.5, 0.0, 0.0, 0.25, 0.0, 0.0];
        let frozen = freeze(&mut arena, &ingress, &samples, 2);
        let mut decision = FrozenExportDecision::new(&plan).unwrap();
        let mut failing_io = CountingIo::default();
        let result = workspace.prepare_with_io(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &failing_policy,
                profile: "preflight",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
            &mut failing_io,
        );
        assert!(
            result.is_err(),
            "preflight case {failure} unexpectedly succeeded"
        );
        drop(result);
        assert!(decision.valid());
        let frozen_range = decision.export_range();
        let frozen_channels = decision.channels().to_vec();
        assert!(
            !staging.exists(),
            "case {failure} created staging before failure"
        );
        assert_eq!(failing_io.opened.len(), 0, "case {failure} opened a WAV");
        assert_eq!(failing_io.headers, 0, "case {failure} wrote a WAV header");

        let corrected = activity_policy(
            &output,
            &[ChannelExportMode::Never; 2],
            custom_layout("nested/{profile}/{channel}", "part-{part}.wav"),
            false,
        );
        let mut retry_io = CountingIo::default();
        let prepared = workspace
            .prepare_with_io(
                &frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &corrected,
                    profile: "preflight",
                    staging_root: &staging,
                    timestamp: TIMESTAMP,
                    decision: &mut decision,
                },
                &mut retry_io,
            )
            .unwrap();
        assert_eq!(decision.export_range(), frozen_range);
        assert_eq!(decision.channels(), frozen_channels);
        let files = prepared.files().unwrap();
        assert_eq!(files.len(), if split { 4 } else { 2 });
        assert_eq!(retry_io.opened.len(), files.len());
        assert_eq!(retry_io.headers, files.len());
        for index in 0..files.len() {
            let file = files.get(index).unwrap();
            assert!(file
                .final_path()
                .starts_with(output.join("nested/preflight")));
            assert!(matches!(file.channel(), 0 | 1));
        }
        drop(prepared);
        arena.shutdown(DEADLINE).unwrap();
    }
}

#[test]
fn fileset_existing_nested_final_fails_before_staging_or_wav_open() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    let final_path = output.join("nested").join("foreign.wav");
    fs::create_dir_all(final_path.parent().unwrap()).unwrap();
    fs::write(&final_path, b"foreign collision").unwrap();
    let policy = activity_policy(
        &output,
        &[ChannelExportMode::Always],
        custom_layout("nested", "foreign.wav"),
        false,
    );
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Policy {
            command: ExportCommand::Recall,
            policy: &policy,
            profile: "preflight",
            staging_root: &staging,
            timestamp: TIMESTAMP,
            decision: &mut decision,
        },
        &mut io,
    );

    assert!(result.is_err(), "existing final must reject preparation");
    drop(result);
    assert_eq!(fs::read(&final_path).unwrap(), b"foreign collision");
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(!staging.exists());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn legacy_recall_existing_final_fails_before_staging_or_wav_open() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    let final_path =
        output.join("lamb-20260818T120000-mic-48000Hz-000000000-000000002-part001.wav");
    fs::create_dir(&output).unwrap();
    fs::write(&final_path, b"foreign legacy recall").unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let names = ["mic".to_string()];
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Recall {
            staging_root: &staging,
            output_dir: &output,
            timestamp: TIMESTAMP,
            channel_names: &names,
        },
        &mut io,
    );

    assert!(
        result.is_err(),
        "legacy Recall collision must reject preparation"
    );
    drop(result);
    assert_eq!(fs::read(&final_path).unwrap(), b"foreign legacy recall");
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(!staging.exists());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn legacy_dump_existing_final_directory_fails_before_hidden_staging_or_wav_open() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let final_directory = root.path().join(TIMESTAMP);
    fs::create_dir(&final_directory).unwrap();
    fs::write(final_directory.join("foreign"), b"foreign legacy dump").unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let names = ["mic".to_string()];
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Dump {
            output_parent: root.path(),
            timestamp: TIMESTAMP,
            channel_names: &names,
        },
        &mut io,
    );

    assert!(
        result.is_err(),
        "legacy Dump collision must reject preparation"
    );
    drop(result);
    assert_eq!(
        fs::read(final_directory.join("foreign")).unwrap(),
        b"foreign legacy dump"
    );
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-lamb-")
    }));
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn legacy_recall_absolute_symlink_ancestor_fails_before_staging_or_wav_open() {
    use std::os::unix::fs::symlink;

    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 512,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("marker"), b"unchanged legacy target").unwrap();
    let redirect = root.path().join("redirect");
    symlink(&target, &redirect).unwrap();
    let output = redirect.join("output");
    let staging = root.path().join("staging");
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let names = ["mic".to_string()];
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Recall {
            staging_root: &staging,
            output_dir: &output,
            timestamp: TIMESTAMP,
            channel_names: &names,
        },
        &mut io,
    );

    assert!(
        result.is_err(),
        "legacy Recall symlink ancestor must reject preparation"
    );
    drop(result);
    assert_eq!(
        fs::read(target.join("marker")).unwrap(),
        b"unchanged legacy target"
    );
    assert!(!target.join("output").exists());
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(!staging.exists());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn atomic_directory_existing_final_fails_before_staging_or_wav_open() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("output");
    let staging = root.path().join("staging");
    let final_directory = output.join(TIMESTAMP);
    fs::create_dir_all(&final_directory).unwrap();
    fs::write(final_directory.join("foreign"), b"preserve").unwrap();
    let policy = activity_policy(
        &output,
        &[ChannelExportMode::Always],
        ResolvedLayout::TimestampDirectory,
        false,
    );
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Policy {
            command: ExportCommand::Dump,
            policy: &policy,
            profile: "preflight",
            staging_root: &staging,
            timestamp: TIMESTAMP,
            decision: &mut decision,
        },
        &mut io,
    );

    assert!(
        result.is_err(),
        "existing final directory must reject preparation"
    );
    drop(result);
    assert_eq!(
        fs::read(final_directory.join("foreign")).unwrap(),
        b"preserve"
    );
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(!staging.exists());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn fileset_absolute_symlink_ancestor_fails_before_staging_or_wav_open() {
    use std::os::unix::fs::symlink;

    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("attacker-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("marker"), b"unchanged").unwrap();
    let symlink_ancestor = root.path().join("redirect");
    symlink(&target, &symlink_ancestor).unwrap();
    let output = symlink_ancestor.join("configured-output");
    let staging = root.path().join("staging");
    let policy = activity_policy(
        &output,
        &[ChannelExportMode::Always],
        custom_layout("nested", "mic.wav"),
        false,
    );
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut io = CountingIo::default();

    let result = workspace.prepare_with_io(
        &frozen,
        PrepareRequest::Policy {
            command: ExportCommand::Recall,
            policy: &policy,
            profile: "preflight",
            staging_root: &staging,
            timestamp: TIMESTAMP,
            decision: &mut decision,
        },
        &mut io,
    );

    assert!(
        result.is_err(),
        "absolute symlink ancestor must reject preparation"
    );
    drop(result);
    assert_eq!(fs::read(target.join("marker")).unwrap(), b"unchanged");
    assert!(!target.join("configured-output").exists());
    assert!(io.opened.is_empty());
    assert_eq!(io.headers, 0);
    assert!(!staging.exists());
    arena.shutdown(DEADLINE).unwrap();
}

fn workspace_with_cleanup_faults(
    plan: &SessionMemoryPlan,
    geometry: Geometry,
) -> (PersistenceWorkspace, Arc<Mutex<CleanupFaultState>>) {
    let state = Arc::new(Mutex::new(CleanupFaultState::default()));
    let workspace = PersistenceWorkspace::new_with_cleanup_io(
        plan,
        workspace_config(geometry),
        Box::new(InjectedCleanupIo {
            state: Arc::clone(&state),
        }),
    )
    .unwrap();
    (workspace, state)
}

#[test]
fn silent_cleanup_failure_is_pending_and_retried_before_reencoding() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.0; 4], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    faults.lock().unwrap().fail_metadata_from_call = Some(2);

    let result = workspace.prepare(
        &frozen,
        PrepareRequest::Recall {
            staging_root: &staging,
            output_dir: &output,
            timestamp: TIMESTAMP,
            channel_names: &[],
        },
    );
    assert!(result.is_err(), "cleanup failure must not return Silent");
    drop(result);
    let pending = *workspace.pending_cleanup().unwrap();
    assert_ne!(pending.inode(), Some(0));
    let pending_path = workspace.pending_cleanup_path().unwrap().to_path_buf();
    assert!(pending_path.exists());

    let blocked_staging = root.path().join("must-not-replace-pending-staging");
    let blocked_output = root.path().join("must-not-replace-pending-output");
    let mut blocked_io = CountingIo::default();
    assert!(workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &blocked_staging,
                output_dir: &blocked_output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut blocked_io,
        )
        .is_err());
    assert_eq!(
        blocked_io.writes, 0,
        "pending cleanup must run before encoding"
    );
    assert_eq!(workspace.pending_cleanup(), Some(&pending));
    assert_eq!(
        workspace.pending_cleanup_path(),
        Some(pending_path.as_path())
    );
    assert!(!blocked_staging.exists());
    assert!(!blocked_output.exists());

    faults.lock().unwrap().fail_metadata_from_call = None;
    let recovered = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    assert!(matches!(&recovered, PreparedPersistence::Silent));
    drop(recovered);
    assert!(workspace.pending_cleanup().is_none());
    assert!(!pending_path.exists());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn write_or_sync_error_preserves_operation_and_pending_cleanup_context() {
    for (fail_write_once, fail_sync_once, expected) in [
        (true, false, "injected write failure"),
        (false, true, "injected sync failure"),
    ] {
        let geometry = Geometry {
            retention_frames: 4,
            channels: 1,
            chunk_frames: 2,
            split_when_over_bytes: 1_000,
            io_buffer_bytes_per_channel: 6,
            maximum_path_bytes: 256,
        };
        let (mut arena, ingress, plan) = runtime(geometry);
        let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
        let frozen = freeze(&mut arena, &ingress, &[0.25; 4], 1);
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        let output = root.path().join("output");
        faults.lock().unwrap().fail_remove = true;
        let mut failing_io = FailingIo {
            fail_write_once,
            fail_sync_once,
        };

        let error = workspace
            .prepare_with_io(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
                &mut failing_io,
            )
            .err()
            .expect("preparation and cleanup must fail");
        match error {
            LambError::PersistenceCleanup { operation, cleanup } => {
                assert!(operation.to_string().contains(expected));
                assert!(cleanup
                    .to_string()
                    .contains("injected cleanup remove failure"));
            }
            other => panic!("expected combined persistence cleanup error, got {other}"),
        }
        let pending_path = workspace.pending_cleanup_path().unwrap().to_path_buf();
        assert!(pending_path.exists());

        let mut blocked_io = CountingIo::default();
        assert!(workspace
            .prepare_with_io(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
                &mut blocked_io,
            )
            .is_err());
        assert_eq!(blocked_io.writes, 0);

        faults.lock().unwrap().fail_remove = false;
        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
            )
            .unwrap();
        assert!(matches!(&prepared, PreparedPersistence::Recall { .. }));
        assert!(!pending_path.exists());
        drop(prepared);
        assert!(workspace.pending_cleanup().is_none());
        arena.shutdown(DEADLINE).unwrap();
    }
}

#[test]
fn initial_identity_and_removal_failure_retains_unidentified_path_without_overwrite() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = Some(1);
        state.fail_remove = true;
    }
    let mut wav_io = CountingIo::default();

    let error = workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut wav_io,
        )
        .err()
        .expect("initial identity and cleanup must fail");
    match error {
        LambError::PersistenceCleanup { operation, cleanup } => {
            assert!(operation
                .to_string()
                .contains("injected cleanup metadata failure"));
            assert!(cleanup
                .to_string()
                .contains("injected cleanup remove failure"));
        }
        other => panic!("expected combined initial cleanup error, got {other}"),
    }
    assert_eq!(wav_io.writes, 0);
    assert!(matches!(
        workspace.pending_cleanup(),
        Some(PendingCleanup::Unidentified { .. })
    ));
    let pending_path = workspace.pending_cleanup_path().unwrap().to_path_buf();
    assert!(pending_path.exists());
    let calls = faults.lock().unwrap();
    assert_eq!(calls.metadata_calls, 1);
    assert_eq!(calls.remove_calls, 1);
    drop(calls);

    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = None;
        state.fail_remove = false;
    }
    let blocked_staging = root.path().join("must-not-overwrite-unidentified-staging");
    let blocked_output = root.path().join("must-not-overwrite-unidentified-output");
    let mut blocked_io = CountingIo::default();
    let error = workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &blocked_staging,
                output_dir: &blocked_output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut blocked_io,
        )
        .err()
        .expect("existing unidentified path must block preparation");
    assert!(matches!(
        error,
        LambError::UnidentifiedStagingCleanup { .. }
    ));
    assert_eq!(blocked_io.writes, 0);
    assert_eq!(
        workspace.pending_cleanup_path(),
        Some(pending_path.as_path())
    );
    assert!(!blocked_staging.exists());
    assert!(!blocked_output.exists());
    assert_eq!(faults.lock().unwrap().remove_calls, 1);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn unidentified_foreign_replacement_is_never_removed_or_reidentified() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = Some(1);
        state.fail_remove = true;
    }
    let mut wav_io = CountingIo::default();
    assert!(workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &root.path().join("staging"),
                output_dir: &root.path().join("output"),
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut wav_io,
        )
        .is_err());
    let unidentified_path = workspace.pending_cleanup_path().unwrap().to_path_buf();
    fs::remove_dir_all(&unidentified_path).unwrap();
    fs::create_dir(&unidentified_path).unwrap();
    fs::write(unidentified_path.join("foreign"), b"preserve").unwrap();
    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = None;
        state.fail_remove = false;
    }

    let mut blocked_io = CountingIo::default();
    let error = workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &root.path().join("other-staging"),
                output_dir: &root.path().join("other-output"),
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut blocked_io,
        )
        .err()
        .expect("foreign replacement must require manual recovery");
    assert!(matches!(
        error,
        LambError::UnidentifiedStagingCleanup { .. }
    ));
    assert_eq!(blocked_io.writes, 0);
    assert_eq!(
        fs::read(unidentified_path.join("foreign")).unwrap(),
        b"preserve"
    );
    assert!(matches!(
        workspace.pending_cleanup(),
        Some(PendingCleanup::Unidentified { .. })
    ));
    assert_eq!(faults.lock().unwrap().remove_calls, 1);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn unidentified_cleanup_recovers_only_after_path_is_absent() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = Some(1);
        state.fail_remove = true;
    }
    let mut wav_io = CountingIo::default();
    assert!(workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut wav_io,
        )
        .is_err());
    let unidentified_path = workspace.pending_cleanup_path().unwrap().to_path_buf();
    fs::remove_dir_all(&unidentified_path).unwrap();
    {
        let mut state = faults.lock().unwrap();
        state.fail_metadata_from_call = None;
        state.fail_remove = false;
    }

    let recovered = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    let new_path = recovered.staging_directory().unwrap().to_path_buf();
    assert_ne!(new_path, unidentified_path);
    assert!(!unidentified_path.exists());
    drop(recovered);
    assert!(workspace.pending_cleanup().is_none());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn prepared_drop_failure_becomes_pending_until_next_prepare_recovers() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 4], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    let staged = prepared.staging_directory().unwrap().to_path_buf();
    faults.lock().unwrap().fail_remove = true;
    drop(prepared);

    assert!(workspace.pending_cleanup().is_some());
    assert_eq!(workspace.pending_cleanup_path(), Some(staged.as_path()));
    assert!(staged.exists());
    let mut blocked_io = CountingIo::default();
    assert!(workspace
        .prepare_with_io(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
            &mut blocked_io,
        )
        .is_err());
    assert_eq!(blocked_io.writes, 0);

    faults.lock().unwrap().fail_remove = false;
    let recovered = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    assert!(!staged.exists());
    drop(recovered);
    assert!(workspace.pending_cleanup().is_none());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn foreign_inode_replacement_completes_cleanup_without_deleting_foreign_data() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, _faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &root.path().join("staging"),
                output_dir: &root.path().join("output"),
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    let staged = prepared.staging_directory().unwrap().to_path_buf();
    let original_directory = File::open(&staged).unwrap();
    fs::remove_dir_all(&staged).unwrap();
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("foreign"), b"preserve").unwrap();

    drop(prepared);

    assert!(workspace.pending_cleanup().is_none());
    assert_eq!(fs::read(staged.join("foreign")).unwrap(), b"preserve");
    drop(original_directory);
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn missing_owned_staging_path_completes_cleanup_and_allows_reuse() {
    let geometry = Geometry {
        retention_frames: 2,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let (mut workspace, _faults) = workspace_with_cleanup_faults(&plan, geometry);
    let frozen = freeze(&mut arena, &ingress, &[0.25; 2], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    let staged = prepared.staging_directory().unwrap().to_path_buf();
    fs::remove_dir_all(&staged).unwrap();

    drop(prepared);

    assert!(workspace.pending_cleanup().is_none());
    let reused = workspace
        .prepare(
            &frozen,
            PrepareRequest::Recall {
                staging_root: &staging,
                output_dir: &output,
                timestamp: TIMESTAMP,
                channel_names: &[],
            },
        )
        .unwrap();
    drop(reused);
    arena.shutdown(DEADLINE).unwrap();
}

impl WavIo for FailingIo {
    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        if self.fail_write_once && bytes.len() != 44 {
            self.fail_write_once = false;
            return Err(io::Error::other("injected write failure"));
        }
        file.write_all(bytes)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        file.flush()
    }

    fn sync_all(&mut self, file: &File) -> io::Result<()> {
        if self.fail_sync_once {
            self.fail_sync_once = false;
            return Err(io::Error::other("injected sync failure"));
        }
        file.sync_all()
    }
}

#[test]
fn write_and_sync_failures_clean_every_slot_and_leave_workspace_reusable() {
    let geometry = Geometry {
        retention_frames: 4,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 4], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");

    for mut io in [
        FailingIo {
            fail_write_once: true,
            fail_sync_once: false,
        },
        FailingIo {
            fail_write_once: false,
            fail_sync_once: true,
        },
    ] {
        assert!(workspace
            .prepare_with_io(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
                &mut io,
            )
            .is_err());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
        assert!(!output.exists());

        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
            )
            .unwrap();
        assert!(matches!(&prepared, PreparedPersistence::Recall { .. }));
        drop(prepared);
    }
    arena.shutdown(DEADLINE).unwrap();
}

fn measured_prepare(frames: u64) -> (usize, usize) {
    let geometry = Geometry {
        retention_frames: frames,
        channels: 1,
        chunk_frames: 4,
        split_when_over_bytes: 1_000_000,
        io_buffer_bytes_per_channel: 12,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let samples = vec![0.25; frames as usize];
    let frozen = freeze(&mut arena, &ingress, &samples, 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let policy = exact_zero_policy(&output, &[ChannelExportMode::Auto]);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();

    let (_, count, bytes) = allocation_count_during(|| {
        let prepared = workspace
            .prepare(
                &frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "test",
                    staging_root: &staging,
                    timestamp: TIMESTAMP,
                    decision: &mut decision,
                },
            )
            .unwrap();
        drop(prepared);
    });
    arena.shutdown(DEADLINE).unwrap();
    (count, bytes)
}

#[test]
fn bounded_operation_allocations_do_not_scale_with_selected_frame_count() {
    let small = measured_prepare(4);
    let maximum = measured_prepare(4_096);
    assert_eq!(
        maximum.0, small.0,
        "allocation count grew: {small:?} -> {maximum:?}"
    );
    assert_eq!(
        maximum.1, small.1,
        "allocated bytes grew: {small:?} -> {maximum:?}"
    );
    assert!(
        maximum.0 <= 2,
        "bounded OS/path allocations changed: {maximum:?}"
    );
    assert!(
        maximum.1 <= 114,
        "bounded OS/path allocations changed: {maximum:?}"
    );
}

#[test]
fn startup_addresses_and_operation_allocations_are_stable_across_sparse_maximum_outputs() {
    let geometry = Geometry {
        retention_frames: 32,
        channels: 4,
        chunk_frames: 32,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 24,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let startup = workspace.allocation_addresses();
    assert_ne!(startup.publication_sync_slots, 0);
    assert_eq!(
        startup.publication_sync_slot_count,
        usize::try_from(plan.manifest_directory_slots()).unwrap()
    );
    assert_ne!(startup.publication_current_artifact, 0);
    assert_eq!(startup.publication_current_artifact_slots, 1);
    assert_ne!(startup.publication_component_a, 0);
    assert_eq!(
        startup.publication_component_a_capacity,
        geometry.maximum_path_bytes as usize + 1
    );
    assert_ne!(startup.publication_component_b, 0);
    assert_eq!(
        startup.publication_component_b_capacity,
        geometry.maximum_path_bytes as usize + 1
    );
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let output = root.path().join("output");
    let policy = exact_zero_policy(&output, &[ChannelExportMode::Auto; 4]);

    let mut small_decision = FrozenExportDecision::new(&plan).unwrap();
    let mut small_samples = vec![0.0; 2 * 4];
    small_samples[0] = 0.25;
    let mut small_frozen = freeze(&mut arena, &ingress, &small_samples, 4);
    let (small_prepared, small_count, small_bytes) = allocation_count_during(|| {
        workspace
            .prepare(
                &small_frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "allocation",
                    staging_root: &staging,
                    timestamp: TIMESTAMP,
                    decision: &mut small_decision,
                },
            )
            .unwrap()
    });
    assert_eq!(small_prepared.files().unwrap().len(), 1);
    drop(small_prepared);
    assert_eq!(workspace.allocation_addresses(), startup);
    arena.release_frozen(&mut small_frozen, DEADLINE).unwrap();

    let mut maximum_decision = FrozenExportDecision::new(&plan).unwrap();
    let maximum_samples = vec![0.25; 32 * 4];
    let maximum_frozen = freeze(&mut arena, &ingress, &maximum_samples, 4);
    let (maximum_prepared, maximum_count, maximum_bytes) = allocation_count_during(|| {
        workspace
            .prepare(
                &maximum_frozen,
                PrepareRequest::Policy {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "allocation",
                    staging_root: &staging,
                    timestamp: TIMESTAMP,
                    decision: &mut maximum_decision,
                },
            )
            .unwrap()
    });
    assert_eq!(maximum_prepared.files().unwrap().len(), 64);
    drop(maximum_prepared);
    assert_eq!(workspace.allocation_addresses(), startup);
    assert_eq!(
        (maximum_count, maximum_bytes),
        (small_count, small_bytes),
        "operation allocations scaled from sparse/small to dense/maximum"
    );
    assert!(maximum_count <= 2, "unexpected fixed allocation count");
    assert!(maximum_bytes <= 64, "unexpected fixed allocated bytes");
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn prepared_publication_allocations_do_not_scale_with_output_count() {
    let geometry = Geometry {
        retention_frames: 32,
        channels: 4,
        chunk_frames: 32,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 24,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let small_output = root.path().join("small-output");
    let maximum_output = root.path().join("large-output");
    let small_policy = exact_zero_policy(&small_output, &[ChannelExportMode::Auto; 4]);
    let maximum_policy = exact_zero_policy(&maximum_output, &[ChannelExportMode::Auto; 4]);

    let mut small_decision = FrozenExportDecision::new(&plan).unwrap();
    let mut small_samples = vec![0.0; 2 * 4];
    small_samples[0] = 0.25;
    let mut small_frozen = freeze(&mut arena, &ingress, &small_samples, 4);
    let small_prepared = workspace
        .prepare(
            &small_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &small_policy,
                profile: "allocation",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut small_decision,
            },
        )
        .unwrap();
    assert_eq!(small_prepared.files().unwrap().len(), 1);
    let (small_result, small_count, small_bytes) =
        allocation_count_during(|| publish_prepared(small_prepared));
    assert!(matches!(small_result, PreparedPublication::Published));
    arena.release_frozen(&mut small_frozen, DEADLINE).unwrap();

    let mut maximum_decision = FrozenExportDecision::new(&plan).unwrap();
    let maximum_samples = vec![0.25; 32 * 4];
    let maximum_frozen = freeze(&mut arena, &ingress, &maximum_samples, 4);
    let maximum_prepared = workspace
        .prepare(
            &maximum_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &maximum_policy,
                profile: "allocation",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut maximum_decision,
            },
        )
        .unwrap();
    assert_eq!(maximum_prepared.files().unwrap().len(), 64);
    let (maximum_result, maximum_count, maximum_bytes) =
        allocation_count_during(|| publish_prepared(maximum_prepared));
    assert!(matches!(maximum_result, PreparedPublication::Published));
    assert_eq!(
        (maximum_count, maximum_bytes),
        (small_count, small_bytes),
        "prepared publication allocations scaled from one output to the startup maximum"
    );
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn prepared_atomic_publication_allocations_do_not_scale_with_output_count() {
    let geometry = Geometry {
        retention_frames: 32,
        channels: 4,
        chunk_frames: 32,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 24,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    fs::create_dir(&staging).unwrap();
    // Equal-length roots keep path byte volume constant across the comparison.
    let small_output = root.path().join("small-output");
    let maximum_output = root.path().join("large-output");
    let small_policy = activity_policy(
        &small_output,
        &[ChannelExportMode::Auto; 4],
        ResolvedLayout::TimestampDirectory,
        false,
    );
    let maximum_policy = activity_policy(
        &maximum_output,
        &[ChannelExportMode::Auto; 4],
        ResolvedLayout::TimestampDirectory,
        false,
    );

    let mut small_decision = FrozenExportDecision::new(&plan).unwrap();
    let mut small_samples = vec![0.0; 2 * 4];
    small_samples[0] = 0.25;
    let mut small_frozen = freeze(&mut arena, &ingress, &small_samples, 4);
    let small_prepared = workspace
        .prepare(
            &small_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &small_policy,
                profile: "allocation",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut small_decision,
            },
        )
        .unwrap();
    assert!(matches!(
        small_prepared,
        PreparedPersistence::AtomicDirectory { .. }
    ));
    assert_eq!(small_prepared.files().unwrap().len(), 1);
    let (small_result, small_count, small_bytes) =
        allocation_count_during(|| publish_prepared(small_prepared));
    assert!(matches!(small_result, PreparedPublication::Published));
    arena.release_frozen(&mut small_frozen, DEADLINE).unwrap();

    let mut maximum_decision = FrozenExportDecision::new(&plan).unwrap();
    let maximum_samples = vec![0.25; 32 * 4];
    let maximum_frozen = freeze(&mut arena, &ingress, &maximum_samples, 4);
    let maximum_prepared = workspace
        .prepare(
            &maximum_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &maximum_policy,
                profile: "allocation",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut maximum_decision,
            },
        )
        .unwrap();
    assert!(matches!(
        maximum_prepared,
        PreparedPersistence::AtomicDirectory { .. }
    ));
    assert_eq!(maximum_prepared.files().unwrap().len(), 64);
    let (maximum_result, maximum_count, maximum_bytes) =
        allocation_count_during(|| publish_prepared(maximum_prepared));
    assert!(matches!(maximum_result, PreparedPublication::Published));
    assert_eq!(
        (maximum_count, maximum_bytes),
        (small_count, small_bytes),
        "prepared atomic publication allocations scaled from one output to the startup maximum"
    );
    arena.shutdown(DEADLINE).unwrap();
}

#[derive(Clone, Copy)]
enum ExpectedCheckpointRecovery {
    RolledBack,
    Complete,
}

fn assert_checkpoint_publication_allocations_do_not_scale(
    checkpoint: PublicationCheckpoint,
    layout: ResolvedLayout,
    expected: ExpectedCheckpointRecovery,
) {
    let geometry = Geometry {
        retention_frames: 32,
        channels: 4,
        chunk_frames: 32,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 24,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let mut workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    fs::create_dir(&staging).unwrap();
    // Equal-length roots make the path byte volume identical in both measured calls.
    let small_output = root.path().join("small-output");
    let maximum_output = root.path().join("large-output");
    let small_policy = activity_policy(
        &small_output,
        &[ChannelExportMode::Auto; 4],
        layout.clone(),
        false,
    );
    let maximum_policy = activity_policy(
        &maximum_output,
        &[ChannelExportMode::Auto; 4],
        layout,
        false,
    );

    let mut small_decision = FrozenExportDecision::new(&plan).unwrap();
    let mut small_samples = vec![0.0; 2 * 4];
    small_samples[0] = 0.25;
    let mut small_frozen = freeze(&mut arena, &ingress, &small_samples, 4);
    let small_prepared = workspace
        .prepare(
            &small_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &small_policy,
                profile: "allocation-checkpoint",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut small_decision,
            },
        )
        .unwrap();
    assert_eq!(small_prepared.files().unwrap().len(), 1);
    let mut small_hook = InterruptPublicationAt(checkpoint);
    let (small_result, small_count, small_bytes) =
        allocation_count_during(|| publish_prepared_with_hook(small_prepared, &mut small_hook));
    let PreparedPublication::Indeterminate {
        cleanup: mut small_cleanup,
        ..
    } = small_result
    else {
        panic!("checkpoint must classify prepared publication as indeterminate")
    };
    let small_recovery = workspace
        .recover_indeterminate_publication(&mut small_cleanup)
        .unwrap();
    match expected {
        ExpectedCheckpointRecovery::RolledBack => {
            assert!(matches!(small_recovery, PublicationRecovery::RolledBack));
            assert_eq!(fs::read_dir(&small_output).unwrap().count(), 0);
        }
        ExpectedCheckpointRecovery::Complete => {
            assert!(matches!(small_recovery, PublicationRecovery::Complete(_)));
            assert_eq!(fs::read_dir(&small_output).unwrap().count(), 1);
        }
    }
    arena.release_frozen(&mut small_frozen, DEADLINE).unwrap();

    let mut maximum_decision = FrozenExportDecision::new(&plan).unwrap();
    let maximum_samples = vec![0.25; 32 * 4];
    let mut maximum_frozen = freeze(&mut arena, &ingress, &maximum_samples, 4);
    let maximum_prepared = workspace
        .prepare(
            &maximum_frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &maximum_policy,
                profile: "allocation-checkpoint",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut maximum_decision,
            },
        )
        .unwrap();
    assert_eq!(maximum_prepared.files().unwrap().len(), 64);
    let mut maximum_hook = InterruptPublicationAt(checkpoint);
    let (maximum_result, maximum_count, maximum_bytes) =
        allocation_count_during(|| publish_prepared_with_hook(maximum_prepared, &mut maximum_hook));
    let PreparedPublication::Indeterminate {
        cleanup: mut maximum_cleanup,
        ..
    } = maximum_result
    else {
        panic!("checkpoint must classify prepared publication as indeterminate")
    };
    assert_eq!(
        (maximum_count, maximum_bytes),
        (small_count, small_bytes),
        "checkpoint publication allocations scaled from one output to the startup maximum"
    );
    let maximum_recovery = workspace
        .recover_indeterminate_publication(&mut maximum_cleanup)
        .unwrap();
    match expected {
        ExpectedCheckpointRecovery::RolledBack => {
            assert!(matches!(maximum_recovery, PublicationRecovery::RolledBack));
            assert_eq!(fs::read_dir(&maximum_output).unwrap().count(), 0);
        }
        ExpectedCheckpointRecovery::Complete => {
            assert!(matches!(maximum_recovery, PublicationRecovery::Complete(_)));
            assert_eq!(fs::read_dir(&maximum_output).unwrap().count(), 1);
        }
    }
    arena.release_frozen(&mut maximum_frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn recall_partial_created_checkpoint_allocations_do_not_scale_and_recovery_rolls_back() {
    assert_checkpoint_publication_allocations_do_not_scale(
        PublicationCheckpoint::RecallPartialCreatedBeforeManifest { index: 0 },
        ResolvedLayout::FlatDetailed,
        ExpectedCheckpointRecovery::RolledBack,
    );
}

#[test]
fn recall_renamed_checkpoint_allocations_do_not_scale_and_recovery_rolls_back() {
    assert_checkpoint_publication_allocations_do_not_scale(
        PublicationCheckpoint::RecallRenamedBeforeManifest { index: 0 },
        ResolvedLayout::FlatDetailed,
        ExpectedCheckpointRecovery::RolledBack,
    );
}

#[test]
fn dump_after_rename_checkpoint_allocations_do_not_scale_and_recovery_completes() {
    assert_checkpoint_publication_allocations_do_not_scale(
        PublicationCheckpoint::DumpAfterRename,
        ResolvedLayout::TimestampDirectory,
        ExpectedCheckpointRecovery::Complete,
    );
}

#[test]
fn current_artifact_cleanup_race_never_deletes_foreign_replacement_and_retries() {
    let geometry = Geometry {
        retention_frames: 32,
        channels: 4,
        chunk_frames: 32,
        split_when_over_bytes: 50,
        io_buffer_bytes_per_channel: 24,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let state = Arc::new(Mutex::new(CleanupRaceState::default()));
    let mut workspace = PersistenceWorkspace::new_with_cleanup_io(
        &plan,
        workspace_config(geometry),
        Box::new(ReplaceAtCleanupCheckRemoveBoundary {
            state: Arc::clone(&state),
        }),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let output = root.path().join("output");
    let policy = exact_zero_policy(&output, &[ChannelExportMode::Auto; 4]);
    let mut decision = FrozenExportDecision::new(&plan).unwrap();
    let mut samples = vec![0.0; 2 * 4];
    samples[0] = 0.25;
    let mut frozen = freeze(&mut arena, &ingress, &samples, 4);
    let prepared = workspace
        .prepare(
            &frozen,
            PrepareRequest::Policy {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "cleanup-race",
                staging_root: &staging,
                timestamp: TIMESTAMP,
                decision: &mut decision,
            },
        )
        .unwrap();
    let result = publish_prepared_with_hook(
        prepared,
        &mut InterruptPublicationAt(PublicationCheckpoint::RecallPartialCreatedBeforeManifest {
            index: 0,
        }),
    );
    let PreparedPublication::Indeterminate {
        cleanup: mut descriptor,
        ..
    } = result
    else {
        panic!("partial-created checkpoint must be indeterminate")
    };

    assert!(matches!(
        workspace
            .recover_indeterminate_publication(&mut descriptor)
            .unwrap(),
        PublicationRecovery::Pending
    ));
    let foreign = state
        .lock()
        .unwrap()
        .replaced_path
        .clone()
        .expect("cleanup seam replaced the public artifact name");
    assert_eq!(fs::read(&foreign).unwrap(), b"foreign cleanup race inode");

    fs::remove_file(&foreign).unwrap();
    assert!(matches!(
        workspace
            .recover_indeterminate_publication(&mut descriptor)
            .unwrap(),
        PublicationRecovery::RolledBack
    ));
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn prepared_indeterminate_descriptor_has_fixed_metadata_only() {
    assert!(std::mem::size_of::<lamb::persistence_workspace::IndeterminatePublication>() <= 32);
}

struct BlockingIo {
    entered: Arc<(Mutex<bool>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
    blocked: bool,
}

impl WavIo for BlockingIo {
    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        if !self.blocked && bytes.len() != 44 {
            self.blocked = true;
            let (lock, condvar) = &*self.entered;
            *lock.lock().unwrap() = true;
            condvar.notify_one();
            let (lock, condvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = condvar.wait(released).unwrap();
            }
        }
        file.write_all(bytes)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        file.flush()
    }

    fn sync_all(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
}

#[test]
fn active_capture_continues_while_frozen_streaming_is_blocked() {
    let geometry = Geometry {
        retention_frames: 8,
        channels: 1,
        chunk_frames: 2,
        split_when_over_bytes: 1_000,
        io_buffer_bytes_per_channel: 6,
        maximum_path_bytes: 256,
    };
    let (mut arena, ingress, plan) = runtime(geometry);
    let workspace = PersistenceWorkspace::new(&plan, workspace_config(geometry)).unwrap();
    let frozen = freeze(&mut arena, &ingress, &[0.25; 4], 1);
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    let entered = Arc::new((Mutex::new(false), Condvar::new()));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let thread_entered = Arc::clone(&entered);
    let thread_release = Arc::clone(&release);

    let encoder = thread::spawn(move || {
        let mut workspace = workspace;
        let mut io = BlockingIo {
            entered: thread_entered,
            release: thread_release,
            blocked: false,
        };
        let prepared = workspace
            .prepare_with_io(
                &frozen,
                PrepareRequest::Recall {
                    staging_root: &staging,
                    output_dir: &output,
                    timestamp: TIMESTAMP,
                    channel_names: &[],
                },
                &mut io,
            )
            .unwrap();
        drop(prepared);
    });

    let (lock, condvar) = &*entered;
    let entered_guard = lock.lock().unwrap();
    let (entered_guard, timeout) = condvar
        .wait_timeout_while(entered_guard, DEADLINE, |entered| !*entered)
        .unwrap();
    assert!(*entered_guard && !timeout.timed_out());

    let outcome = ingress.try_push_interleaved(&[1.0; 6], 1).unwrap();
    assert_eq!(outcome.enqueued_frames, 6);
    assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 4..10);

    let (lock, condvar) = &*release;
    *lock.lock().unwrap() = true;
    condvar.notify_one();
    encoder.join().unwrap();
    arena.shutdown(DEADLINE).unwrap();
}
