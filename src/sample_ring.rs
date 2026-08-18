use crate::error::{LambError, Result};
use crate::memory_plan::{
    ring_metadata_budget, ExactArray, MaterializedBuffer, RING_CHUNK_OBJECT_RESERVE_BYTES,
    RING_FIXED_METADATA_RESERVE_BYTES,
};
use std::mem::size_of;
use std::ops::Range;
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
    data: Mutex<MaterializedBuffer<f32>>,
}

const _: () = assert!(size_of::<Chunk>() as u64 <= RING_CHUNK_OBJECT_RESERVE_BYTES);
const _: () = assert!(size_of::<Arc<Chunk>>() == size_of::<usize>());

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
    start_frame: u64,
    end_frame: u64,
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
    total_frames: u64,
    active_counter: Option<Arc<AtomicU32>>,
}

#[derive(Debug)]
pub struct SnapshotSelection {
    requested_start: u64,
    oldest_frame: u64,
    snapshot: Snapshot,
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
    first_chunk: usize,
    range_start: u64,
    range_end: u64,
    segments: &mut Vec<SnapshotSegment>,
) {
    for offset in 0..chunks.len() {
        let chunk = &chunks[(first_chunk + offset) % chunks.len()];
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
    chunks: ExactArray<Arc<Chunk>>,
    write_chunk: Mutex<usize>,
    global_write_frame: AtomicU64,
    clear_after_frame: AtomicU64,
    next_sequence: AtomicU64,
    dropped_frames: AtomicU64,
    active_snapshots: Arc<AtomicU32>,
    last_overrun: Mutex<Option<SystemTime>>,
    allocated_sample_bytes: u64,
    metadata_budget_bytes: u64,
}

const _: () = assert!(size_of::<SampleRing>() as u64 <= RING_FIXED_METADATA_RESERVE_BYTES);

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
        let chunk_count = usize::try_from(cfg.chunk_count)
            .map_err(|_| LambError::Validation("ring chunk count overflow".to_string()))?;
        let mut allocated_sample_bytes = 0_u64;
        let chunks = ExactArray::try_from_fn(chunk_count, |_| {
            let data = MaterializedBuffer::new_zeroed(samples_per_chunk)?;
            let data_bytes = u64::try_from(data.allocated_bytes()).map_err(|_| {
                LambError::Validation("ring sample byte count overflow".to_string())
            })?;
            allocated_sample_bytes =
                allocated_sample_bytes
                    .checked_add(data_bytes)
                    .ok_or_else(|| {
                        LambError::Validation("ring sample byte count overflow".to_string())
                    })?;
            Ok(Arc::new(Chunk {
                sequence: AtomicU64::new(0),
                state: AtomicU8::new(ChunkState::Writable as u8),
                pin_count: AtomicU32::new(0),
                valid_start_frame: AtomicU64::new(0),
                valid_frame_count: AtomicU32::new(0),
                data: Mutex::new(data),
            }))
        })?;
        let metadata_budget_bytes = ring_metadata_budget(
            u64::try_from(chunks.len())
                .map_err(|_| LambError::Validation("ring chunk count overflow".to_string()))?,
            u64::try_from(samples_per_chunk)
                .ok()
                .and_then(|samples| samples.checked_mul(size_of::<f32>() as u64))
                .ok_or_else(|| {
                    LambError::Validation("ring chunk sample byte count overflow".to_string())
                })?,
        )?
        .total()?;
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
            allocated_sample_bytes,
            metadata_budget_bytes,
        })
    }

    pub fn materialize_pages(&self) -> Result<()> {
        for chunk in self.chunks.iter() {
            let mut data = chunk
                .data
                .lock()
                .map_err(|_| LambError::Capture("chunk data lock poisoned".to_string()))?;
            data.materialize_pages()?;
        }
        Ok(())
    }

    pub fn allocated_sample_bytes(&self) -> u64 {
        self.allocated_sample_bytes
    }

    pub fn metadata_budget_bytes(&self) -> u64 {
        self.metadata_budget_bytes
    }

    pub fn reset(&self) -> Result<()> {
        let mut write_chunk = self
            .write_chunk
            .lock()
            .map_err(|_| LambError::Control("write chunk lock poisoned".to_string()))?;
        let mut last_overrun = self
            .last_overrun
            .lock()
            .map_err(|_| LambError::Control("last overrun lock poisoned".to_string()))?;
        if self.active_snapshots.load(Ordering::Acquire) != 0
            || self
                .chunks
                .iter()
                .any(|chunk| chunk.pin_count.load(Ordering::Acquire) != 0)
        {
            return Err(LambError::Control(
                "cannot reset ring while a snapshot is active".to_string(),
            ));
        }
        if self.chunks.iter().any(|chunk| {
            ChunkState::from_u8(chunk.state.load(Ordering::Acquire)) == ChunkState::Writing
        }) {
            return Err(LambError::Control(
                "cannot reset ring while a writer is active".to_string(),
            ));
        }

        for chunk in self.chunks.iter() {
            chunk.sequence.store(0, Ordering::Release);
            chunk.valid_start_frame.store(0, Ordering::Release);
            chunk.valid_frame_count.store(0, Ordering::Release);
            chunk
                .state
                .store(ChunkState::Writable as u8, Ordering::Release);
        }
        *write_chunk = 0;
        self.global_write_frame.store(0, Ordering::Release);
        self.clear_after_frame.store(0, Ordering::Release);
        self.next_sequence.store(1, Ordering::Release);
        self.dropped_frames.store(0, Ordering::Release);
        *last_overrun = None;
        Ok(())
    }

    pub fn copy_interleaved_range_into(
        &self,
        range: Range<u64>,
        destination: &mut [f32],
    ) -> Result<u64> {
        if range.start > range.end {
            return Err(LambError::ExportInvariant("copy range start exceeds end"));
        }
        let channels = self.cfg.channels as usize;
        if !destination.len().is_multiple_of(channels) {
            return Err(LambError::ExportInvariant(
                "destination sample length is not whole frames",
            ));
        }

        let _writer = self
            .write_chunk
            .lock()
            .map_err(|_| LambError::ExportInvariant("write chunk lock poisoned"))?;
        let head = self.write_head_frame();
        let oldest = self.oldest_frame_at(head);
        if range.start < oldest || range.end > head {
            return Err(LambError::ExportInvariant(
                "copy range is outside retained range",
            ));
        }

        let destination_frames = destination.len() / channels;
        let destination_frames = u64::try_from(destination_frames)
            .map_err(|_| LambError::ExportInvariant("destination frame count overflow"))?;
        let capacity_end = range
            .start
            .checked_add(destination_frames)
            .ok_or(LambError::ExportInvariant("copy frame range overflow"))?;
        let copy_end = range.end.min(capacity_end);
        let mut cursor = range.start;
        let mut destination_frame = 0_usize;
        while cursor < copy_end {
            let chunk_index =
                ((cursor / u64::from(self.cfg.chunk_frames)) % self.chunks.len() as u64) as usize;
            let chunk = &self.chunks[chunk_index];
            if ChunkState::from_u8(chunk.state.load(Ordering::Acquire)) != ChunkState::Published {
                return Err(LambError::ExportInvariant(
                    "copy range has incomplete coverage",
                ));
            }
            let valid_start = chunk.valid_start_frame.load(Ordering::Acquire);
            let valid_end = valid_start
                .checked_add(u64::from(chunk.valid_frame_count.load(Ordering::Acquire)))
                .ok_or(LambError::ExportInvariant("chunk frame range overflow"))?;
            if cursor < valid_start || cursor >= valid_end {
                return Err(LambError::ExportInvariant(
                    "copy range has incomplete coverage",
                ));
            }
            let segment_end = copy_end.min(valid_end);
            let segment_frames = usize::try_from(segment_end - cursor)
                .map_err(|_| LambError::ExportInvariant("copy frame count overflow"))?;
            let source_frame = usize::try_from(cursor - valid_start)
                .map_err(|_| LambError::ExportInvariant("copy frame offset overflow"))?;
            let source_start = source_frame
                .checked_mul(channels)
                .ok_or(LambError::ExportInvariant("copy sample offset overflow"))?;
            let sample_count = segment_frames
                .checked_mul(channels)
                .ok_or(LambError::ExportInvariant("copy sample count overflow"))?;
            let destination_start =
                destination_frame
                    .checked_mul(channels)
                    .ok_or(LambError::ExportInvariant(
                        "destination sample offset overflow",
                    ))?;
            let data = chunk
                .data
                .lock()
                .map_err(|_| LambError::ExportInvariant("chunk data lock poisoned"))?;
            destination[destination_start..destination_start + sample_count]
                .copy_from_slice(&data[source_start..source_start + sample_count]);
            cursor = segment_end;
            destination_frame += segment_frames;
        }
        u64::try_from(destination_frame)
            .map_err(|_| LambError::ExportInvariant("copied frame count overflow"))
    }

    pub fn write_interleaved(&self, samples: &[f32], channels: u32) -> Result<()> {
        if channels != self.cfg.channels {
            return Err(LambError::CaptureInvariant(
                "incoming channels do not match ring channels",
            ));
        }
        if channels == 0 || !samples.len().is_multiple_of(channels as usize) {
            return Err(LambError::CaptureInvariant(
                "input sample length is not whole frames",
            ));
        }

        let total_frames = samples.len() / channels as usize;
        let mut frame_index = 0usize;
        while frame_index < total_frames {
            let mut write_chunk = self
                .write_chunk
                .lock()
                .map_err(|_| LambError::CaptureInvariant("write chunk lock poisoned"))?;
            let global_frame = self.global_write_frame.load(Ordering::Acquire);
            let offset = (global_frame % u64::from(self.cfg.chunk_frames)) as u32;
            let chunk = Arc::clone(&self.chunks[*write_chunk]);

            let frames_available = (self.cfg.chunk_frames - offset) as usize;
            let frames_to_copy = frames_available.min(total_frames - frame_index);
            let frames_to_copy_u64 = u64::try_from(frames_to_copy)
                .map_err(|_| LambError::CaptureInvariant("local frame count overflow"))?;
            let new_global = global_frame
                .checked_add(frames_to_copy_u64)
                .ok_or(LambError::CaptureInvariant("local frame counter exhausted"))?;
            let next_sequence = if offset == 0 {
                Some(
                    self.next_sequence
                        .load(Ordering::Acquire)
                        .checked_add(1)
                        .ok_or(LambError::CaptureInvariant("ring sequence exhausted"))?,
                )
            } else {
                None
            };

            if offset == 0 {
                if chunk.pin_count.load(Ordering::Acquire) > 0 {
                    let remaining = u64::try_from(total_frames - frame_index)
                        .map_err(|_| LambError::CaptureInvariant("dropped frame count overflow"))?;
                    self.record_overrun(remaining)?;
                    break;
                }
                chunk
                    .state
                    .store(ChunkState::Writing as u8, Ordering::Release);
                chunk
                    .valid_start_frame
                    .store(global_frame, Ordering::Release);
                chunk.valid_frame_count.store(0, Ordering::Release);
                let sequence = self.next_sequence.load(Ordering::Acquire);
                let next_sequence = next_sequence.ok_or(LambError::CaptureInvariant(
                    "ring sequence state is inconsistent",
                ))?;
                self.next_sequence.store(next_sequence, Ordering::Release);
                chunk.sequence.store(sequence, Ordering::Release);
            } else if chunk.pin_count.load(Ordering::Acquire) > 0 {
                let remaining = u64::try_from(total_frames - frame_index)
                    .map_err(|_| LambError::CaptureInvariant("dropped frame count overflow"))?;
                self.record_overrun(remaining)?;
                break;
            } else {
                chunk
                    .state
                    .store(ChunkState::Writing as u8, Ordering::Release);
            }

            {
                let mut data = chunk
                    .data
                    .lock()
                    .map_err(|_| LambError::CaptureInvariant("chunk data lock poisoned"))?;
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
            self.global_write_frame.store(new_global, Ordering::Release);
            frame_index += frames_to_copy;

            if new_valid >= self.cfg.chunk_frames {
                *write_chunk = (*write_chunk + 1) % self.chunks.len();
            }
        }
        Ok(())
    }

    pub fn snapshot_last_frames(&self, requested_frames: u64) -> Result<Snapshot> {
        let mut segments = Vec::with_capacity(self.chunks.len());
        let (range, guard) = {
            let _writer = self
                .write_chunk
                .lock()
                .map_err(|_| LambError::Control("write chunk lock poisoned".to_string()))?;
            let head = self.write_head_frame();
            let oldest = self.oldest_frame_at(head);
            let range = head.saturating_sub(requested_frames).max(oldest)..head;
            let guard = self.pin_snapshot_range(&range, head, oldest, &mut segments)?;
            (range, guard)
        };
        self.finish_snapshot(range, segments, guard)
    }

    pub fn write_head_frame(&self) -> u64 {
        self.global_write_frame.load(Ordering::Acquire)
    }

    pub fn oldest_frame(&self) -> u64 {
        let _writer = self
            .write_chunk
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let head = self.write_head_frame();
        self.oldest_frame_at(head)
    }

    fn oldest_frame_at(&self, head: u64) -> u64 {
        let mut oldest = head;
        for _ in 0..self.chunks.len() {
            if oldest == 0 {
                break;
            }
            let preceding_frame = oldest - 1;
            let chunk_index = ((preceding_frame / u64::from(self.cfg.chunk_frames))
                % self.chunks.len() as u64) as usize;
            let chunk = &self.chunks[chunk_index];
            if ChunkState::from_u8(chunk.state.load(Ordering::Acquire)) != ChunkState::Published {
                break;
            }
            let start = chunk.valid_start_frame.load(Ordering::Acquire);
            let end = start + u64::from(chunk.valid_frame_count.load(Ordering::Acquire));
            if start >= oldest || end < oldest {
                break;
            }
            oldest = start;
        }
        oldest.max(self.clear_after_frame.load(Ordering::Acquire))
    }

    pub fn snapshot_range(&self, range: Range<u64>) -> Result<Snapshot> {
        let mut segments = Vec::with_capacity(self.chunks.len());
        let guard = {
            let _writer = self
                .write_chunk
                .lock()
                .map_err(|_| LambError::Control("write chunk lock poisoned".to_string()))?;
            let head = self.write_head_frame();
            let oldest = self.oldest_frame_at(head);
            self.pin_snapshot_range(&range, head, oldest, &mut segments)?
        };
        self.finish_snapshot(range, segments, guard)
    }

    pub fn select_snapshot(&self, committed_start: Option<u64>) -> Result<SnapshotSelection> {
        let mut segments = Vec::with_capacity(self.chunks.len());
        let (requested_start, oldest_frame, range, guard) = {
            let _writer = self
                .write_chunk
                .lock()
                .map_err(|_| LambError::Control("write chunk lock poisoned".to_string()))?;
            let head = self.write_head_frame();
            let oldest_frame = self.oldest_frame_at(head);
            let requested_start = committed_start.unwrap_or(oldest_frame);
            let range = requested_start.max(oldest_frame)..head;
            let guard = self.pin_snapshot_range(&range, head, oldest_frame, &mut segments)?;
            (requested_start, oldest_frame, range, guard)
        };
        let snapshot = self.finish_snapshot(range, segments, guard)?;
        Ok(SnapshotSelection {
            requested_start,
            oldest_frame,
            snapshot,
        })
    }

    /// Validate the captured bounds and pin overlaps while `write_chunk` is held.
    fn pin_snapshot_range(
        &self,
        range: &Range<u64>,
        head: u64,
        oldest: u64,
        segments: &mut Vec<SnapshotSegment>,
    ) -> Result<Option<ActiveSnapshotGuard>> {
        if range.start > range.end {
            return Err(LambError::Control(format!(
                "snapshot range start {} exceeds end {}",
                range.start, range.end
            )));
        }
        if range.start < oldest {
            return Err(LambError::Control(format!(
                "snapshot range starts at {}, before oldest retained frame {oldest}",
                range.start
            )));
        }
        if range.end > head {
            return Err(LambError::Control(format!(
                "snapshot range ends at {}, after write head {head}",
                range.end
            )));
        }
        if range.start == range.end {
            return Ok(None);
        }

        let guard = ActiveSnapshotGuard::acquire(
            Arc::clone(&self.active_snapshots),
            self.cfg.max_active_snapshots,
        )?;
        let first_chunk =
            ((range.start / u64::from(self.cfg.chunk_frames)) % self.chunks.len() as u64) as usize;
        collect_segments(&self.chunks, first_chunk, range.start, range.end, segments);
        Ok(Some(guard))
    }

    /// Sort and prove exact coverage after the writer lock has been released.
    fn finish_snapshot(
        &self,
        range: Range<u64>,
        mut segments: Vec<SnapshotSegment>,
        guard: Option<ActiveSnapshotGuard>,
    ) -> Result<Snapshot> {
        segments.sort_by_key(|segment| {
            segment.chunk.valid_start_frame.load(Ordering::Acquire)
                + u64::from(segment.start_frame_in_chunk)
        });
        let total_frames = segments
            .iter()
            .map(|segment| u64::from(segment.frame_count))
            .sum();
        let mut snapshot = Snapshot {
            segments,
            start_frame: range.start,
            end_frame: range.end,
            channels: self.cfg.channels,
            sample_rate: self.cfg.sample_rate,
            format: self.cfg.format,
            total_frames,
            active_counter: None,
        };

        let mut expected_start = range.start;
        for segment in &snapshot.segments {
            let segment_start = segment.chunk.valid_start_frame.load(Ordering::Acquire)
                + u64::from(segment.start_frame_in_chunk);
            if segment_start != expected_start {
                return Err(LambError::Control(format!(
                    "snapshot range has incomplete coverage at frame {expected_start}"
                )));
            }
            expected_start = segment_start + u64::from(segment.frame_count);
        }
        if expected_start != range.end {
            return Err(LambError::Control(format!(
                "snapshot range has incomplete coverage at frame {expected_start}"
            )));
        }

        snapshot.active_counter = guard.map(ActiveSnapshotGuard::consume);
        Ok(snapshot)
    }

    pub fn clear(&self) -> Result<()> {
        let current = self.global_write_frame.load(Ordering::Acquire);
        self.clear_after_frame.store(current, Ordering::Release);
        Ok(())
    }

    pub fn status(&self) -> RingStatus {
        let (global, oldest) = {
            let _writer = self
                .write_chunk
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let global = self.global_write_frame.load(Ordering::Acquire);
            (global, self.oldest_frame_at(global))
        };
        let capacity = u64::from(self.cfg.chunk_frames) * u64::from(self.cfg.chunk_count);
        RingStatus {
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            retained_frames: global.saturating_sub(oldest),
            capacity_frames: capacity,
            active_snapshots: self.active_snapshots.load(Ordering::Acquire),
            last_overrun: *self
                .last_overrun
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    fn record_overrun(&self, frames: u64) -> Result<()> {
        self.add_dropped_frames(frames)?;
        if let Ok(mut last) = self.last_overrun.lock() {
            *last = Some(SystemTime::now());
        }
        Ok(())
    }

    /// Record frames dropped outside the normal write path (e.g. PipeWire
    /// buffer underrun, empty dequeues, format mismatches).  Does NOT
    /// update `last_overrun` because these are not chunk-pin backpressure.
    pub fn record_dropped_frames(&self, frames: u64) -> Result<()> {
        self.add_dropped_frames(frames)
    }

    fn add_dropped_frames(&self, frames: u64) -> Result<()> {
        self.dropped_frames
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(frames)
            })
            .map(|_| ())
            .map_err(|_| LambError::CaptureInvariant("dropped frame counter exhausted"))
    }
}

