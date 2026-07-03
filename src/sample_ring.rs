use crate::error::{LambError, Result};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Writable = 0,
    Writing = 1,
    Published = 2,
    Stale = 3,
}

impl ChunkState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Writing,
            2 => Self::Published,
            3 => Self::Stale,
            _ => Self::Writable,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RingConfig {
    pub channels: u32,
    pub sample_rate: u32,
    pub format: SampleFormat,
    pub chunk_frames: u32,
    pub chunk_count: u32,
    pub max_active_snapshots: u32,
}

#[derive(Debug, Clone)]
pub struct RingStatus {
    pub dropped_frames: u64,
    pub retained_frames: u64,
    pub capacity_frames: u64,
    pub active_snapshots: u32,
    pub last_overrun: Option<SystemTime>,
}

#[derive(Debug)]
struct Chunk {
    sequence: AtomicU64,
    state: AtomicU8,
    pin_count: AtomicU32,
    valid_start_frame: AtomicU64,
    valid_frame_count: AtomicU32,
    data: Mutex<Vec<f32>>,
}

#[derive(Debug, Clone)]
pub struct SnapshotSegment {
    chunk: Arc<Chunk>,
    expected_sequence: u64,
    start_frame_in_chunk: u32,
    frame_count: u32,
}

#[derive(Debug)]
pub struct Snapshot {
    segments: Vec<SnapshotSegment>,
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
    total_frames: u64,
    active_counter: Option<Arc<AtomicU32>>,
}

/// Guard that holds the active-snapshot slot during snapshot construction.
///
/// Increments the counter on creation (failing if at capacity) and
/// decrements it on drop UNLESS explicitly consumed via [`consume`].
/// Consuming transfers ownership of the slot to the returned `Snapshot`
/// (which decrements on its own `Drop`), making the guard panic-safe:
/// an early return or unwind will still release the slot.
struct ActiveSnapshotGuard {
    counter: Arc<AtomicU32>,
    consumed: bool,
}

impl ActiveSnapshotGuard {
    fn acquire(counter: Arc<AtomicU32>, max: u32) -> Result<Self> {
        let current = counter.load(Ordering::Acquire);
        if current >= max {
            return Err(LambError::Control("export already active".to_string()));
        }
        counter.fetch_add(1, Ordering::AcqRel);
        Ok(Self {
            counter,
            consumed: false,
        })
    }

    /// Transfer the slot to the caller (typically the [`Snapshot`]).
    /// After this call the guard will NOT decrement the counter on drop.
    fn consume(mut self) -> Arc<AtomicU32> {
        self.consumed = true;
        Arc::clone(&self.counter)
    }
}

