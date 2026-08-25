use lamb::capture_arena::{
    CalibrationCaptureRequest, CaptureArena, CaptureIngress, CaptureRuntimeConfig,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(2);

fn runtime() -> (CaptureArena, CaptureIngress) {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 100,
        channels: 2,
        sample_rate: 1_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 10,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 16,
        capture_slot_frames: 10,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: 4 * 1024,
        maximum_path_bytes: 512,
        maximum_calibration_seconds: 1,
        headroom: 1.0,
    })
    .unwrap();
    let (arena, ingress) = CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: 2,
                sample_rate: 1_000,
                format: SampleFormat::F32Le,
                chunk_frames: 10,
                chunk_count: 10,
                max_active_snapshots: 1,
            },
            queue_slots: 16,
            slot_frames: 10,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        },
    )
    .unwrap();
    (arena, ingress)
}

#[test]
fn calibration_request_validation_is_bounded_by_the_startup_plan() {
    let (mut arena, _ingress) = runtime();
    for request in [
        CalibrationCaptureRequest {
            channel: 2,
            frames: 1,
        },
        CalibrationCaptureRequest {
            channel: 1,
            frames: 0,
        },
        CalibrationCaptureRequest {
            channel: 1,
            frames: 1_001,
        },
    ] {
        assert!(arena.calibrate_channel(request, DEADLINE).is_err());
    }
    arena.shutdown(DEADLINE).unwrap();
}
