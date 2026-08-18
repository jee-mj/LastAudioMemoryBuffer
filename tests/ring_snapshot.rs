use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::sync::Arc;
use std::thread;

fn ring() -> SampleRing {
    SampleRing::new(RingConfig {
        channels: 2,
        sample_rate: 10,
        format: SampleFormat::F32Le,
        chunk_frames: 4,
        chunk_count: 3,
        max_active_snapshots: 1,
    })
    .unwrap()
}

/// Ring with many small chunks to exercise wrap ordering and concurrent
/// snapshot pinning. Capacity stays modest so tests finish quickly.
fn wide_ring() -> SampleRing {
    SampleRing::new(RingConfig {
        channels: 1,
        sample_rate: 100,
        format: SampleFormat::F32Le,
        chunk_frames: 2,
        chunk_count: 128,
        max_active_snapshots: 4,
    })
    .unwrap()
}

fn interleaved_frames(frames: u32, channels: u32, base: f32) -> Vec<f32> {
    (0..frames * channels).map(|i| base + i as f32).collect()
}

#[test]
fn snapshot_range_returns_exact_absolute_frames() {
    let ring = ring();
    let frames: Vec<f32> = (0..8)
        .flat_map(|frame| [frame as f32, 100.0 + frame as f32])
        .collect();
    ring.write_interleaved(&frames, 2).unwrap();

    let snapshot = ring.snapshot_range(2..5).unwrap();

    assert_eq!((snapshot.start_frame(), snapshot.end_frame()), (2, 5));
    assert_eq!(
        snapshot.read_channel_samples(0).unwrap(),
        vec![2.0, 3.0, 4.0]
    );
    assert_eq!(
        snapshot.read_channel_samples(1).unwrap(),
        vec![102.0, 103.0, 104.0]
    );
}

#[test]
fn snapshot_range_tracks_and_enforces_wrapped_boundaries() {
    let ring = wide_ring();
    let capacity = ring.status().capacity_frames;
    let head = capacity + 32;
    ring.write_interleaved(&interleaved_frames(head as u32, 1, 0.0), 1)
        .unwrap();

    assert_eq!(ring.write_head_frame(), head);
    assert_eq!(ring.oldest_frame(), 32);

    let snapshot = ring.snapshot_range(ring.oldest_frame()..head).unwrap();
    assert_eq!((snapshot.start_frame(), snapshot.end_frame()), (32, head));
    assert_eq!(snapshot.total_frames(), capacity);
    assert_eq!(
        snapshot.read_channel_samples(0).unwrap(),
        (32..head).map(|frame| frame as f32).collect::<Vec<_>>()
    );
    drop(snapshot);

    assert!(ring.snapshot_range(31..head).is_err());
}

#[test]
fn oldest_frame_starts_at_the_oldest_contiguous_published_chunk() {
    let ring = ring();
    let frames: Vec<f32> = (0..13)
        .flat_map(|frame| [frame as f32, 100.0 + frame as f32])
        .collect();
    ring.write_interleaved(&frames, 2).unwrap();

    assert_eq!(ring.write_head_frame(), 13);
    assert_eq!(ring.oldest_frame(), 4);
    assert_eq!(ring.status().retained_frames, 9);

    let snapshot = ring.snapshot_range(4..13).unwrap();
    assert_eq!((snapshot.start_frame(), snapshot.end_frame()), (4, 13));
    assert_eq!(
        snapshot.read_channel_samples(0).unwrap(),
        (4..13).map(|frame| frame as f32).collect::<Vec<_>>()
    );
    assert!(ring.snapshot_range(1..13).is_err());
}

#[test]
fn clear_clamps_the_oldest_contiguous_published_chunk_boundary() {
    let ring = ring();
    ring.write_interleaved(&interleaved_frames(6, 2, 0.0), 2)
        .unwrap();
    ring.clear().unwrap();
    ring.write_interleaved(&interleaved_frames(7, 2, 12.0), 2)
        .unwrap();

    assert_eq!(ring.write_head_frame(), 13);
    assert_eq!(ring.oldest_frame(), 6);

    let snapshot = ring.snapshot_range(6..13).unwrap();
    assert_eq!((snapshot.start_frame(), snapshot.end_frame()), (6, 13));
    assert_eq!(snapshot.total_frames(), 7);
}

