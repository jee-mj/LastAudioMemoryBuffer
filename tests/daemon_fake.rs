use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lamb::activity::ThresholdSource;
use lamb::activity::{ActivityDetectorKind, ChannelExportMode, FrozenExportDecision};
use lamb::calibration::{ConfiguredDeviceSelector, InputBackend, StaleReason};
use lamb::capture_arena::{CaptureArena, CaptureRuntimeConfig};
use lamb::capture_runtime::DEFAULT_MAXIMUM_PATH_BYTES;
use lamb::control::{
    send_request, write_persistence_response, CalibrationEvaluation, CalibrationReportStatus,
    CaptureState, ConfiguredInputReport, ControlRequest, ControlResponse, DaemonLifecycleStatus,
    DaemonState, DaemonStatus, ErrorClass, PersistenceClientResponseSeed,
    PersistenceOutcomeResponse, RetryPolicy, StoredThresholdReport, ThresholdChannelReport,
    ThresholdReport, ThresholdRequest,
};
use lamb::dump::{CommittedPersistenceRef, DumpCoordinator, PolicyPersistenceRequest};
use lamb::export_policy::{
    ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
    ResolvedLayout,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::persistence_workspace::{PersistenceWorkspace, PersistenceWorkspaceConfig};
use lamb::sample_ring::{RingConfig, SampleFormat};
use serde::de::DeserializeSeed;

struct ThreadCountingAllocator;
thread_local! {
    static COUNT_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static PAUSE_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    static ALLOCATION_BYTES: Cell<usize> = const { Cell::new(0) };
}
unsafe impl GlobalAlloc for ThreadCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT_ALLOCATIONS.with(|enabled| {
            if enabled.get() && !PAUSE_ALLOCATIONS.with(Cell::get) {
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
    struct ResetCounting;
    impl Drop for ResetCounting {
        fn drop(&mut self) {
            COUNT_ALLOCATIONS.with(|enabled| enabled.set(false));
            PAUSE_ALLOCATIONS.with(|paused| paused.set(false));
        }
    }
    ALLOCATION_COUNT.with(|count| count.set(0));
    ALLOCATION_BYTES.with(|bytes| bytes.set(0));
    COUNT_ALLOCATIONS.with(|enabled| enabled.set(true));
    let _reset = ResetCounting;
    let result = operation();
    (
        result,
        ALLOCATION_COUNT.with(Cell::get),
        ALLOCATION_BYTES.with(Cell::get),
    )
}

fn pause_counting_after_delivery() {
    PAUSE_ALLOCATIONS.with(|paused| paused.set(true));
}

fn allocation_snapshot() -> (usize, usize) {
    (
        ALLOCATION_COUNT.with(Cell::get),
        ALLOCATION_BYTES.with(Cell::get),
    )
}

#[test]
fn allocation_count_during_restores_flags_after_unwind() {
    let unwind = std::panic::catch_unwind(|| allocation_count_during(|| panic!("test unwind")));
    assert!(unwind.is_err());
    assert!(!COUNT_ALLOCATIONS.with(Cell::get));
    assert!(!PAUSE_ALLOCATIONS.with(Cell::get));
    let (_, count, bytes) = allocation_count_during(|| {});
    assert_eq!((count, bytes), (0, 0));
}

#[test]
fn threshold_requests_round_trip_through_the_nested_protocol() {
    let request = ControlRequest::Threshold {
        request: ThresholdRequest::Calibrate {
            profile: "studio".to_string(),
            channel: "mic".to_string(),
            seconds: 5,
        },
    };

    let encoded = serde_json::to_string(&request).unwrap();
    assert_eq!(
        encoded,
        r#"{"command":"threshold","request":{"operation":"calibrate","profile":"studio","channel":"mic","seconds":5}}"#
    );
    assert_eq!(
        serde_json::from_str::<ControlRequest>(&encoded).unwrap(),
        request
    );
}

#[test]
fn threshold_report_is_optional_for_legacy_response_compatibility() {
    let old: ControlResponse =
        serde_json::from_str(r#"{"ok":true,"message":"status","status":null}"#).unwrap();
    assert_eq!(old.threshold_report, None);

    let report = ThresholdReport {
        profile: "studio".to_string(),
        active_profile: true,
        capturing: false,
        channels: vec![ThresholdChannelReport {
            channel: "mic".to_string(),
            detector: "windowed-rms-peak".to_string(),
            detector_version: "windowed-rms-peak-v1".to_string(),
            configured_input: ConfiguredInputReport {
                backend: InputBackend::PipeWire,
                selector: ConfiguredDeviceSelector::PipeWireAuto,
                source: "source_FL".to_string(),
                input_id: "a".repeat(64),
            },
            stored: Some(StoredThresholdReport {
                threshold_dbfs: -42.0,
                source: ThresholdSource::Manual,
                updated_at_unix_seconds: 7,
                age_seconds: Some(3),
                calibration_id: None,
            }),
            artifact_status: CalibrationReportStatus::NotApplicable,
            current_live_identity: None,
            configured_identity_matches: None,
            calibration_evaluation: CalibrationEvaluation::NotResolved,
            effective_threshold_dbfs: Some(-42.0),
        }],
        message: "stored manual threshold".to_string(),
    };
    let response = ControlResponse {
        ok: true,
        message: "threshold updated".to_string(),
        status: None,
        error_context: lamb::control::ControlErrorContext::default(),
        persistence_outcome: None,
        threshold_report: Some(report.clone()),
    };
    assert_eq!(
        serde_json::from_str::<ControlResponse>(&serde_json::to_string(&response).unwrap())
            .unwrap(),
        response
    );
}

#[test]
fn supervisor_status_wire_golden_includes_legacy_and_additive_fields() {
    let status = DaemonStatus {
        state: "waiting-for-device".to_string(),
        active_export_count: 0,
        pending_recall_count: 0,
        buffer_capacity_seconds: 0.0,
        retained_seconds: 0.0,
        dropped_frames: 0,
        target: Some("studio-input".to_string()),
        resolved_target: None,
        sample_rate: 0,
        channel_count: 0,
        format: "F32LE".to_string(),
        last_error: Some("selected target is missing".to_string()),
        lifecycle: DaemonLifecycleStatus {
            daemon_state: DaemonState::Degraded,
            capture_state: CaptureState::WaitingForDevice,
            error_class: Some(ErrorClass::Transient),
            last_error: Some("selected target is missing".to_string()),
            retry_policy: RetryPolicy::BoundedBackoff,
            retry_attempt: 3,
            next_retry_at: Some(1_700_000_005),
            active_profile: Some("studio".to_string()),
            resolved_target: None,
        },
    };

    assert_eq!(
        serde_json::to_string(&status).unwrap(),
        r#"{"state":"waiting-for-device","active_export_count":0,"pending_recall_count":0,"buffer_capacity_seconds":0.0,"retained_seconds":0.0,"dropped_frames":0,"target":"studio-input","resolved_target":null,"sample_rate":0,"channel_count":0,"format":"F32LE","last_error":"selected target is missing","daemonState":"degraded","captureState":"waiting-for-device","errorClass":"transient","lastError":"selected target is missing","retryPolicy":"bounded-backoff","retryAttempt":3,"nextRetryAt":1700000005,"activeProfile":"studio","resolvedTarget":null}"#
    );
}

#[test]
fn legacy_status_wire_deserializes_with_additive_defaults() {
    let old = r#"{"state":"capturing","active_export_count":0,"pending_recall_count":0,"buffer_capacity_seconds":5.0,"retained_seconds":2.5,"dropped_frames":7,"target":"studio-input","resolved_target":"fake","sample_rate":48000,"channel_count":2,"format":"F32LE","last_error":null}"#;
    let status: DaemonStatus = serde_json::from_str(old).unwrap();

    assert_eq!(status.state, "capturing");
    assert_eq!(status.active_export_count, 0);
    assert_eq!(status.pending_recall_count, 0);
    assert_eq!(status.buffer_capacity_seconds, 5.0);
    assert_eq!(status.retained_seconds, 2.5);
    assert_eq!(status.dropped_frames, 7);
    assert_eq!(status.target.as_deref(), Some("studio-input"));
    assert_eq!(status.resolved_target.as_deref(), Some("fake"));
    assert_eq!(status.sample_rate, 48_000);
    assert_eq!(status.channel_count, 2);
    assert_eq!(status.format, "F32LE");
    assert_eq!(status.last_error, None);
    assert_eq!(status.lifecycle, DaemonLifecycleStatus::default());
}

#[test]
fn threshold_report_round_trips_all_typed_profile_wide_fields() {
    let report = ThresholdReport {
        profile: "studio".to_string(),
        active_profile: false,
        capturing: false,
        channels: vec![ThresholdChannelReport {
            channel: "mic".to_string(),
            detector: "windowed-rms-peak".to_string(),
            detector_version: "windowed-rms-peak-v1".to_string(),
            configured_input: ConfiguredInputReport {
                backend: InputBackend::Jack,
                selector: ConfiguredDeviceSelector::JackSourceClient("system".to_string()),
                source: "system:capture_1".to_string(),
                input_id: "b".repeat(64),
            },
            stored: Some(StoredThresholdReport {
                threshold_dbfs: -48.0,
                source: ThresholdSource::Calibrated,
                updated_at_unix_seconds: 10,
                age_seconds: Some(5),
                calibration_id: Some("cal-1".to_string()),
            }),
            artifact_status: CalibrationReportStatus::Stale {
                reason: StaleReason::MissingLiveIdentity,
            },
            current_live_identity: None,
            configured_identity_matches: None,
            calibration_evaluation: CalibrationEvaluation::NotResolved,
            effective_threshold_dbfs: None,
        }],
        message: "threshold report".to_string(),
    };

    let encoded = serde_json::to_string(&report).unwrap();
    assert!(encoded.contains("configured_input"), "{encoded}");
    assert!(encoded.contains("calibration_evaluation"), "{encoded}");
    assert_eq!(
        serde_json::from_str::<ThresholdReport>(&encoded).unwrap(),
        report
    );
}

#[test]
fn persistence_written_response_round_trips_with_source_frame_metadata() {
    let response = ControlResponse {
        ok: true,
        message: "written".to_string(),
        status: None,
        error_context: lamb::control::ControlErrorContext::default(),
        persistence_outcome: Some(PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 350,
            frames: 250,
            export_start_frame: 100,
            export_frames: 250,
            duration_seconds: 2.5,
            lost_frames: 25,
            retention_lost_frames: 25,
            cleared_frames: 0,
            capture_dropped_frames: 0,
            output_directory: PathBuf::from("/tmp/out/20260818T120000"),
            files: vec![PathBuf::from("/tmp/out/20260818T120000/mic.wav")],
        }),
        threshold_report: None,
    };

    let encoded = serde_json::to_string(&response).unwrap();
    assert!(encoded.contains(r#""kind":"written""#), "{encoded}");
    let decoded: ControlResponse = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, response);
}

#[test]
fn persistence_non_written_responses_round_trip_as_successes() {
    let responses = [
        ControlResponse {
            ok: true,
            message: "skipped silent".to_string(),
            status: None,
            error_context: lamb::control::ControlErrorContext::default(),
            persistence_outcome: Some(PersistenceOutcomeResponse::SkippedSilent {
                start_frame: 350,
                end_frame: 450,
                frames: 100,
                duration_seconds: 1.0,
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            }),
            threshold_report: None,
        },
        ControlResponse {
            ok: true,
            message: "no new audio".to_string(),
            status: None,
            error_context: lamb::control::ControlErrorContext::default(),
            persistence_outcome: Some(PersistenceOutcomeResponse::NoNewAudio {
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            }),
            threshold_report: None,
        },
    ];

    for response in responses {
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: ControlResponse = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded, response);
    }
}

#[test]
fn control_response_without_persistence_outcome_remains_compatible() {
    let response: ControlResponse =
        serde_json::from_str(r#"{"ok":true,"message":"status","status":null}"#).unwrap();

    assert_eq!(response.persistence_outcome, None);
}

struct FixedWriter<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> FixedWriter<N> {
    fn new() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }

    fn written(&self) -> &[u8] {
        &self.bytes[..self.length]
    }
}

impl<const N: usize> Write for FixedWriter<N> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .length
            .checked_add(bytes.len())
            .filter(|end| *end <= N)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WriteZero, "fixed writer is full"))?;
        self.bytes[self.length..end].copy_from_slice(bytes);
        self.length = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn maximum_escaped_path(index: usize) -> String {
    let prefix = format!("/tmp/ordered-{index:02}/quoted\"line\nslash\\");
    let maximum = usize::try_from(DEFAULT_MAXIMUM_PATH_BYTES).unwrap();
    assert!(prefix.len() < maximum);
    let padding = maximum - prefix.len();
    prefix + &"x".repeat(padding)
}

fn streaming_response_fixture(file_count: usize) -> (Vec<u8>, Vec<u8>) {
    let output_directory = maximum_escaped_path(99);
    let files: Vec<_> = (0..file_count).map(maximum_escaped_path).collect();
    let response = ControlResponse {
        ok: true,
        message: "written fixture".to_string(),
        status: None,
        error_context: lamb::control::ControlErrorContext::default(),
        persistence_outcome: Some(PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 350,
            frames: 250,
            export_start_frame: 100,
            export_frames: 250,
            duration_seconds: 2.5,
            lost_frames: 25,
            retention_lost_frames: 10,
            cleared_frames: 5,
            capture_dropped_frames: 10,
            output_directory: PathBuf::from(&output_directory),
            files: files.iter().map(PathBuf::from).collect(),
        }),
        threshold_report: None,
    };
    let mut expected = format!(
        concat!(
            "written 250 frames (2.500 seconds), source frames 100..350\n",
            "warning: 25 frames were lost before persistence ",
            "(retention 10, cleared 5, capture-dropped 10)\n",
            "output directory: {}"
        ),
        output_directory
    )
    .into_bytes();
    for file in files {
        expected.push(b'\n');
        expected.extend_from_slice(file.as_bytes());
    }
    expected.push(b'\n');
    (serde_json::to_vec(&response).unwrap(), expected)
}

