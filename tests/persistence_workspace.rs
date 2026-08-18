use lamb::capture_arena::{CaptureArena, CaptureIngress, CaptureRuntimeConfig, FrozenCaptureEpoch};
use lamb::error::LambError;
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::persistence_workspace::{
    CleanupIo, PendingCleanup, PersistenceWorkspace, PersistenceWorkspaceConfig, PrepareRequest,
    PreparedPersistence, WavIo,
};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

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
}

impl WavIo for CountingIo {
    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        self.writes += 1;
        file.write_all(bytes)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        file.flush()
    }

    fn sync_all(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
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

    let (_, count, bytes) = allocation_count_during(|| {
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
        "bounded OS/path allocation bytes changed: {maximum:?}"
    );
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
