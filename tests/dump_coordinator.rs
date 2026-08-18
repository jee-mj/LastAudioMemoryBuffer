use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use lamb::dump::{DumpCoordinator, DumpOutcome, FrameRange, PublishedOutput, SampleSnapshot};
use lamb::error::LambError;
use lamb::export_wav::{publish_dump, DumpPublishRequest};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};

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
        .chunks_exact(3)
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
            lost_frames: 0,
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
            lost_frames: 0,
            output_directory: PathBuf::from("/tmp/b"),
            files: vec![PathBuf::from("/tmp/b/audio.wav")],
        }
    );

    let no_new = coordinator
        .dump(&ring, |_| {
            panic!("publisher must not run without new audio")
        })
        .unwrap();
    assert_eq!(no_new, DumpOutcome::NoNewAudio);
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
            lost_frames: 0,
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
            lost_frames: 0,
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
            lost_frames: 0,
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
            lost_frames: 0,
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
            lost_frames: 0,
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
    assert_eq!(second_outcome, DumpOutcome::NoNewAudio);
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
            lost_frames: 2,
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
            lost_frames: 3,
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
            DumpOutcome::Written {
                range, lost_frames, ..
            } => {
                assert_eq!(range.start, handled_until + lost_frames);
                handled_until = range.end;
            }
            DumpOutcome::NoNewAudio if done.load(Ordering::Acquire) => break,
            DumpOutcome::NoNewAudio => thread::yield_now(),
            DumpOutcome::SkippedSilent { .. } => panic!("writer only captures nonzero samples"),
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
            lost_frames: 0,
            ..
        }
    ));
    assert_eq!(
        wav_s24_samples(&output.path().join(timestamp_b).join("mic.wav")),
        vec![4_194_304, -4_194_304]
    );
}