fn streaming_client_allocations(file_count: usize) -> ((usize, usize), Vec<u8>) {
    const OUTPUT_CAPACITY: usize = 300_000;
    let (json, expected) = streaming_response_fixture(file_count);
    let mut writer = Box::new(FixedWriter::<OUTPUT_CAPACITY>::new());
    let mut deserializer = serde_json::Deserializer::from_slice(&json);
    let seed = PersistenceClientResponseSeed::new(&mut *writer);

    let (result, count, bytes) = allocation_count_during(|| seed.deserialize(&mut deserializer));
    result.unwrap().unwrap();
    assert_eq!(writer.written(), expected);
    ((count, bytes), expected)
}

#[test]
fn streaming_persistence_client_allocations_do_not_scale_with_file_count() {
    let (one_allocations, one_output) = streaming_client_allocations(1);
    let (maximum_allocations, maximum_output) = streaming_client_allocations(64);

    eprintln!(
        "streaming persistence client: one={one_allocations:?}, maximum={maximum_allocations:?}"
    );
    assert_eq!(one_allocations, maximum_allocations);
    assert!(maximum_output.len() > one_output.len());
    assert_eq!(one_output.iter().filter(|byte| **byte == b'\n').count(), 6);
    assert_eq!(
        maximum_output.iter().filter(|byte| **byte == b'\n').count(),
        132
    );
}