impl Snapshot {
    pub fn start_frame(&self) -> u64 {
        self.start_frame
    }

    pub fn end_frame(&self) -> u64 {
        self.end_frame
    }

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

impl SnapshotSelection {
    pub fn requested_start(&self) -> u64 {
        self.requested_start
    }

    pub fn oldest_frame(&self) -> u64 {
        self.oldest_frame
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> Snapshot {
        self.snapshot
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

#[cfg(all(test, target_os = "linux"))]
mod materialization_tests {
    use super::{ChunkState, RingConfig, SampleFormat, SampleRing};
    use crate::error::LambError;
    use std::mem::size_of;

    #[test]
    fn ring_chunk_index_has_exact_stable_storage() {
        let ring = SampleRing::new(RingConfig {
            channels: 2,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 3,
            max_active_snapshots: 1,
        })
        .unwrap();
        let address = ring.chunks.as_slice().as_ptr();

        assert_eq!(ring.chunks.len(), 3);
        assert_eq!(
            ring.chunks.allocated_bytes(),
            3 * size_of::<std::sync::Arc<super::Chunk>>()
        );
        ring.write_interleaved(&[1.0, 2.0, 3.0, 4.0], 2).unwrap();
        assert_eq!(ring.chunks.as_slice().as_ptr(), address);
    }

    #[test]
    fn ring_sample_arenas_have_exact_resident_f32_layouts() {
        let ring = SampleRing::new(RingConfig {
            channels: 2,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 1_025,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap();
        let page_size = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();

        for chunk in ring.chunks.iter() {
            let data = chunk.data.lock().unwrap();
            assert_eq!(data.allocated_bytes(), 1_025 * 2 * size_of::<f32>());
            assert!(data.as_slice().iter().all(|sample| *sample == 0.0));

            let start = data.as_slice().as_ptr() as usize;
            let end = start.checked_add(data.allocated_bytes()).unwrap();
            let page_start = start / page_size * page_size;
            let page_end = end.div_ceil(page_size) * page_size;
            let mut residency = vec![0_u8; (page_end - page_start) / page_size];
            let result = unsafe {
                libc::mincore(
                    page_start as *mut libc::c_void,
                    page_end - page_start,
                    residency.as_mut_ptr(),
                )
            };
            assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
            assert!(residency.iter().all(|entry| entry & 1 == 1));
        }
    }

    #[test]
    fn reset_reuses_exact_chunk_index_and_sample_allocations() {
        let ring = SampleRing::new(RingConfig {
            channels: 2,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 3,
            max_active_snapshots: 1,
        })
        .unwrap();
        let index_address = ring.chunks.as_slice().as_ptr();
        let sample_addresses = ring
            .chunks
            .iter()
            .map(|chunk| chunk.data.lock().unwrap().as_slice().as_ptr())
            .collect::<Vec<_>>();

        ring.write_interleaved(&[1.0; 16], 2).unwrap();
        ring.reset().unwrap();

        assert_eq!(ring.chunks.as_slice().as_ptr(), index_address);
        for (chunk, expected) in ring.chunks.iter().zip(sample_addresses) {
            assert_eq!(chunk.data.lock().unwrap().as_slice().as_ptr(), expected);
            assert_eq!(chunk.sequence.load(std::sync::atomic::Ordering::Acquire), 0);
            assert_eq!(
                chunk
                    .valid_start_frame
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            assert_eq!(
                chunk
                    .valid_frame_count
                    .load(std::sync::atomic::Ordering::Acquire),
                0
            );
            assert_eq!(
                chunk.pin_count.load(std::sync::atomic::Ordering::Acquire),
                0
            );
            assert_eq!(
                super::ChunkState::from_u8(chunk.state.load(std::sync::atomic::Ordering::Acquire)),
                super::ChunkState::Writable
            );
        }
    }

    #[test]
    fn reset_failure_does_not_partially_clear_ring_metadata() {
        let ring = SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap();
        ring.write_interleaved(&[1.0, 2.0], 1).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _last_overrun = ring.last_overrun.lock().unwrap();
            panic!("poison last-overrun metadata");
        }));

        assert!(ring.reset().is_err());
        assert_eq!(ring.write_head_frame(), 2);
        assert_eq!(ring.oldest_frame(), 0);
    }

    #[test]
    fn local_frame_overflow_returns_static_error_before_mutation() {
        let ring = SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap();
        ring.global_write_frame
            .store(u64::MAX, std::sync::atomic::Ordering::Release);

        assert!(matches!(
            ring.write_interleaved(&[1.0], 1),
            Err(LambError::CaptureInvariant("local frame counter exhausted"))
        ));
        assert_eq!(ring.write_head_frame(), u64::MAX);
    }

    #[test]
    fn sequence_overflow_returns_static_error_before_chunk_mutation() {
        let ring = SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap();
        ring.next_sequence
            .store(u64::MAX, std::sync::atomic::Ordering::Release);

        assert!(matches!(
            ring.write_interleaved(&[1.0], 1),
            Err(LambError::CaptureInvariant("ring sequence exhausted"))
        ));
        assert_eq!(ring.write_head_frame(), 0);
        assert_eq!(
            ChunkState::from_u8(
                ring.chunks[0]
                    .state
                    .load(std::sync::atomic::Ordering::Acquire)
            ),
            ChunkState::Writable
        );
    }

    #[test]
    fn dropped_counter_overflow_returns_static_error_without_wrap() {
        let ring = SampleRing::new(RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 2,
            max_active_snapshots: 1,
        })
        .unwrap();
        ring.dropped_frames
            .store(u64::MAX, std::sync::atomic::Ordering::Release);

        assert!(matches!(
            ring.record_dropped_frames(1),
            Err(LambError::CaptureInvariant(
                "dropped frame counter exhausted"
            ))
        ));
        assert_eq!(
            ring.dropped_frames
                .load(std::sync::atomic::Ordering::Acquire),
            u64::MAX
        );
    }
}
