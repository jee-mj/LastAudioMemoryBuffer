use lamb::capture_arena::{CaptureArena, CaptureIngress, CaptureRuntimeConfig, FrozenCaptureEpoch};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::ops::Range;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(2);

fn runtime(
    retention_frames: u64,
    queue_slots: u32,
    slot_frames: u32,
) -> (CaptureArena, CaptureIngress) {
    let channels = 1;
    let chunk_frames = 4;
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames,
        channels,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: queue_slots,
        capture_slot_frames: slot_frames,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: 4 * 1024,
        maximum_path_bytes: 512,
        headroom: 1.0,
    })
    .unwrap();
    CaptureArena::new(
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
    .unwrap()
}

fn mono(range: Range<u64>) -> Vec<f32> {
    range.map(|frame| frame as f32).collect()
}

fn collect_frozen(frozen: &FrozenCaptureEpoch, scratch_frames: usize) -> Vec<f32> {
    let mut scratch = vec![0.0; scratch_frames * frozen.channels() as usize];
    let mut output = Vec::new();
    let mut cursor = frozen.absolute_range().start;
    while cursor < frozen.absolute_range().end {
        let copied = frozen
            .copy_interleaved_range_into(cursor..frozen.absolute_range().end, &mut scratch)
            .unwrap();
        assert!(copied > 0);
        output.extend_from_slice(&scratch[..copied as usize]);
        cursor += copied;
    }
    output
}

#[test]
fn ingress_splits_callbacks_and_freeze_preserves_queue_order() {
    let (mut arena, ingress) = runtime(16, 8, 3);
    let pushed = ingress.try_push_interleaved(&mono(0..8), 1).unwrap();
    assert_eq!(pushed.enqueued_frames, 8);
    assert_eq!(pushed.dropped_frames, 0);

    let frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();

    assert_eq!(frozen.absolute_range(), 0..8);
    assert_eq!(collect_frozen(&frozen, 2), mono(0..8));
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn producer_continues_while_frozen_epoch_is_held() {
    let (mut arena, ingress) = runtime(8, 16, 4);
    ingress.try_push_interleaved(&mono(0..5), 1).unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();

    ingress.try_push_interleaved(&mono(5..17), 1).unwrap();
    assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 9..17);
    assert_eq!(collect_frozen(&frozen, 2), mono(0..5));

    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn repeated_worker_switches_preserve_contiguous_ranges() {
    let (mut arena, ingress) = runtime(16, 8, 4);
    let mut start = 0;
    for width in [3_u64, 5, 2, 4] {
        let end = start + width;
        ingress.try_push_interleaved(&mono(start..end), 1).unwrap();
        let mut frozen = arena.freeze_since(Some(start), DEADLINE).unwrap().unwrap();
        assert_eq!(frozen.absolute_range(), start..end);
        assert_eq!(collect_frozen(&frozen, 3), mono(start..end));
        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
        start = end;
    }
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn pending_frozen_rejects_second_freeze_without_stopping_capture() {
    let (mut arena, ingress) = runtime(16, 8, 4);
    ingress.try_push_interleaved(&mono(0..4), 1).unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();

    ingress.try_push_interleaved(&mono(4..8), 1).unwrap();
    assert!(arena.freeze_since(Some(4), DEADLINE).is_err());
    assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 4..8);

    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn clear_and_release_are_ordered_worker_commands() {
    let (mut arena, ingress) = runtime(16, 8, 4);
    ingress.try_push_interleaved(&mono(0..5), 1).unwrap();
    arena.clear_active(DEADLINE).unwrap();
    assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 5..5);

    ingress.try_push_interleaved(&mono(5..7), 1).unwrap();
    let mut frozen = arena.freeze_since(Some(5), DEADLINE).unwrap().unwrap();
    assert_eq!(collect_frozen(&frozen, 2), mono(5..7));
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    assert!(frozen
        .copy_interleaved_range_into(5..6, &mut [0.0; 1])
        .is_err());
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn worker_status_accounts_enqueued_and_dropped_frames_once() {
    let (mut arena, ingress) = runtime(8, 8, 4);
    let pushed = ingress.try_push_interleaved(&mono(0..6), 1).unwrap();
    let status = arena.status(DEADLINE).unwrap();

    assert_eq!(pushed.enqueued_frames + pushed.dropped_frames, 6);
    assert_eq!(status.ingress_enqueued_frames, pushed.enqueued_frames);
    assert_eq!(status.capture_dropped_frames, pushed.dropped_frames);
    assert_eq!(
        status.worker_written_frames + status.worker_dropped_frames,
        pushed.enqueued_frames
    );
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn shutdown_stops_worker_and_closes_ingress() {
    let (mut arena, ingress) = runtime(8, 4, 4);
    ingress.try_push_interleaved(&mono(0..2), 1).unwrap();

    arena.shutdown(DEADLINE).unwrap();

    assert!(ingress.try_push_interleaved(&mono(2..3), 1).is_err());
}

#[test]
fn wrong_arena_release_keeps_capability_retryable() {
    let (mut first, ingress) = runtime(8, 8, 4);
    let (mut other, _other_ingress) = runtime(8, 8, 4);
    ingress.try_push_interleaved(&mono(0..3), 1).unwrap();
    let mut frozen = first.freeze_since(None, DEADLINE).unwrap().unwrap();

    assert!(other.release_frozen(&mut frozen, DEADLINE).is_err());
    assert_eq!(collect_frozen(&frozen, 2), mono(0..3));
    first.release_frozen(&mut frozen, DEADLINE).unwrap();

    first.shutdown(DEADLINE).unwrap();
    other.shutdown(DEADLINE).unwrap();
}