#[derive(Clone, Copy, Debug)]
struct AllocationPhases {
    outputs: usize,
    path_len: usize,
    output_directory_len: usize,
    callback_entry: (usize, usize),
    post_writer: (usize, usize),
}

fn response_allocation_measurement(
    layout: ResolvedLayout,
    frames: u64,
    active_channels: u32,
    root: &Path,
) -> AllocationPhases {
    const CHANNELS: u32 = 64;
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 2,
        channels: CHANNELS,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 2,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 32,
        capture_slot_frames: 2,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: 1024,
        maximum_path_bytes: 256,
        maximum_calibration_seconds: 0,
        headroom: 1.0,
    })
    .unwrap();
    let (arena, ingress) = CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: CHANNELS,
                sample_rate: 48_000,
                format: SampleFormat::F32Le,
                chunk_frames: 2,
                chunk_count: 1,
                max_active_snapshots: 1,
            },
            queue_slots: 32,
            slot_frames: 2,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        },
    )
    .unwrap();
    let mut workspace = PersistenceWorkspace::new(
        &plan,
        PersistenceWorkspaceConfig {
            retention_frames: 2,
            channels: CHANNELS,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 2,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            io_buffer_bytes_per_channel: 1024,
            maximum_path_bytes: 256,
        },
    )
    .unwrap();
    let policy = ResolvedExportPolicy::new(
        root.join("output"),
        layout,
        ResolvedActivityPolicy {
            detector: ActivityDetectorKind::ExactZero,
            channels: (0..CHANNELS)
                .map(|index| ChannelActivityPolicy {
                    name: format!("channel-{index:02}"),
                    mode: if index < active_channels {
                        ChannelExportMode::Auto
                    } else {
                        ChannelExportMode::Never
                    },
                    threshold: None,
                })
                .collect(),
            whole_export_exact_zero_gate: false,
            trim_leading_silence: false,
        },
    )
    .unwrap();
    let coordinator =
        DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
    let staging_root = root.join("staging");
    let mut samples = [0.0; 128];
    for sample in samples
        .iter_mut()
        .take(usize::try_from(frames * u64::from(CHANNELS)).unwrap())
    {
        *sample = 0.25;
    }
    let outputs = Cell::new(0);
    let path_len = Cell::new(0);
    let output_directory_len = Cell::new(0);
    let callback_entry = Cell::new((0, 0));
    let post_writer = Cell::new((0, 0));
    let (_result, count, bytes) = allocation_count_during(|| {
        ingress
            .try_push_interleaved(
                &samples[..usize::try_from(frames * u64::from(CHANNELS)).unwrap()],
                CHANNELS,
            )
            .unwrap();
        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                PolicyPersistenceRequest {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "allocation",
                    staging_root: &staging_root,
                    timestamp: "20260818T120000",
                },
                Duration::from_secs(2),
                Duration::from_secs(2),
                |outcome| {
                    callback_entry.set(allocation_snapshot());
                    if let CommittedPersistenceRef::Written {
                        output,
                        frames,
                        losses,
                        ..
                    } = &outcome
                    {
                        outputs.set(output.files.len());
                        let directory = output.output_directory.to_str().unwrap();
                        output_directory_len.set(directory.len());
                        for file in output.files.iter() {
                            let path = file.final_path().to_str().unwrap();
                            if path_len.get() == 0 {
                                path_len.set(path.len());
                            }
                            assert_eq!(path.len(), path_len.get());
                        }
                        let arena_status = arena.status(Duration::from_secs(2)).unwrap();
                        let message = format!(
                            "written: {frames} frames; warning: {} frames lost before persistence",
                            losses.lost_frames()
                        );
                        let status = DaemonStatus {
                            state: "capturing".to_string(),
                            active_export_count: u32::from(arena_status.frozen_pending),
                            pending_recall_count: 0,
                            buffer_capacity_seconds: arena_status.capacity_frames as f64 / 48_000.0,
                            retained_seconds: arena_status.retained_frames as f64 / 48_000.0,
                            dropped_frames: arena_status.dropped_frames,
                            target: None,
                            resolved_target: None,
                            sample_rate: 48_000,
                            channel_count: CHANNELS,
                            format: "F32LE".to_string(),
                            last_error: None,
                            lifecycle: lamb::control::DaemonLifecycleStatus::default(),
                        };
                        write_persistence_response(
                            &mut io::sink(),
                            true,
                            &message,
                            &status,
                            48_000,
                            outcome,
                        )?;
                    } else {
                        panic!("allocation fixture must write");
                    }
                    post_writer.set(allocation_snapshot());
                    pause_counting_after_delivery();
                    Ok(())
                },
            )
            .unwrap()
    });
    assert_eq!((count, bytes), post_writer.get());
    coordinator
        .persist_policy_with_delivery(
            &arena,
            &mut workspace,
            PolicyPersistenceRequest {
                command: ExportCommand::Recall,
                policy: &policy,
                profile: "allocation",
                staging_root: &staging_root,
                timestamp: "20260818T120001",
            },
            Duration::from_secs(2),
            Duration::from_secs(2),
            |outcome| {
                assert!(matches!(
                    outcome,
                    CommittedPersistenceRef::NoNewAudio { .. }
                ));
                Ok(())
            },
        )
        .unwrap();
    AllocationPhases {
        outputs: outputs.get(),
        path_len: path_len.get(),
        output_directory_len: output_directory_len.get(),
        callback_entry: callback_entry.get(),
        post_writer: post_writer.get(),
    }
}