#[test]
fn atomic_selection_clamps_and_pins_a_partially_recycled_range() {
    let ring = ring();
    let frames: Vec<f32> = (0..13)
        .flat_map(|frame| [frame as f32, 100.0 + frame as f32])
        .collect();
    ring.write_interleaved(&frames, 2).unwrap();

    let selected = ring.select_snapshot(Some(1)).unwrap();
    assert_eq!(selected.requested_start(), 1);
    assert_eq!(selected.oldest_frame(), 4);
    assert_eq!(
        (
            selected.snapshot().start_frame(),
            selected.snapshot().end_frame()
        ),
        (4, 13)
    );

    ring.write_interleaved(&[13.0, 113.0], 2).unwrap();
    assert_eq!(ring.write_head_frame(), 13);
    assert_eq!(ring.status().dropped_frames, 1);
    assert_eq!(
        selected.snapshot().read_channel_samples(0).unwrap(),
        (4..13).map(|frame| frame as f32).collect::<Vec<_>>()
    );
    drop(selected);

    ring.write_interleaved(&[13.0, 113.0], 2).unwrap();
    let next = ring.select_snapshot(Some(13)).unwrap();
    assert_eq!(
        (next.snapshot().start_frame(), next.snapshot().end_frame()),
        (13, 14)
    );
    assert_eq!(next.snapshot().read_channel_samples(0).unwrap(), vec![13.0]);
}

#[test]
fn snapshot_range_rejects_incomplete_coverage() {
    let ring = ring();
    ring.write_interleaved(&interleaved_frames(8, 2, 0.0), 2)
        .unwrap();

    assert!(ring.snapshot_range(2..9).is_err());
    assert_eq!(ring.status().active_snapshots, 0);

    ring.write_interleaved(&interleaved_frames(8, 2, 100.0), 2)
        .unwrap();
    assert_eq!(ring.status().dropped_frames, 0);

    let snapshot = ring
        .snapshot_range(ring.oldest_frame()..ring.write_head_frame())
        .unwrap();
    assert_eq!(snapshot.total_frames(), ring.status().capacity_frames);
}

#[test]
fn snapshot_range_rejects_reversed_ranges() {
    let ring = ring();
    ring.write_interleaved(&interleaved_frames(8, 2, 0.0), 2)
        .unwrap();

    let start = 5;
    let end = 2;
    assert!(ring.snapshot_range(start..end).is_err());
}

#[test]
fn oldest_frame_remains_stable_while_writer_publishes() {
    let ring = Arc::new(
        SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 100,
            format: SampleFormat::F32Le,
            chunk_frames: 200_000,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap(),
    );
    ring.write_interleaved(&[0.0], 1).unwrap();

    let start = Arc::new(std::sync::Barrier::new(2));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ring_w = Arc::clone(&ring);
    let start_w = Arc::clone(&start);
    let done_w = Arc::clone(&done);
    let writer = thread::spawn(move || {
        start_w.wait();
        for frame in 1..100_001 {
            ring_w.write_interleaved(&[frame as f32], 1).unwrap();
        }
        done_w.store(true, std::sync::atomic::Ordering::Release);
    });

    start.wait();
    let mut transient_oldest = None;
    while !done.load(std::sync::atomic::Ordering::Acquire) {
        let oldest = ring.oldest_frame();
        if oldest != 0 {
            transient_oldest = Some(oldest);
            break;
        }
    }
    writer.join().unwrap();

    assert_eq!(
        transient_oldest, None,
        "oldest frame must remain at the retention boundary while unpublished appends do not advance it"
    );
}

#[test]
fn write_splits_buffer_across_chunks_and_snapshots_chronologically() {
    let ring = ring();
    let frames: Vec<f32> = (0..20).map(|v| v as f32).collect();
    ring.write_interleaved(&frames, 2).unwrap();
    let snapshot = ring.snapshot_last_frames(6).unwrap();
    assert_eq!(snapshot.total_frames(), 6);
    assert_eq!(snapshot.channels(), 2);
    assert!(snapshot.segments().len() >= 2);
}

#[test]
fn pinned_chunks_are_not_overwritten_and_overrun_is_counted() {
    let ring = ring();
    let frames: Vec<f32> = (0..24).map(|v| v as f32).collect();
    ring.write_interleaved(&frames, 2).unwrap();
    let snapshot = ring.snapshot_last_frames(12).unwrap();
    let more: Vec<f32> = (100..140).map(|v| v as f32).collect();
    ring.write_interleaved(&more, 2).unwrap();
    assert!(ring.status().dropped_frames > 0);
    drop(snapshot);
}