impl Drop for ActiveSnapshotGuard {
    fn drop(&mut self) {
        if !self.consumed {
            self.counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Collect chunk segments that overlap `[range_start, range_end)`.
///
/// Each candidate chunk is validated (Published, sequence-stable) and
/// pinned before a [`SnapshotSegment`] is appended.
fn collect_segments(
    chunks: &[Arc<Chunk>],
    range_start: u64,
    range_end: u64,
    segments: &mut Vec<SnapshotSegment>,
) {
    for chunk in chunks {
        let state = ChunkState::from_u8(chunk.state.load(Ordering::Acquire));
        if state != ChunkState::Published {
            continue;
        }
        let sequence = chunk.sequence.load(Ordering::Acquire);
        let valid_start = chunk.valid_start_frame.load(Ordering::Acquire);
        let valid_count = u64::from(chunk.valid_frame_count.load(Ordering::Acquire));
        let valid_end = valid_start + valid_count;
        if valid_end <= range_start || valid_start >= range_end {
            continue;
        }
        let overlap_start = valid_start.max(range_start);
        let overlap_end = valid_end.min(range_end);
        if overlap_start >= overlap_end {
            continue;
        }
        chunk.pin_count.fetch_add(1, Ordering::AcqRel);
        let state_after = ChunkState::from_u8(chunk.state.load(Ordering::Acquire));
        let sequence_after = chunk.sequence.load(Ordering::Acquire);
        if state_after != ChunkState::Published || sequence_after != sequence {
            chunk.pin_count.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        segments.push(SnapshotSegment {
            chunk: Arc::clone(chunk),
            expected_sequence: sequence,
            start_frame_in_chunk: (overlap_start - valid_start) as u32,
            frame_count: (overlap_end - overlap_start) as u32,
        });
    }
}

pub struct SampleRing {
    cfg: RingConfig,
    chunks: Vec<Arc<Chunk>>,
    write_chunk: Mutex<usize>,
    global_write_frame: AtomicU64,
    clear_after_frame: AtomicU64,
    next_sequence: AtomicU64,
    dropped_frames: AtomicU64,
    active_snapshots: Arc<AtomicU32>,
    last_overrun: Mutex<Option<SystemTime>>,
}

impl SampleRing {
    pub fn new(cfg: RingConfig) -> Result<Self> {
        if cfg.channels == 0 {
            return Err(LambError::Validation("channels must be > 0".to_string()));
        }
        if cfg.sample_rate == 0 {
            return Err(LambError::Validation("sample_rate must be > 0".to_string()));
        }
        if cfg.chunk_frames == 0 {
            return Err(LambError::Validation(
                "chunk_frames must be > 0".to_string(),
            ));
        }
        if cfg.chunk_count == 0 {
            return Err(LambError::Validation("chunk_count must be > 0".to_string()));
        }
        if cfg.max_active_snapshots == 0 {
            return Err(LambError::Validation(
                "max_active_snapshots must be > 0".to_string(),
            ));
        }
        let samples_per_chunk = usize::try_from(cfg.chunk_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(cfg.channels as usize))
            .ok_or_else(|| LambError::Validation("chunk allocation size overflow".to_string()))?;
        let mut chunks = Vec::with_capacity(cfg.chunk_count as usize);
        for _ in 0..cfg.chunk_count {
            chunks.push(Arc::new(Chunk {
                sequence: AtomicU64::new(0),
                state: AtomicU8::new(ChunkState::Writable as u8),
                pin_count: AtomicU32::new(0),
                valid_start_frame: AtomicU64::new(0),
                valid_frame_count: AtomicU32::new(0),
                data: Mutex::new(vec![0.0; samples_per_chunk]),
            }));
        }
        Ok(Self {
            cfg,
            chunks,
            write_chunk: Mutex::new(0),
            global_write_frame: AtomicU64::new(0),
            clear_after_frame: AtomicU64::new(0),
            next_sequence: AtomicU64::new(1),
            dropped_frames: AtomicU64::new(0),
            active_snapshots: Arc::new(AtomicU32::new(0)),
            last_overrun: Mutex::new(None),
        })
    }

    pub fn write_interleaved(&self, samples: &[f32], channels: u32) -> Result<()> {
        if channels != self.cfg.channels {
            return Err(LambError::Capture(format!(
                "incoming channels {channels} do not match ring channels {}",
                self.cfg.channels
            )));
        }
        if channels == 0 || !samples.len().is_multiple_of(channels as usize) {
            return Err(LambError::Capture(
                "input sample length is not whole frames".to_string(),
            ));
        }

        let total_frames = samples.len() / channels as usize;
        let mut frame_index = 0usize;
        while frame_index < total_frames {
            let global_frame = self.global_write_frame.load(Ordering::Acquire);
            let offset = (global_frame % u64::from(self.cfg.chunk_frames)) as u32;
            let mut write_chunk = self
                .write_chunk
                .lock()
                .map_err(|_| LambError::Capture("write chunk lock poisoned".to_string()))?;
            let chunk = Arc::clone(&self.chunks[*write_chunk]);

            if offset == 0 {
                if chunk.pin_count.load(Ordering::Acquire) > 0 {
                    let remaining = (total_frames - frame_index) as u64;
                    self.record_overrun(remaining);
                    break;
                }
                chunk
                    .state
                    .store(ChunkState::Writing as u8, Ordering::Release);
                chunk
                    .valid_start_frame
                    .store(global_frame, Ordering::Release);
                chunk.valid_frame_count.store(0, Ordering::Release);
                let sequence = self.next_sequence.fetch_add(1, Ordering::AcqRel);
                chunk.sequence.store(sequence, Ordering::Release);
            } else if chunk.pin_count.load(Ordering::Acquire) > 0 {
                let remaining = (total_frames - frame_index) as u64;
                self.record_overrun(remaining);
                break;
            } else {
                chunk
                    .state
                    .store(ChunkState::Writing as u8, Ordering::Release);
            }

            let frames_available = (self.cfg.chunk_frames - offset) as usize;
            let frames_to_copy = frames_available.min(total_frames - frame_index);
            {
                let mut data = chunk
                    .data
                    .lock()
                    .map_err(|_| LambError::Capture("chunk data lock poisoned".to_string()))?;
                let dst_start = offset as usize * channels as usize;
                let src_start = frame_index * channels as usize;
                let sample_count = frames_to_copy * channels as usize;
                data[dst_start..dst_start + sample_count]
                    .copy_from_slice(&samples[src_start..src_start + sample_count]);
            }
            let new_valid = offset + frames_to_copy as u32;
            chunk.valid_frame_count.store(new_valid, Ordering::Release);
            chunk
                .state
                .store(ChunkState::Published as u8, Ordering::Release);
            self.global_write_frame
                .fetch_add(frames_to_copy as u64, Ordering::AcqRel);
            frame_index += frames_to_copy;

            if new_valid >= self.cfg.chunk_frames {
                *write_chunk = (*write_chunk + 1) % self.chunks.len();
            }
        }
        Ok(())
    }

    pub fn snapshot_last_frames(&self, requested_frames: u64) -> Result<Snapshot> {
        let end_frame_before = self.global_write_frame.load(Ordering::Acquire);
        let clear_after = self.clear_after_frame.load(Ordering::Acquire);
        let start_frame = end_frame_before
            .saturating_sub(requested_frames)
            .max(clear_after);
        if start_frame >= end_frame_before {
            return Ok(Snapshot {
                segments: Vec::new(),
                channels: self.cfg.channels,
                sample_rate: self.cfg.sample_rate,
                format: self.cfg.format,
                total_frames: 0,
                active_counter: None,
            });
        }

        let guard = ActiveSnapshotGuard::acquire(
            Arc::clone(&self.active_snapshots),
            self.cfg.max_active_snapshots,
        )?;

        let mut segments = Vec::new();

        // First pass: capture chunks overlapping [start_frame, end_frame_before)
        collect_segments(&self.chunks, start_frame, end_frame_before, &mut segments);

        // Second pass: catch chunks published during first pass iteration.
        // The writer may have advanced global_write_frame and published new
        // chunks while we were iterating.  We scan the delta range so those
        // chunks are not silently lost (TOCTOU fix).
        let end_frame_after = self.global_write_frame.load(Ordering::Acquire);
        if end_frame_after > end_frame_before {
            collect_segments(
                &self.chunks,
                end_frame_before,
                end_frame_after,
                &mut segments,
            );
        }

        segments.sort_by_key(|segment| {
            segment.chunk.valid_start_frame.load(Ordering::Acquire)
                + u64::from(segment.start_frame_in_chunk)
        });
        let total_frames = segments
            .iter()
            .map(|segment| u64::from(segment.frame_count))
            .sum();
        if total_frames == 0 {
            return Ok(Snapshot {
                segments,
                channels: self.cfg.channels,
                sample_rate: self.cfg.sample_rate,
                format: self.cfg.format,
                total_frames,
                active_counter: None,
            });
        }
        Ok(Snapshot {
            segments,
            channels: self.cfg.channels,
            sample_rate: self.cfg.sample_rate,
            format: self.cfg.format,
            total_frames,
            active_counter: Some(guard.consume()),
        })
    }

    pub fn clear(&self) -> Result<()> {
        let current = self.global_write_frame.load(Ordering::Acquire);
        self.clear_after_frame.store(current, Ordering::Release);
        Ok(())
    }

    pub fn status(&self) -> RingStatus {
        let global = self.global_write_frame.load(Ordering::Acquire);
        let clear_after = self.clear_after_frame.load(Ordering::Acquire);
        let capacity = u64::from(self.cfg.chunk_frames) * u64::from(self.cfg.chunk_count);
        RingStatus {
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            retained_frames: global.saturating_sub(clear_after).min(capacity),
            capacity_frames: capacity,
            active_snapshots: self.active_snapshots.load(Ordering::Acquire),
            last_overrun: *self
                .last_overrun
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    fn record_overrun(&self, frames: u64) {
        self.dropped_frames.fetch_add(frames, Ordering::AcqRel);
        if let Ok(mut last) = self.last_overrun.lock() {
            *last = Some(SystemTime::now());
        }
    }

    /// Record frames dropped outside the normal write path (e.g. PipeWire
    /// buffer underrun, empty dequeues, format mismatches).  Does NOT
    /// update `last_overrun` because these are not chunk-pin backpressure.
    pub fn record_dropped_frames(&self, frames: u64) {
        self.dropped_frames.fetch_add(frames, Ordering::AcqRel);
    }
}

impl Snapshot {
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn format(&self) -> SampleFormat {
        self.format
    }

    pub fn segments(&self) -> &[SnapshotSegment] {
        &self.segments
    }

    pub fn read_channel_samples(&self, channel_index: u32) -> Result<Vec<f32>> {
        if channel_index >= self.channels {
            return Err(LambError::Export(format!(
                "channel index {channel_index} out of range for {} channels",
                self.channels
            )));
        }
        let mut out = Vec::with_capacity(self.total_frames as usize);
        for segment in &self.segments {
            let sequence = segment.chunk.sequence.load(Ordering::Acquire);
            let state = ChunkState::from_u8(segment.chunk.state.load(Ordering::Acquire));
            if sequence != segment.expected_sequence || state != ChunkState::Published {
                return Err(LambError::Export(
                    "snapshot segment generation mismatch".to_string(),
                ));
            }
            let data = segment
                .chunk
                .data
                .lock()
                .map_err(|_| LambError::Export("chunk data lock poisoned".to_string()))?;
            for frame_offset in 0..segment.frame_count {
                let frame = segment.start_frame_in_chunk + frame_offset;
                let index = frame as usize * self.channels as usize + channel_index as usize;
                out.push(data[index]);
            }
        }
        Ok(out)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        for segment in &self.segments {
            segment.chunk.pin_count.fetch_sub(1, Ordering::AcqRel);
        }
        if let Some(counter) = &self.active_counter {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}