#[test]
fn prepared_response_allocations() {
    let mut comparisons = Vec::new();
    for (layout, label) in [
        (ResolvedLayout::FlatDetailed, "file-set"),
        (ResolvedLayout::TimestampDirectory, "atomic-directory"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let small_root = temp.path().join("small-root");
        let maximum_root = temp.path().join("large-root");
        fs::create_dir_all(&small_root).unwrap();
        fs::create_dir_all(&maximum_root).unwrap();
        let small = response_allocation_measurement(layout.clone(), 2, 1, &small_root);
        let maximum = response_allocation_measurement(layout, 2, 64, &maximum_root);
        assert_eq!(small.outputs, 1, "{label} small output count");
        assert_eq!(maximum.outputs, 64, "{label} maximum output count");
        assert_eq!(
            small.path_len, maximum.path_len,
            "{label} canonical path lengths differ"
        );
        assert_eq!(
            small.output_directory_len, maximum.output_directory_len,
            "{label} output-directory lengths differ"
        );
        eprintln!("{label}: small={small:?}, maximum={maximum:?}");
        comparisons.push((label, small, maximum));
    }
    for (label, small, maximum) in comparisons {
        assert_eq!(
            small.callback_entry, maximum.callback_entry,
            "{label} prepare/publish/commit allocations scaled: {small:?} -> {maximum:?}"
        );
        assert_eq!(
            small.post_writer, maximum.post_writer,
            "{label} response allocations scaled: {small:?} -> {maximum:?}"
        );
    }
}

#[test]
fn recall_then_dump_share_one_capture_session_cursor() {
    assert_persistence_commands_share_cursor(ControlRequest::Recall, ControlRequest::Dump);
}

#[test]
fn dump_then_recall_share_one_capture_session_cursor() {
    assert_persistence_commands_share_cursor(ControlRequest::Dump, ControlRequest::Recall);
}

fn assert_persistence_commands_share_cursor(first: ControlRequest, second: ControlRequest) {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let mut daemon = spawn_fake_daemon(temp.path(), &socket, &out);
    wait_for_socket(&mut daemon, &socket);
    wait_for_retained_audio(&mut daemon, &socket);

    let first_response = send_request(&socket, &first).unwrap();
    let second_response = send_request(&socket, &second).unwrap();
    let stop_response = send_request(&socket, &ControlRequest::Stop).unwrap();
    assert!(stop_response.ok, "stop failed: {}", stop_response.message);
    let _ = daemon.wait();

    assert!(
        first_response.ok,
        "first command failed: {first_response:?}"
    );
    assert!(
        second_response.ok,
        "second command failed: {second_response:?}"
    );
    let first_end = match first_response.persistence_outcome {
        Some(PersistenceOutcomeResponse::Written {
            start_frame,
            end_frame,
            frames,
            export_start_frame,
            export_frames,
            files,
            ..
        }) => {
            assert_eq!(end_frame - start_frame, frames);
            assert!(export_start_frame >= start_frame);
            assert_eq!(export_start_frame + export_frames, end_frame);
            assert!(!files.is_empty());
            assert!(files
                .iter()
                .all(|path| path.starts_with(&out) && path.is_file()));
            if matches!(first, ControlRequest::Recall) {
                assert!(
                    files
                        .iter()
                        .all(|path| path.parent() == Some(out.as_path())),
                    "CommandDefault recall files must be direct children of outputDir: {files:?}"
                );
            }
            end_frame
        }
        outcome => panic!("first command should write captured audio, got {outcome:?}"),
    };
    match second_response.persistence_outcome {
        Some(PersistenceOutcomeResponse::NoNewAudio { .. }) => {}
        Some(PersistenceOutcomeResponse::Written {
            start_frame,
            retention_lost_frames,
            ..
        }) => {
            assert!(
                start_frame >= first_end,
                "persistence ranges must never overlap: {start_frame} < {first_end}"
            );
            let gap = start_frame - first_end;
            if gap > 0 {
                assert!(
                    retention_lost_frames >= gap,
                    "a {gap}-frame gap must be reported as retention loss, got {retention_lost_frames}"
                );
            }
        }
        outcome => panic!("second command should report new or no audio, got {outcome:?}"),
    }
}

fn spawn_fake_daemon(root: &Path, socket: &Path, out: &Path) -> Child {
    let config = root.join("shared-cursor.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 5
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 1

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_lamb"))
        .arg("daemon")
        .arg("--config")
        .arg(config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_socket(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");
}

fn wait_for_retained_audio(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_observation = "no status response received".to_string();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "daemon exited before reporting retained audio: {status}; last observation: {last_observation}"
            );
        }
        match send_request(socket, &ControlRequest::Status) {
            Ok(response) if !response.ok => {
                last_observation = format!("status command failed: {}", response.message);
            }
            Ok(response) => match response.status {
                Some(status) if status.retained_seconds > 0.0 => return,
                Some(status) => {
                    last_observation = format!(
                        "state={}, retained_seconds={}",
                        status.state, status.retained_seconds
                    );
                }
                None => last_observation = "status response omitted daemon status".to_string(),
            },
            Err(error) => last_observation = format!("status request failed: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "daemon did not report retained audio before the 5-second deadline; last observation: {last_observation}"
    );
}

#[test]
fn fake_daemon_status_recall_clear_stop() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(status.status.success());
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("capturing"), "{body}");

    let recall = Command::new(exe)
        .arg("recall")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(recall.status.success());
    assert!(
        String::from_utf8_lossy(&recall.stdout).contains("written"),
        "recall should render its persistence outcome"
    );

    let clear = Command::new(exe)
        .arg("clear")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(clear.status.success());

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();

    let exported: Vec<_> = fs::read_dir(&out).unwrap().collect();
    assert!(!exported.is_empty(), "recall did not export files");
}

#[test]
fn fake_daemon_runtime_validation_does_not_require_pipewire_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();
}