#[test]
fn clear_clamps_future_snapshots_but_keeps_existing_snapshot_valid() {
    let ring = ring();
    let frames: Vec<f32> = (0..16).map(|v| v as f32).collect();
    ring.write_interleaved(&frames, 2).unwrap();
    let old_snapshot = ring.snapshot_last_frames(4).unwrap();
    ring.clear().unwrap();
    let new_snapshot = ring.snapshot_last_frames(12).unwrap();
    assert_eq!(new_snapshot.total_frames(), 0);
    assert_eq!(old_snapshot.total_frames(), 4);
}

#[test]
fn snapshot_captures_full_buffer_after_wrap() {
    // Fill the ring exactly, then write enough extra frames to wrap at
    // least once.  The snapshot of the last `capacity` frames must
    // return exactly `capacity` frames.
    let ring = wide_ring();
    let capacity = ring.status().capacity_frames;

    // Fill buffer completely with frames 0..capacity-1
    let fill = interleaved_frames(capacity as u32, 1, 0.0);
    ring.write_interleaved(&fill, 1).unwrap();
    assert_eq!(ring.status().retained_frames, capacity);

    // Write more frames to trigger wrap-around (these overwrite the oldest chunks)
    let extra = interleaved_frames(32, 1, capacity as f32);
    ring.write_interleaved(&extra, 1).unwrap();

    // Snapshot the last `capacity` frames — should get exactly capacity
    let snap = ring.snapshot_last_frames(capacity).unwrap();
    assert_eq!(
        snap.total_frames(),
        capacity,
        "snapshot after wrap must capture the exact retained range"
    );
}

#[test]
fn snapshot_while_writer_is_active_captures_all_published_frames() {
    // Concurrent-writer regression: a writer thread continuously feeds
    // frames while we take snapshots. Each snapshot must cover the exact
    // retained range captured while segment publication is stable.
    let ring = Arc::new(wide_ring());
    let capacity = ring.status().capacity_frames;

    // Pre-fill so the writer will be wrapping
    let fill = interleaved_frames(capacity as u32, 1, 0.0);
    ring.write_interleaved(&fill, 1).unwrap();

    let ring_w = Arc::clone(&ring);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_w = Arc::clone(&stop);

    let writer = thread::spawn(move || {
        let mut seq = capacity as u32;
        while !stop_w.load(std::sync::atomic::Ordering::Acquire) {
            // Write one chunk-worth at a time and yield so the
            // snapshot thread has a chance to pin chunks before the
            // writer wraps around and reuses them.
            let data = interleaved_frames(2, 1, seq as f32);
            let _ = ring_w.write_interleaved(&data, 1);
            seq += 2;
            thread::yield_now();
        }
    });

    // Take several exact snapshots while the writer is active.
    for _ in 0..20 {
        let snap = ring.snapshot_last_frames(capacity).unwrap();
        assert_eq!(
            snap.total_frames(),
            capacity,
            "snapshot during concurrent writes must capture the exact retained range",
        );
    }

    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = writer.join();
}

#[test]
fn fixed_buffer_copy_caps_at_capacity_and_preserves_interleaving() {
    let ring = ring();
    let samples: Vec<f32> = (0..6)
        .flat_map(|frame| [frame as f32, 100.0 + frame as f32])
        .collect();
    ring.write_interleaved(&samples, 2).unwrap();
    let mut destination = [0.0; 6];

    let copied = ring
        .copy_interleaved_range_into(1..6, &mut destination)
        .unwrap();

    assert_eq!(copied, 3);
    assert_eq!(destination, [1.0, 101.0, 2.0, 102.0, 3.0, 103.0]);
}

#[test]
fn reset_rejects_active_snapshots_and_clears_all_ring_state_after_release() {
    let ring = ring();
    ring.write_interleaved(&interleaved_frames(6, 2, 0.0), 2)
        .unwrap();
    ring.record_dropped_frames(7).unwrap();
    let snapshot = ring.snapshot_last_frames(2).unwrap();

    assert!(ring.reset().is_err());
    drop(snapshot);
    ring.reset().unwrap();

    assert_eq!(ring.write_head_frame(), 0);
    assert_eq!(ring.oldest_frame(), 0);
    assert_eq!(ring.status().retained_frames, 0);
    assert_eq!(ring.status().dropped_frames, 0);
    assert_eq!(ring.status().active_snapshots, 0);

    ring.write_interleaved(&[8.0, 108.0], 2).unwrap();
    assert_eq!(ring.snapshot_last_frames(1).unwrap().total_frames(), 1);
}
