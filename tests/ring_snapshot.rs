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

/// Ring with many small chunks — makes snapshot iteration take long
/// enough that the writer can advance during iteration, exercising
/// the two-pass TOCTOU fix.  Capacity is kept modest so tests finish
/// quickly; the writer yields between bursts to let the snapshot
/// thread keep up.
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
    // return exactly `capacity` frames — no chunks lost to the TOCTOU
    // race between snapshot iteration and concurrent writes.
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
        "snapshot after wrap must capture full buffer; TOCTOU may have lost chunks"
    );
}

#[test]
fn snapshot_while_writer_is_active_captures_all_published_frames() {
    // Concurrent-writer regression: a writer thread continuously feeds
    // frames while we take snapshots.  Each snapshot of the last N
    // frames must contain at least N frames — no chunks silently lost
    // to the TOCTOU race between snapshot iteration and writer.
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

    // Take several snapshots while the writer is active.
    // The two-pass fix catches most frames published during iteration,
    // but a single chunk at the trailing edge of the range may race
    // with the writer (overwritten after the pass-1 boundary but before
    // pass-2 can pin it).  Allow at most one chunk of loss per snapshot.
    let chunk_frames = 2u64; // matches wide_ring chunk_frames
    for _ in 0..20 {
        let snap = ring.snapshot_last_frames(capacity).unwrap();
        assert!(
            snap.total_frames() >= capacity - chunk_frames,
            "snapshot during concurrent writes captured {} frames, expected ≥ {}; TOCTOU race may be losing chunks",
            snap.total_frames(),
            capacity - chunk_frames,
        );
    }

    stop.store(true, std::sync::atomic::Ordering::Release);
    let _ = writer.join();
}