#[test]
fn daemon_expands_percent_t_control_socket_under_runtime_dir() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "%t/lamb/control.sock"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating expanded socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        socket.exists(),
        "daemon did not create expanded control socket"
    );

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "<USERNAME>".to_string())
}

#[test]
fn dump_request_round_trips() {
    let request = lamb::control::ControlRequest::Dump;
    let encoded = serde_json::to_string(&request).unwrap();
    assert_eq!(encoded, r#"{"command":"dump"}"#);
    let decoded: lamb::control::ControlRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, lamb::control::ControlRequest::Dump);
}

#[test]
fn fake_daemon_dump_exports_files_with_iso8601_timestamp_and_channel_names() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    thread::sleep(Duration::from_millis(500));

    let dump = Command::new(exe)
        .arg("dump")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        dump.status.success(),
        "dump failed: stderr={}",
        String::from_utf8_lossy(&dump.stderr)
    );

    let stdout = String::from_utf8(dump.stdout).unwrap();
    assert!(
        stdout.contains("written"),
        "dump output unexpected: {stdout}"
    );

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();

    let exported: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        exported.len(),
        1,
        "dump should publish one directory, got {exported:?}"
    );
    assert!(exported[0].is_dir(), "dump output should be a directory");
    let timestamp = exported[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(timestamp.len(), 14, "timestamp should be ISO-8601 compact");
    assert!(timestamp.chars().all(|c| c.is_ascii_digit()));
    let names: Vec<String> = fs::read_dir(&exported[0])
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let joined = names.join(" ");
    assert!(
        joined.contains(".wav"),
        "dump should export WAV files, got: {joined}"
    );
    assert!(names.contains(&"mic.wav".to_string()), "got: {joined}");
    assert!(names.contains(&"gtr.wav".to_string()), "got: {joined}");
}

#[test]
fn tight_memory_max_is_inspectable_without_capture_allocation() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 5
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 1

[memory]
headroom = 1.25
max = 100

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_socket(&mut child, &socket);

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert_eq!(status["errorClass"], "permanent");
    assert_eq!(status["retryPolicy"], "manual");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert_eq!(status["sample_rate"], 0);
    assert_eq!(status["channel_count"], 0);
    assert!(status["lastError"].as_str().unwrap().contains("memory"));
    assert!(child.try_wait().unwrap().is_none());

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    assert!(child.wait().unwrap().success());
    assert!(!socket.exists());
}
