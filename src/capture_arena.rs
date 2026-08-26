use crate::error::{LambError, Result};
use crate::memory_plan::{
    ExactArray, MaterializedBuffer, SessionMemoryPlan, CALIBRATION_SLOT_METADATA_BYTES,
    CAPTURE_COMMAND_RESULT_SLOT_BYTES, CAPTURE_QUEUE_SLOT_METADATA_BYTES,
};
use crate::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::cell::{Cell, UnsafeCell};
use std::fmt;
use std::marker::PhantomData;
use std::mem::{size_of, MaybeUninit};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

const EPOCH_COUNT: usize = 2;
const COMMAND_IDLE: u8 = 0;
const COMMAND_PREPARING: u8 = 1;
const COMMAND_READY: u8 = 2;
const COMMAND_TAKING: u8 = 3;
const COMMAND_PROCESSING: u8 = 4;
const COMMAND_RESULT_READY: u8 = 5;
const COMMAND_RESULT_TAKING: u8 = 6;
const COMMAND_KIND_FREEZE: u8 = 1;
const COMMAND_KIND_RELEASE: u8 = 2;
const COMMAND_KIND_CLEAR: u8 = 3;
const COMMAND_KIND_STATUS: u8 = 4;
const COMMAND_KIND_SHUTDOWN: u8 = 5;
const CALIBRATION_IDLE: u8 = 0;
const CALIBRATION_READY: u8 = 1;
const CALIBRATION_CAPTURING: u8 = 2;
const CALIBRATION_COMPLETE: u8 = 3;
const CALIBRATION_LEASED: u8 = 4;
const CALIBRATION_PREPARING: u8 = 5;
const CALIBRATION_CANCELLED: u8 = 6;
const CALIBRATION_FAILED: u8 = 7;
const PRODUCER_OPEN: u64 = 1 << 63;
const PRODUCER_COUNT_MASK: u64 = PRODUCER_OPEN - 1;
const WORKER_IDLE_WAIT: Duration = Duration::from_millis(10);
static NEXT_CAPTURE_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePushOutcome {
    pub enqueued_frames: u64,
    pub dropped_frames: u64,
    pub published_sequence: u64,
}

#[derive(Debug, Clone)]
pub struct CaptureArenaStatus {
    pub dropped_frames: u64,
    pub retained_frames: u64,
    pub capacity_frames: u64,
    pub last_overrun: Option<SystemTime>,
    pub active_absolute_range: Range<u64>,
    pub ingress_enqueued_frames: u64,
    pub capture_dropped_frames: u64,
    pub worker_written_frames: u64,
    pub worker_dropped_frames: u64,
    pub frozen_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureClearReport {
    pub arena_runtime_id: u64,
    pub coordinator_id: u64,
    pub clear_id: u64,
    pub expected_active_start: u64,
    pub active_absolute_range: Range<u64>,
    pub pending_retention_lost_frames: u64,
    pub pending_cleared_frames: u64,
    pub cumulative_dropped_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureClearAccounting {
    pub arena_runtime_id: u64,
    pub coordinator_id: u64,
    pub clear_id: u64,
    pub expected_active_start: u64,
    pub pending_retention_lost_frames: u64,
    pub pending_cleared_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureClearRecovery {
    pub arena_runtime_id: u64,
    pub coordinator_id: u64,
    pub clear_id: u64,
}

impl CaptureClearRecovery {
    fn matches(self, report: &CaptureClearReport) -> bool {
        self.arena_runtime_id == report.arena_runtime_id
            && self.coordinator_id == report.coordinator_id
            && self.clear_id == report.clear_id
    }
}

#[derive(Debug, Clone)]
pub struct CaptureRuntimeConfig {
    pub ring: RingConfig,
    pub queue_slots: u32,
    pub slot_frames: u32,
    pub sample_bytes: u32,
    pub worker_stack_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CalibrationCaptureRequest {
    pub channel: u32,
    pub frames: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CalibrationCaptureMetadata {
    pub frames: u64,
    pub sample_rate: u32,
    pub complete_windows: u64,
    /// Frames present in the latest started window when that trailing window is incomplete.
    pub partial_final_frames: u64,
    pub dropped_frames: u64,
    pub usable: bool,
}

pub struct CalibrationLease<'a> {
    slot: &'a CalibrationSlot,
    metadata: CalibrationCaptureMetadata,
}

impl CalibrationLease<'_> {
    pub fn samples(&self) -> &[f32] {
        unsafe { &(*self.slot.samples.get()).as_slice()[..self.metadata.frames as usize] }
    }
    pub fn rms(&self) -> &[f32] {
        unsafe { &(*self.slot.rms.get()).as_slice()[..self.metadata.complete_windows as usize] }
    }
    /// The lease is the sole owner while the calibration slot is `LEASED`, so
    /// percentile selection may reorder the complete RMS prefix in place.
    pub fn rms_mut(&mut self) -> &mut [f32] {
        unsafe {
            &mut (*self.slot.rms.get()).as_mut_slice()[..self.metadata.complete_windows as usize]
        }
    }
    /// Returns disjoint leased buffers for persistence without allocating a
    /// recording-sized copy. The slot cannot be reused until this lease drops.
    pub fn persistence_parts_mut(&mut self) -> (&[f32], &mut [f32], &[f32]) {
        unsafe {
            let frames = self.metadata.frames as usize;
            let windows = self.metadata.complete_windows as usize;
            (
                &(*self.slot.samples.get()).as_slice()[..frames],
                &mut (*self.slot.rms.get()).as_mut_slice()[..windows],
                &(*self.slot.peak.get()).as_slice()[..windows],
            )
        }
    }
    pub fn peak(&self) -> &[f32] {
        unsafe { &(*self.slot.peak.get()).as_slice()[..self.metadata.complete_windows as usize] }
    }
    pub fn sample_rate(&self) -> u32 {
        self.metadata.sample_rate
    }
    pub fn complete_windows(&self) -> u64 {
        self.metadata.complete_windows
    }
    /// Returns the frame count in the latest trailing incomplete window, or zero.
    pub fn partial_final_frames(&self) -> u64 {
        self.metadata.partial_final_frames
    }
    pub fn metadata(&self) -> CalibrationCaptureMetadata {
        self.metadata
    }
}

impl Drop for CalibrationLease<'_> {
    fn drop(&mut self) {
        let _ = self.slot.state.compare_exchange(
            CALIBRATION_LEASED,
            CALIBRATION_IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub struct CaptureIngress {
    queue: Arc<IngressQueue>,
    wake: Arc<WorkerWake>,
    _single_producer: PhantomData<Cell<()>>,
}

pub struct CaptureArena {
    shared: Arc<RuntimeShared>,
    command_client: Mutex<CommandClientState>,
    worker: Option<JoinHandle<()>>,
    runtime_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrozenEpochIdentity {
    runtime_id: u64,
    index: usize,
    generation: u64,
}

impl fmt::Debug for CaptureArena {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureArena")
            .field("worker_running", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

pub struct FrozenCaptureEpoch {
    runtime: Arc<RuntimeShared>,
    ring: Arc<SampleRing>,
    runtime_id: u64,
    index: usize,
    generation: u64,
    base: u64,
    absolute_range: Range<u64>,
    local_range: Range<u64>,
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
    released: bool,
    release_pending: bool,
}

impl fmt::Debug for FrozenCaptureEpoch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrozenCaptureEpoch")
            .field("absolute_range", &self.absolute_range)
            .field("local_range", &self.local_range)
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

struct IngressSlot {
    storage: UnsafeCell<IngressSlotStorage>,
}

struct IngressSlotStorage {
    frames: u32,
    samples: MaterializedBuffer<f32>,
}

unsafe impl Sync for IngressSlot {}

const _: () = assert!(size_of::<IngressSlot>() as u64 <= CAPTURE_QUEUE_SLOT_METADATA_BYTES);

struct IngressQueue {
    slots: ExactArray<IngressSlot>,
    channels: u32,
    slot_frames: u32,
    published_sequence: AtomicU64,
    consumed_sequence: AtomicU64,
    enqueued_frames: AtomicU64,
    dropped_frames: AtomicU64,
    cumulative_dropped_frames: AtomicU64,
    drop_counter_exhausted: AtomicBool,
    producer_admission: AtomicU64,
    producer_generation: AtomicU64,
    producer_generation_exhausted: AtomicBool,
}

struct WorkerWake {
    mutex: Mutex<()>,
    condvar: Condvar,
}

struct RuntimeShared {
    queue: Arc<IngressQueue>,
    command: Arc<CommandSlot>,
    calibration: Arc<CalibrationSlot>,
    wake: Arc<WorkerWake>,
    worker_done: AtomicBool,
    final_worker_written_frames: AtomicU64,
    final_worker_dropped_frames: AtomicU64,
    #[cfg(test)]
    shutdown_reply_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    clear_reply_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_start_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_before_accept_pause:
        Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_after_consume_pause:
        Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_timeout_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_wait_pause: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    calibration_fault_after_start: AtomicBool,
}

struct CalibrationSlot {
    state: AtomicU8,
    request: UnsafeCell<MaybeUninit<CalibrationRequest>>,
    metadata: UnsafeCell<CalibrationCaptureMetadata>,
    samples: UnsafeCell<MaterializedBuffer<f32>>,
    rms: UnsafeCell<MaterializedBuffer<f32>>,
    peak: UnsafeCell<MaterializedBuffer<f32>>,
    window_frames: u64,
    hop_frames: u64,
    wait_mutex: Mutex<()>,
    wait_condvar: Condvar,
}

#[derive(Clone, Copy)]
struct CalibrationRequest {
    channel: u32,
    frames: u64,
}
unsafe impl Sync for CalibrationSlot {}
const _: () = assert!(size_of::<CalibrationSlot>() as u64 <= CALIBRATION_SLOT_METADATA_BYTES);

struct CommandSlot {
    state: AtomicU8,
    kind: AtomicU8,
    abandoned: AtomicBool,
    command: UnsafeCell<MaybeUninit<WorkerCommand>>,
    result: UnsafeCell<MaybeUninit<Result<WorkerReply>>>,
    #[cfg(test)]
    submission_count: AtomicU64,
}

struct CommandClientState {
    recovered_frozen: Option<FrozenDescriptor>,
    recovered_clear: Option<CaptureClearReport>,
    shutdown: CommandClientShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandClientShutdown {
    NotRequested,
    Acknowledged,
    Joined,
}

impl Default for CommandClientState {
    fn default() -> Self {
        Self {
            recovered_frozen: None,
            recovered_clear: None,
            shutdown: CommandClientShutdown::NotRequested,
        }
    }
}

unsafe impl Sync for CommandSlot {}

const _: () = assert!(size_of::<CommandSlot>() as u64 <= CAPTURE_COMMAND_RESULT_SLOT_BYTES);

enum WorkerCommand {
    Freeze {
        target_sequence: u64,
        committed_start: Option<u64>,
    },
    Release {
        target_sequence: u64,
        index: usize,
        generation: u64,
    },
    Clear {
        target_sequence: u64,
        ingress_dropped_frames: u64,
        accounting: CaptureClearAccounting,
    },
    Status {
        target_sequence: u64,
    },
    Shutdown {
        target_sequence: u64,
    },
    #[cfg(test)]
    #[allow(dead_code)]
    DropProbe(tests::DropProbe),
}

impl WorkerCommand {
    fn target_sequence(&self) -> u64 {
        match self {
            Self::Freeze {
                target_sequence, ..
            }
            | Self::Release {
                target_sequence, ..
            }
            | Self::Clear {
                target_sequence, ..
            }
            | Self::Status { target_sequence }
            | Self::Shutdown { target_sequence } => *target_sequence,
            #[cfg(test)]
            Self::DropProbe(_) => 0,
        }
    }
}

enum WorkerReply {
    Frozen(Option<FrozenDescriptor>),
    Released,
    Cleared(CaptureClearReport),
    Status(CaptureArenaStatus),
    Shutdown,
    #[cfg(test)]
    #[allow(dead_code)]
    DropProbe(tests::DropProbe),
}

struct FrozenDescriptor {
    ring: Arc<SampleRing>,
    index: usize,
    generation: u64,
    base: u64,
    absolute_range: Range<u64>,
    local_range: Range<u64>,
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
}

struct FrozenRecord {
    index: usize,
    generation: u64,
}

struct WorkerState {
    shared: Arc<RuntimeShared>,
    epochs: [Arc<SampleRing>; EPOCH_COUNT],
    active: usize,
    bases: [u64; EPOCH_COUNT],
    absolute_head: u64,
    next_generation: u64,
    frozen: Option<FrozenRecord>,
    retired_dropped_frames: u64,
    retired_last_overrun: Option<SystemTime>,
    worker_written_frames: u64,
    worker_dropped_frames: u64,
    worker_fault: Option<&'static str>,
    channels: u32,
    sample_rate: u32,
    format: SampleFormat,
    calibration: Option<CalibrationCapture>,
}

struct CalibrationCapture {
    request: CalibrationRequest,
    first_sequence: u64,
    dropped_at_acceptance: u64,
    copied: u64,
    complete_windows: u64,
    sums: [f64; 2],
    peaks: [f32; 2],
    counts: [u64; 2],
    starts: [u64; 2],
    started: [bool; 2],
    invalid: bool,
}

impl IngressQueue {
    fn new(slot_count: u32, slot_frames: u32, channels: u32) -> Result<Self> {
        let samples_per_slot = usize::try_from(slot_frames)
            .ok()
            .and_then(|frames| frames.checked_mul(channels as usize))
            .ok_or(LambError::Validation(
                "capture queue slot sample count overflow".to_string(),
            ))?;
        let slots = ExactArray::try_from_fn(slot_count as usize, |_| {
            Ok(IngressSlot {
                storage: UnsafeCell::new(IngressSlotStorage {
                    frames: 0,
                    samples: MaterializedBuffer::new_zeroed(samples_per_slot)?,
                }),
            })
        })?;
        let queue = Self {
            slots,
            channels,
            slot_frames,
            published_sequence: AtomicU64::new(0),
            consumed_sequence: AtomicU64::new(0),
            enqueued_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            cumulative_dropped_frames: AtomicU64::new(0),
            drop_counter_exhausted: AtomicBool::new(false),
            producer_admission: AtomicU64::new(PRODUCER_OPEN),
            producer_generation: AtomicU64::new(0),
            producer_generation_exhausted: AtomicBool::new(false),
        };
        queue.materialize_pages()?;
        Ok(queue)
    }

    fn materialize_pages(&self) -> Result<()> {
        for slot in self.slots.iter() {
            let storage = unsafe { &mut *slot.storage.get() };
            storage.samples.materialize_pages()?;
        }
        Ok(())
    }

    fn published_head(&self) -> u64 {
        self.published_sequence.load(Ordering::Acquire)
    }

    fn consumed_head(&self) -> u64 {
        self.consumed_sequence.load(Ordering::Acquire)
    }

    fn try_push(&self, samples: &[f32], channels: u32) -> Result<CapturePushOutcome> {
        if self.drop_counter_exhausted.load(Ordering::Acquire) {
            return Err(LambError::CaptureInvariant(
                "cumulative capture dropped frame counter exhausted",
            ));
        }
        if channels != self.channels {
            return Err(LambError::CaptureInvariant(
                "incoming channels do not match capture ingress channels",
            ));
        }
        if channels == 0 || !samples.len().is_multiple_of(channels as usize) {
            return Err(LambError::CaptureInvariant(
                "input sample length is not whole frames",
            ));
        }
        let total_frames = u64::try_from(samples.len() / channels as usize)
            .map_err(|_| LambError::CaptureInvariant("input frame count overflow"))?;
        let mut source_frame = 0_u64;
        let mut enqueued_frames = 0_u64;

        while source_frame < total_frames {
            let published = self.published_sequence.load(Ordering::Relaxed);
            let consumed = self.consumed_sequence.load(Ordering::Acquire);
            let queued = published
                .checked_sub(consumed)
                .ok_or(LambError::CaptureInvariant(
                    "capture queue sequence invariant failed",
                ))?;
            if queued >= self.slots.len() as u64 {
                let dropped_frames = total_frames - source_frame;
                self.add_dropped_frames(dropped_frames)?;
                return Ok(CapturePushOutcome {
                    enqueued_frames,
                    dropped_frames,
                    published_sequence: published,
                });
            }

            let frames = (total_frames - source_frame).min(u64::from(self.slot_frames));
            let next_published = published.checked_add(1).ok_or(LambError::CaptureInvariant(
                "capture queue sequence exhausted",
            ))?;
            self.add_enqueued_frames(frames)?;
            let slot = &self.slots[(published % self.slots.len() as u64) as usize];
            let storage = unsafe { &mut *slot.storage.get() };
            let source_start = usize::try_from(source_frame)
                .ok()
                .and_then(|frame| frame.checked_mul(channels as usize))
                .ok_or(LambError::CaptureInvariant(
                    "capture queue source offset overflow",
                ))?;
            let sample_count = usize::try_from(frames)
                .ok()
                .and_then(|frames| frames.checked_mul(channels as usize))
                .ok_or(LambError::CaptureInvariant(
                    "capture queue sample count overflow",
                ))?;
            storage.samples[..sample_count]
                .copy_from_slice(&samples[source_start..source_start + sample_count]);
            storage.frames = frames as u32;
            self.published_sequence
                .store(next_published, Ordering::Release);
            source_frame += frames;
            enqueued_frames += frames;
        }

        Ok(CapturePushOutcome {
            enqueued_frames,
            dropped_frames: 0,
            published_sequence: self.published_head(),
        })
    }

    fn add_enqueued_frames(&self, frames: u64) -> Result<()> {
        checked_atomic_add(
            &self.enqueued_frames,
            frames,
            "capture ingress enqueued frame counter exhausted",
        )
    }

    fn add_dropped_frames(&self, frames: u64) -> Result<()> {
        self.add_cumulative_dropped_frames(frames)?;
        if let Err(error) = checked_atomic_add(
            &self.dropped_frames,
            frames,
            "capture ingress dropped frame counter exhausted",
        ) {
            self.drop_counter_exhausted.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    fn add_cumulative_dropped_frames(&self, frames: u64) -> Result<()> {
        if self.drop_counter_exhausted.load(Ordering::Acquire) {
            return Err(LambError::CaptureInvariant(
                "cumulative capture dropped frame counter exhausted",
            ));
        }
        if checked_atomic_add(
            &self.cumulative_dropped_frames,
            frames,
            "cumulative capture dropped frame counter exhausted",
        )
        .is_err()
        {
            self.drop_counter_exhausted.store(true, Ordering::Release);
            return Err(LambError::CaptureInvariant(
                "cumulative capture dropped frame counter exhausted",
            ));
        }
        Ok(())
    }

    fn close_producer_admission(&self) {
        self.producer_admission
            .fetch_and(PRODUCER_COUNT_MASK, Ordering::AcqRel);
    }

    fn admitted_producers(&self) -> u64 {
        self.producer_admission.load(Ordering::Acquire) & PRODUCER_COUNT_MASK
    }

    fn calibration_boundary(&self) -> Result<Option<(u64, u64)>> {
        if self.producer_generation_exhausted.load(Ordering::Acquire) {
            return Err(LambError::CaptureInvariant(
                "capture producer generation exhausted",
            ));
        }
        let generation = self.producer_generation.load(Ordering::Acquire);
        if self.admitted_producers() != 0 {
            return Ok(None);
        }
        let first_sequence = self.published_head();
        let dropped_at_acceptance = self.cumulative_dropped_frames.load(Ordering::Acquire);
        if self.admitted_producers() != 0
            || self.producer_generation.load(Ordering::Acquire) != generation
        {
            return Ok(None);
        }
        Ok(Some((first_sequence, dropped_at_acceptance)))
    }

    fn consume_next(&self, consume: impl FnOnce(&[f32], u64, u64)) -> bool {
        let consumed = self.consumed_sequence.load(Ordering::Relaxed);
        let published = self.published_sequence.load(Ordering::Acquire);
        if consumed >= published {
            return false;
        }
        let slot = &self.slots[(consumed % self.slots.len() as u64) as usize];
        let storage = unsafe { &*slot.storage.get() };
        let frames = u64::from(storage.frames);
        let sample_count = storage.frames as usize * self.channels as usize;
        consume(&storage.samples[..sample_count], frames, consumed);
        self.consumed_sequence
            .store(consumed + 1, Ordering::Release);
        true
    }
}

impl CaptureIngress {
    pub fn try_push_interleaved(
        &self,
        samples: &[f32],
        channels: u32,
    ) -> Result<CapturePushOutcome> {
        let _producer = ProducerAdmission::acquire(&self.queue)?;
        let outcome = self.queue.try_push(samples, channels)?;
        if outcome.enqueued_frames > 0 {
            self.wake.condvar.notify_one();
        }
        Ok(outcome)
    }

    pub fn published_head_sequence(&self) -> u64 {
        self.queue.published_head()
    }
}

struct ProducerAdmission<'a> {
    queue: &'a IngressQueue,
}

impl<'a> ProducerAdmission<'a> {
    fn acquire(queue: &'a IngressQueue) -> Result<Self> {
        let mut observed = queue.producer_admission.load(Ordering::Acquire);
        loop {
            if observed & PRODUCER_OPEN == 0 {
                return Err(LambError::CaptureInvariant("capture ingress is closed"));
            }
            let count = observed & PRODUCER_COUNT_MASK;
            if count == PRODUCER_COUNT_MASK {
                return Err(LambError::CaptureInvariant(
                    "capture producer count exhausted",
                ));
            }
            match queue.producer_admission.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self { queue }),
                Err(actual) => observed = actual,
            }
        }
    }
}

impl Drop for ProducerAdmission<'_> {
    fn drop(&mut self) {
        if self
            .queue
            .producer_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .is_err()
        {
            self.queue
                .producer_generation_exhausted
                .store(true, Ordering::Release);
        }
        let previous = self.queue.producer_admission.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & PRODUCER_COUNT_MASK > 0,
            "capture producer count underflow"
        );
    }
}

impl CommandSlot {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(COMMAND_IDLE),
            kind: AtomicU8::new(0),
            abandoned: AtomicBool::new(false),
            command: UnsafeCell::new(MaybeUninit::uninit()),
            result: UnsafeCell::new(MaybeUninit::uninit()),
            #[cfg(test)]
            submission_count: AtomicU64::new(0),
        }
    }

    fn begin(&self, kind: u8) -> Result<()> {
        self.state
            .compare_exchange(
                COMMAND_IDLE,
                COMMAND_PREPARING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| LambError::ControlInvariant("capture command already pending"))?;
        self.kind.store(kind, Ordering::Relaxed);
        self.abandoned.store(false, Ordering::Release);
        Ok(())
    }

    unsafe fn publish(&self, command: WorkerCommand) {
        unsafe { (*self.command.get()).write(command) };
        #[cfg(test)]
        self.submission_count.fetch_add(1, Ordering::AcqRel);
        self.state.store(COMMAND_READY, Ordering::Release);
    }

    fn ready_target(&self) -> Option<u64> {
        if self.state.load(Ordering::Acquire) != COMMAND_READY {
            return None;
        }
        let command = unsafe { (*self.command.get()).assume_init_ref() };
        Some(command.target_sequence())
    }

    fn take_command(&self) -> WorkerCommand {
        self.state
            .compare_exchange(
                COMMAND_READY,
                COMMAND_TAKING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("capture worker must take one ready command");
        let command = unsafe { (*self.command.get()).assume_init_read() };
        self.state.store(COMMAND_PROCESSING, Ordering::Release);
        command
    }

    unsafe fn publish_result(&self, result: Result<WorkerReply>) {
        unsafe { (*self.result.get()).write(result) };
        self.state.store(COMMAND_RESULT_READY, Ordering::Release);
    }

    fn take_result(&self) -> Option<Result<WorkerReply>> {
        if self
            .state
            .compare_exchange(
                COMMAND_RESULT_READY,
                COMMAND_RESULT_TAKING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return None;
        }
        let result = unsafe { (*self.result.get()).assume_init_read() };
        self.kind.store(0, Ordering::Relaxed);
        self.abandoned.store(false, Ordering::Relaxed);
        self.state.store(COMMAND_IDLE, Ordering::Release);
        Some(result)
    }

    fn cancel_preparing(&self) -> bool {
        self.state
            .compare_exchange(
                COMMAND_PREPARING,
                COMMAND_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

impl CalibrationSlot {
    fn new(sample_frames: u64, windows: u64, window_frames: u64, hop_frames: u64) -> Result<Self> {
        let sample_frames = usize::try_from(sample_frames).map_err(|_| {
            LambError::Validation("calibration sample capacity exceeds usize".to_string())
        })?;
        let windows = usize::try_from(windows).map_err(|_| {
            LambError::Validation("calibration window capacity exceeds usize".to_string())
        })?;
        Ok(Self {
            state: AtomicU8::new(CALIBRATION_IDLE),
            request: UnsafeCell::new(MaybeUninit::uninit()),
            metadata: UnsafeCell::new(CalibrationCaptureMetadata {
                frames: 0,
                sample_rate: 0,
                complete_windows: 0,
                partial_final_frames: 0,
                dropped_frames: 0,
                usable: false,
            }),
            samples: UnsafeCell::new(MaterializedBuffer::new_zeroed(sample_frames)?),
            rms: UnsafeCell::new(MaterializedBuffer::new_zeroed(windows)?),
            peak: UnsafeCell::new(MaterializedBuffer::new_zeroed(windows)?),
            window_frames,
            hop_frames,
            wait_mutex: Mutex::new(()),
            wait_condvar: Condvar::new(),
        })
    }

    fn begin(&self, request: CalibrationRequest) -> Result<()> {
        self.state
            .compare_exchange(
                CALIBRATION_IDLE,
                CALIBRATION_PREPARING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| LambError::ControlInvariant("calibration already pending"))?;
        unsafe { (*self.request.get()).write(request) };
        // The release store publishes the initialized request to the sole worker.
        self.state.store(CALIBRATION_READY, Ordering::Release);
        Ok(())
    }

    fn take_ready(&self) -> Option<CalibrationRequest> {
        self.state
            .compare_exchange(
                CALIBRATION_READY,
                CALIBRATION_CAPTURING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()?;
        Some(unsafe { (*self.request.get()).assume_init_read() })
    }

    fn samples_capacity(&self) -> u64 {
        unsafe { (&*self.samples.get()).len() as u64 }
    }

    fn notify_waiters(&self) {
        self.wait_condvar.notify_all();
    }
}

impl Drop for CommandSlot {
    fn drop(&mut self) {
        match self.state.load(Ordering::Acquire) {
            COMMAND_READY => unsafe { self.command.get_mut().assume_init_drop() },
            COMMAND_RESULT_READY => unsafe { self.result.get_mut().assume_init_drop() },
            _ => {}
        }
    }
}

impl CaptureArena {
    pub fn new(
        plan: &SessionMemoryPlan,
        runtime: CaptureRuntimeConfig,
    ) -> Result<(Self, CaptureIngress)> {
        let validation = runtime.clone();
        Self::allocate_validated(plan, &validation, || Self::allocate_runtime(plan, runtime))
    }

    fn allocate_validated<T>(
        plan: &SessionMemoryPlan,
        runtime: &CaptureRuntimeConfig,
        allocate: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        validate_runtime_plan(plan, runtime)?;
        allocate()
    }

    fn allocate_runtime(
        plan: &SessionMemoryPlan,
        runtime: CaptureRuntimeConfig,
    ) -> Result<(Self, CaptureIngress)> {
        let runtime_id = NEXT_CAPTURE_RUNTIME_ID
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| LambError::CaptureInvariant("capture runtime identity exhausted"))?;
        let config = runtime.ring;
        let calibration = Arc::new(CalibrationSlot::new(
            plan.calibration_sample_frames(),
            plan.calibration_complete_windows(),
            plan.calibration_window_frames(),
            plan.calibration_hop_frames(),
        )?);
        let queue = Arc::new(IngressQueue::new(
            runtime.queue_slots,
            runtime.slot_frames,
            config.channels,
        )?);
        let epochs = [
            Arc::new(SampleRing::new(config.clone())?),
            Arc::new(SampleRing::new(config.clone())?),
        ];
        for epoch in &epochs {
            epoch.materialize_pages()?;
        }
        let wake = Arc::new(WorkerWake {
            mutex: Mutex::new(()),
            condvar: Condvar::new(),
        });
        let shared = Arc::new(RuntimeShared {
            queue: Arc::clone(&queue),
            command: Arc::new(CommandSlot::new()),
            calibration: Arc::clone(&calibration),
            wake: Arc::clone(&wake),
            worker_done: AtomicBool::new(false),
            final_worker_written_frames: AtomicU64::new(0),
            final_worker_dropped_frames: AtomicU64::new(0),
            #[cfg(test)]
            shutdown_reply_pause: Mutex::new(None),
            #[cfg(test)]
            clear_reply_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_start_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_before_accept_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_after_consume_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_timeout_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_wait_pause: Mutex::new(None),
            #[cfg(test)]
            calibration_fault_after_start: AtomicBool::new(false),
        });
        let stack_size = usize::try_from(runtime.worker_stack_bytes).map_err(|_| {
            LambError::Validation("capture worker stack size exceeds usize".to_string())
        })?;
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("lamb-capture-worker".to_string())
            .stack_size(stack_size)
            .spawn(move || {
                let state = WorkerState {
                    shared: worker_shared,
                    epochs,
                    active: 0,
                    bases: [0, 0],
                    absolute_head: 0,
                    next_generation: 1,
                    frozen: None,
                    retired_dropped_frames: 0,
                    retired_last_overrun: None,
                    worker_written_frames: 0,
                    worker_dropped_frames: 0,
                    worker_fault: None,
                    channels: config.channels,
                    sample_rate: config.sample_rate,
                    format: config.format,
                    calibration: None,
                };
                state.run();
            })
            .map_err(|error| {
                LambError::Capture(format!("failed to start capture worker: {error}"))
            })?;

        let ingress = CaptureIngress {
            queue,
            wake,
            _single_producer: PhantomData,
        };
        Ok((
            Self {
                shared,
                command_client: Mutex::new(CommandClientState::default()),
                worker: Some(worker),
                runtime_id,
            },
            ingress,
        ))
    }

    pub fn runtime_id(&self) -> u64 {
        self.runtime_id
    }

    pub fn calibrate_channel(
        &self,
        request: CalibrationCaptureRequest,
        timeout: Duration,
    ) -> Result<CalibrationLease<'_>> {
        let deadline = command_deadline(timeout)?;
        if request.channel >= self.shared.queue.channels {
            return Err(LambError::Validation(
                "calibration channel is out of range".to_string(),
            ));
        }
        if request.frames == 0 || request.frames > self.shared.calibration.samples_capacity() {
            return Err(LambError::Validation(
                "calibration frame count is outside startup capacity".to_string(),
            ));
        }
        self.shared.calibration.begin(CalibrationRequest {
            channel: request.channel,
            frames: request.frames,
        })?;
        self.shared.wake.condvar.notify_one();
        loop {
            let state = self.shared.calibration.state.load(Ordering::Acquire);
            match state {
                CALIBRATION_COMPLETE => {
                    if self
                        .shared
                        .calibration
                        .state
                        .compare_exchange(
                            CALIBRATION_COMPLETE,
                            CALIBRATION_LEASED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(CalibrationLease {
                            slot: &self.shared.calibration,
                            metadata: unsafe { *self.shared.calibration.metadata.get() },
                        });
                    }
                }
                CALIBRATION_FAILED => {
                    if self
                        .shared
                        .calibration
                        .state
                        .compare_exchange(
                            CALIBRATION_FAILED,
                            CALIBRATION_IDLE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Err(LambError::CaptureInvariant(
                            "capture worker failed during calibration",
                        ));
                    }
                }
                CALIBRATION_IDLE => {
                    return Err(LambError::CaptureInvariant(
                        "calibration capture was cancelled",
                    ))
                }
                CALIBRATION_READY | CALIBRATION_CAPTURING if Instant::now() >= deadline => {
                    #[cfg(test)]
                    Self::run_calibration_pause(&self.shared.calibration_timeout_pause);
                    let _guard = self.shared.calibration.wait_mutex.lock().map_err(|_| {
                        LambError::ControlInvariant("calibration wait mutex poisoned")
                    })?;
                    let cancelled = match self.shared.calibration.state.load(Ordering::Acquire) {
                        CALIBRATION_READY => self
                            .shared
                            .calibration
                            .state
                            .compare_exchange(
                                CALIBRATION_READY,
                                CALIBRATION_IDLE,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok(),
                        CALIBRATION_CAPTURING => self
                            .shared
                            .calibration
                            .state
                            .compare_exchange(
                                CALIBRATION_CAPTURING,
                                CALIBRATION_CANCELLED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok(),
                        _ => false,
                    };
                    if cancelled {
                        self.shared.wake.condvar.notify_all();
                        return Err(LambError::ControlInvariant("calibration timed out"));
                    }
                }
                _ if self.worker_has_exited() => {
                    let _guard = self.shared.calibration.wait_mutex.lock().map_err(|_| {
                        LambError::ControlInvariant("calibration wait mutex poisoned")
                    })?;
                    match self.shared.calibration.state.load(Ordering::Acquire) {
                        CALIBRATION_READY | CALIBRATION_CAPTURING | CALIBRATION_CANCELLED => {
                            self.shared
                                .calibration
                                .state
                                .store(CALIBRATION_IDLE, Ordering::Release);
                            return Err(LambError::CaptureInvariant(
                                "capture worker exited during calibration",
                            ));
                        }
                        _ => {}
                    }
                }
                _ => {
                    #[cfg(test)]
                    Self::run_calibration_pause(&self.shared.calibration_wait_pause);
                    let guard = self.shared.calibration.wait_mutex.lock().map_err(|_| {
                        LambError::ControlInvariant("calibration wait mutex poisoned")
                    })?;
                    if self.shared.calibration.state.load(Ordering::Acquire) == state {
                        let _ = self.shared.calibration.wait_condvar.wait_timeout(
                            guard,
                            deadline.saturating_duration_since(Instant::now()),
                        );
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn run_calibration_pause(
        pause: &Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    ) {
        if let Some((entered, release)) = pause.lock().ok().and_then(|mut pause| pause.take()) {
            entered.wait();
            release.wait();
        }
    }

    pub fn freeze_since(
        &self,
        committed_start: Option<u64>,
        timeout: Duration,
    ) -> Result<Option<FrozenCaptureEpoch>> {
        let deadline = command_deadline(timeout)?;
        let mut client = self.lock_command_client(deadline)?;
        let result = if let Some(result) =
            self.recover_before(&mut client, COMMAND_KIND_FREEZE, deadline)?
        {
            result
        } else {
            self.submit_locked(
                COMMAND_KIND_FREEZE,
                |target_sequence| WorkerCommand::Freeze {
                    target_sequence,
                    committed_start,
                },
                deadline,
            )
        };
        let reply = result?;
        match reply {
            WorkerReply::Frozen(descriptor) => {
                Ok(descriptor.map(|descriptor| self.frozen_epoch_from_descriptor(descriptor)))
            }
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong freeze result",
            )),
        }
    }

    pub fn release_frozen(&self, frozen: &mut FrozenCaptureEpoch, timeout: Duration) -> Result<()> {
        if !Arc::ptr_eq(&self.shared, &frozen.runtime) {
            return Err(LambError::ControlInvariant(
                "frozen epoch belongs to a different capture runtime",
            ));
        }
        if frozen.released {
            return Err(LambError::ControlInvariant(
                "frozen epoch was already released",
            ));
        }
        let deadline = command_deadline(timeout)?;
        let mut client = self.lock_command_client(deadline)?;
        let result = if let Some(result) =
            self.recover_before(&mut client, COMMAND_KIND_RELEASE, deadline)?
        {
            result
        } else {
            self.submit_locked(
                COMMAND_KIND_RELEASE,
                |target_sequence| WorkerCommand::Release {
                    target_sequence,
                    index: frozen.index,
                    generation: frozen.generation,
                },
                deadline,
            )
        };
        let reply = match result {
            Ok(reply) => {
                frozen.release_pending = false;
                reply
            }
            Err(error) => {
                frozen.release_pending =
                    self.shared.command.state.load(Ordering::Acquire) != COMMAND_IDLE;
                return Err(error);
            }
        };
        match reply {
            WorkerReply::Released => {
                frozen.released = true;
                Ok(())
            }
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong release result",
            )),
        }
    }

    pub fn clear_active(&self, timeout: Duration) -> Result<CaptureClearReport> {
        self.clear_active_accounted(
            CaptureClearAccounting {
                arena_runtime_id: self.runtime_id,
                coordinator_id: 0,
                clear_id: 0,
                expected_active_start: 0,
                pending_retention_lost_frames: 0,
                pending_cleared_frames: 0,
            },
            timeout,
        )
    }

    pub fn clear_active_accounted(
        &self,
        accounting: CaptureClearAccounting,
        timeout: Duration,
    ) -> Result<CaptureClearReport> {
        if accounting.arena_runtime_id != self.runtime_id {
            return Err(LambError::ControlInvariant(
                "clear accounting belongs to a different capture runtime",
            ));
        }
        match self.execute_command(
            COMMAND_KIND_CLEAR,
            |target_sequence| WorkerCommand::Clear {
                target_sequence,
                ingress_dropped_frames: self.shared.queue.dropped_frames.load(Ordering::Acquire),
                accounting,
            },
            timeout,
        )? {
            WorkerReply::Cleared(report) => Ok(report),
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong clear result",
            )),
        }
    }

    pub fn cumulative_capture_dropped_frames(&self) -> u64 {
        self.shared
            .queue
            .cumulative_dropped_frames
            .load(Ordering::Acquire)
    }

    pub fn recover_clear_result(
        &self,
        recovery: CaptureClearRecovery,
        timeout: Duration,
    ) -> Result<Option<CaptureClearReport>> {
        if recovery.arena_runtime_id != self.runtime_id {
            return Err(LambError::ControlInvariant(
                "clear recovery belongs to a different capture runtime",
            ));
        }
        let deadline = command_deadline(timeout)?;
        let mut client = self.lock_command_client(deadline)?;
        if let Some(report) = client.recovered_clear.as_ref() {
            if !recovery.matches(report) {
                return Err(LambError::ControlInvariant(
                    "clear recovery identity does not match retained report",
                ));
            }
            return Ok(client.recovered_clear.take());
        }
        if self.shared.command.state.load(Ordering::Acquire) == COMMAND_IDLE {
            return Ok(None);
        }
        if !self.shared.command.abandoned.load(Ordering::Acquire)
            || self.shared.command.kind.load(Ordering::Acquire) != COMMAND_KIND_CLEAR
        {
            return Err(LambError::ControlInvariant(
                "capture clear result is not recoverable",
            ));
        }
        match self.wait_slot_result(deadline)?? {
            WorkerReply::Cleared(report) if recovery.matches(&report) => Ok(Some(report)),
            WorkerReply::Cleared(report) => {
                client.recovered_clear = Some(report);
                Err(LambError::ControlInvariant(
                    "clear recovery identity does not match completed report",
                ))
            }
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong clear recovery result",
            )),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_clear_reply_pause_for_test(
        &self,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    ) {
        *self.shared.clear_reply_pause.lock().unwrap() = Some((entered, release));
    }

    pub fn status(&self, timeout: Duration) -> Result<CaptureArenaStatus> {
        match self.execute_command(
            COMMAND_KIND_STATUS,
            |target_sequence| WorkerCommand::Status { target_sequence },
            timeout,
        )? {
            WorkerReply::Status(status) => Ok(status),
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong status result",
            )),
        }
    }

    pub fn active_absolute_range(&self, timeout: Duration) -> Result<Range<u64>> {
        Ok(self.status(timeout)?.active_absolute_range)
    }

    pub fn shutdown(&mut self, timeout: Duration) -> Result<()> {
        if self.worker.is_none() {
            if let Ok(mut client) = self.command_client.try_lock() {
                client.shutdown = CommandClientShutdown::Joined;
            }
            return Ok(());
        }
        let deadline = command_deadline(timeout)?;
        let mut client = self.lock_command_client(deadline)?;
        self.shared.queue.close_producer_admission();
        self.wait_producers_idle(deadline)?;
        if client.shutdown == CommandClientShutdown::NotRequested {
            let exited = self.worker_has_exited();
            let recovered = if exited
                && self.shared.command.kind.load(Ordering::Acquire) == COMMAND_KIND_SHUTDOWN
            {
                self.shared.command.take_result()
            } else {
                self.recover_before(&mut client, COMMAND_KIND_SHUTDOWN, deadline)?
            };
            if let Some(result) = recovered {
                Self::acknowledge_shutdown(&mut client, result)?;
            } else if self.shared.worker_done.load(Ordering::Acquire) {
                client.shutdown = CommandClientShutdown::Acknowledged;
            } else if !self.worker_has_exited() {
                let result = self.submit_locked(
                    COMMAND_KIND_SHUTDOWN,
                    |target_sequence| WorkerCommand::Shutdown { target_sequence },
                    deadline,
                );
                Self::acknowledge_shutdown(&mut client, result)?;
            }
        }
        if client.shutdown == CommandClientShutdown::Acknowledged && !self.worker_has_exited() {
            self.wait_worker_done(deadline)?;
        }
        drop(client);
        if let Some(worker) = self.worker.take() {
            let joined = worker.join();
            self.classify_unconsumed_after_worker();
            joined.map_err(|_| LambError::ControlInvariant("capture worker panicked"))?;
        }
        let mut client = self.command_client.try_lock().map_err(|_| {
            LambError::ControlInvariant("capture command client unavailable after join")
        })?;
        client.shutdown = CommandClientShutdown::Joined;
        Ok(())
    }

    fn acknowledge_shutdown(
        client: &mut CommandClientState,
        result: Result<WorkerReply>,
    ) -> Result<()> {
        match result? {
            WorkerReply::Shutdown => {
                client.shutdown = CommandClientShutdown::Acknowledged;
                Ok(())
            }
            _ => Err(LambError::ControlInvariant(
                "capture worker returned the wrong shutdown result",
            )),
        }
    }

    fn worker_has_exited(&self) -> bool {
        self.shared.worker_done.load(Ordering::Acquire)
            || self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn execute_command(
        &self,
        kind: u8,
        build: impl FnOnce(u64) -> WorkerCommand,
        timeout: Duration,
    ) -> Result<WorkerReply> {
        let deadline = command_deadline(timeout)?;
        let mut client = self.lock_command_client(deadline)?;
        if let Some(result) = self.recover_before(&mut client, kind, deadline)? {
            return result;
        }
        self.submit_locked(kind, build, deadline)
    }

    fn submit_locked(
        &self,
        kind: u8,
        build: impl FnOnce(u64) -> WorkerCommand,
        deadline: Instant,
    ) -> Result<WorkerReply> {
        self.shared.command.begin(kind)?;
        let target_sequence = self.shared.queue.published_head();
        unsafe { self.shared.command.publish(build(target_sequence)) };
        self.shared.wake.condvar.notify_one();
        self.wait_slot_result(deadline)?
    }

    fn recover_before(
        &self,
        client: &mut CommandClientState,
        requested_kind: u8,
        deadline: Instant,
    ) -> Result<Option<Result<WorkerReply>>> {
        if client.recovered_clear.is_some() && requested_kind != COMMAND_KIND_STATUS {
            return Err(LambError::ControlInvariant(
                "clear result pending explicit recovery",
            ));
        }
        if client.recovered_frozen.is_some() {
            if requested_kind == COMMAND_KIND_FREEZE {
                return Ok(Some(Ok(WorkerReply::Frozen(
                    client.recovered_frozen.take(),
                ))));
            }
            return Err(LambError::ControlInvariant(
                "frozen result pending recovery",
            ));
        }
        if self.shared.command.state.load(Ordering::Acquire) == COMMAND_IDLE {
            return Ok(None);
        }
        if !self.shared.command.abandoned.load(Ordering::Acquire) {
            return Err(LambError::ControlInvariant(
                "capture command already pending",
            ));
        }
        let stale_kind = self.shared.command.kind.load(Ordering::Acquire);
        if stale_kind == COMMAND_KIND_RELEASE && requested_kind != COMMAND_KIND_RELEASE {
            return Err(LambError::ControlInvariant(
                "release result pending recovery",
            ));
        }
        let stale_result = self.wait_slot_result(deadline)?;
        if stale_kind == COMMAND_KIND_CLEAR {
            if let Ok(WorkerReply::Cleared(report)) = stale_result {
                client.recovered_clear = Some(report);
                return if requested_kind == COMMAND_KIND_STATUS {
                    Ok(None)
                } else {
                    Err(LambError::ControlInvariant(
                        "clear result pending explicit recovery",
                    ))
                };
            }
            return if requested_kind == COMMAND_KIND_CLEAR {
                Ok(Some(stale_result))
            } else {
                Ok(None)
            };
        }
        if stale_kind == COMMAND_KIND_FREEZE {
            if let Ok(WorkerReply::Frozen(Some(descriptor))) = stale_result {
                if requested_kind == COMMAND_KIND_FREEZE {
                    return Ok(Some(Ok(WorkerReply::Frozen(Some(descriptor)))));
                }
                client.recovered_frozen = Some(descriptor);
                return Err(LambError::ControlInvariant(
                    "frozen result pending recovery",
                ));
            }
            return if requested_kind == COMMAND_KIND_FREEZE {
                Ok(Some(stale_result))
            } else {
                Ok(None)
            };
        }
        if stale_kind == COMMAND_KIND_RELEASE {
            return Ok(Some(stale_result));
        }
        if stale_kind == requested_kind {
            return Ok(Some(stale_result));
        }
        if stale_kind == COMMAND_KIND_SHUTDOWN {
            return Err(LambError::ControlInvariant(
                "shutdown result pending recovery",
            ));
        }
        Ok(None)
    }

    #[cfg(test)]
    fn wait_result(&self, timeout: Duration) -> Result<WorkerReply> {
        let deadline = match Instant::now().checked_add(timeout) {
            Some(deadline) => deadline,
            None => {
                self.shared.command.abandoned.store(true, Ordering::Release);
                return Err(LambError::ControlInvariant(
                    "capture command deadline overflow",
                ));
            }
        };
        self.wait_slot_result(deadline)?
    }

    fn wait_slot_result(&self, deadline: Instant) -> Result<Result<WorkerReply>> {
        loop {
            if let Some(result) = self.shared.command.take_result() {
                return Ok(result);
            }
            if Instant::now() >= deadline {
                self.shared.command.abandoned.store(true, Ordering::Release);
                return Err(LambError::ControlInvariant("capture command timed out"));
            }
            thread::yield_now();
        }
    }

    fn lock_command_client(&self, deadline: Instant) -> Result<MutexGuard<'_, CommandClientState>> {
        loop {
            match self.command_client.try_lock() {
                Ok(client) => return Ok(client),
                Err(TryLockError::Poisoned(_)) => {
                    return Err(LambError::ControlInvariant(
                        "capture command client mutex poisoned",
                    ))
                }
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::yield_now(),
                Err(TryLockError::WouldBlock) => {
                    return Err(LambError::ControlInvariant(
                        "capture command client timed out",
                    ))
                }
            }
        }
    }

    fn frozen_epoch_from_descriptor(&self, descriptor: FrozenDescriptor) -> FrozenCaptureEpoch {
        FrozenCaptureEpoch {
            runtime: Arc::clone(&self.shared),
            ring: descriptor.ring,
            runtime_id: self.runtime_id,
            index: descriptor.index,
            generation: descriptor.generation,
            base: descriptor.base,
            absolute_range: descriptor.absolute_range,
            local_range: descriptor.local_range,
            channels: descriptor.channels,
            sample_rate: descriptor.sample_rate,
            format: descriptor.format,
            released: false,
            release_pending: false,
        }
    }

    fn wait_worker_done(&self, deadline: Instant) -> Result<()> {
        while !self.shared.worker_done.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return Err(LambError::ControlInvariant("capture shutdown timed out"));
            }
            thread::yield_now();
        }
        Ok(())
    }

    fn wait_producers_idle(&self, deadline: Instant) -> Result<()> {
        while self.shared.queue.admitted_producers() != 0 {
            if Instant::now() >= deadline {
                return Err(LambError::ControlInvariant(
                    "capture producer shutdown timed out",
                ));
            }
            thread::yield_now();
        }
        Ok(())
    }

    fn shutdown_for_drop(&mut self) {
        if self.worker.is_none() {
            return;
        }
        self.shared.queue.close_producer_admission();
        while self.shared.queue.admitted_producers() != 0 {
            thread::yield_now();
        }
        let mut client = match self.command_client.lock() {
            Ok(client) => client,
            Err(poisoned) => poisoned.into_inner(),
        };
        if client.shutdown == CommandClientShutdown::NotRequested {
            loop {
                match self.shared.command.state.load(Ordering::Acquire) {
                    COMMAND_IDLE => break,
                    COMMAND_PREPARING => {
                        self.shared.command.cancel_preparing();
                    }
                    COMMAND_RESULT_READY => {
                        if let Some(result) = self.shared.command.take_result() {
                            match result {
                                Ok(WorkerReply::Frozen(Some(descriptor))) => {
                                    client.recovered_frozen = Some(descriptor);
                                }
                                Ok(WorkerReply::Shutdown) => {
                                    client.shutdown = CommandClientShutdown::Acknowledged;
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {
                        if self.worker_has_exited() {
                            break;
                        }
                        self.shared.wake.condvar.notify_one();
                        thread::yield_now();
                    }
                }
            }
            if client.shutdown == CommandClientShutdown::NotRequested
                && self.shared.worker_done.load(Ordering::Acquire)
            {
                client.shutdown = CommandClientShutdown::Acknowledged;
            }
            if client.shutdown == CommandClientShutdown::NotRequested
                && !self.worker_has_exited()
                && self.shared.command.begin(COMMAND_KIND_SHUTDOWN).is_ok()
            {
                let target_sequence = self.shared.queue.published_head();
                unsafe {
                    self.shared
                        .command
                        .publish(WorkerCommand::Shutdown { target_sequence });
                }
                self.shared.wake.condvar.notify_one();
                while !self.worker_has_exited() {
                    if let Some(result) = self.shared.command.take_result() {
                        let _ = Self::acknowledge_shutdown(&mut client, result);
                        break;
                    }
                    thread::yield_now();
                }
            }
        }
        while client.shutdown == CommandClientShutdown::Acknowledged && !self.worker_has_exited() {
            thread::yield_now();
        }
        drop(client);
        let joined = if let Some(worker) = self.worker.take() {
            worker.join().is_ok()
        } else {
            true
        };
        self.classify_unconsumed_after_worker();
        if joined {
            let mut client = match self.command_client.lock() {
                Ok(client) => client,
                Err(poisoned) => poisoned.into_inner(),
            };
            client.shutdown = CommandClientShutdown::Joined;
        }
    }

    fn classify_unconsumed_after_worker(&self) {
        while self.shared.queue.consume_next(|_, frames, _| {
            let _ = self.shared.queue.add_dropped_frames(frames);
        }) {}
    }
}

impl Drop for CaptureArena {
    fn drop(&mut self) {
        self.shutdown_for_drop();
    }
}

impl FrozenCaptureEpoch {
    pub(crate) fn identity(&self) -> FrozenEpochIdentity {
        FrozenEpochIdentity {
            runtime_id: self.runtime_id,
            index: self.index,
            generation: self.generation,
        }
    }

    pub fn absolute_range(&self) -> Range<u64> {
        self.absolute_range.clone()
    }

    pub fn local_range(&self) -> Range<u64> {
        self.local_range.clone()
    }

    pub fn total_frames(&self) -> u64 {
        self.absolute_range.end - self.absolute_range.start
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

    pub fn copy_interleaved_range_into(
        &self,
        absolute_range: Range<u64>,
        destination: &mut [f32],
    ) -> Result<u64> {
        if self.released {
            return Err(LambError::ExportInvariant(
                "frozen capture epoch was released",
            ));
        }
        if self.release_pending {
            return Err(LambError::ExportInvariant(
                "frozen capture release is pending",
            ));
        }
        if absolute_range.start < self.absolute_range.start
            || absolute_range.end > self.absolute_range.end
            || absolute_range.start > absolute_range.end
        {
            return Err(LambError::ExportInvariant(
                "requested absolute range is outside frozen range",
            ));
        }
        let local_range = (absolute_range.start - self.base)..(absolute_range.end - self.base);
        self.ring
            .copy_interleaved_range_into(local_range, destination)
    }
}

impl WorkerState {
    fn run(mut self) {
        loop {
            self.cancel_calibration_if_requested();
            let command_state = self.shared.command.state.load(Ordering::Acquire);
            if command_state == COMMAND_PREPARING {
                self.wait_for_work();
                continue;
            }
            if let Some(target) = self.shared.command.ready_target() {
                if self.shared.queue.consumed_head() < target {
                    if self.consume_one() {
                        continue;
                    }
                } else {
                    let command = self.shared.command.take_command();
                    let shutdown = matches!(command, WorkerCommand::Shutdown { .. });
                    #[cfg(test)]
                    let clear = matches!(command, WorkerCommand::Clear { .. });
                    let result = self.process_command(command);
                    #[cfg(test)]
                    if clear {
                        if let Some((entered, release)) = self
                            .shared
                            .clear_reply_pause
                            .lock()
                            .ok()
                            .and_then(|mut pause| pause.take())
                        {
                            entered.wait();
                            release.wait();
                        }
                    }
                    unsafe { self.shared.command.publish_result(result) };
                    self.shared.wake.condvar.notify_all();
                    if shutdown {
                        #[cfg(test)]
                        if let Some((entered, release)) = self
                            .shared
                            .shutdown_reply_pause
                            .lock()
                            .ok()
                            .and_then(|pause| pause.clone())
                        {
                            entered.wait();
                            release.wait();
                        }
                        break;
                    }
                    continue;
                }
            } else if self.consume_one() {
                continue;
            }
            self.wait_for_work();
        }
        self.fail_calibration();
        self.shared.worker_done.store(true, Ordering::Release);
        self.shared.wake.condvar.notify_all();
    }

    fn wait_for_work(&self) {
        let Ok(guard) = self.shared.wake.mutex.lock() else {
            return;
        };
        if self.shared.command.state.load(Ordering::Acquire) != COMMAND_IDLE
            || self.shared.queue.consumed_head() < self.shared.queue.published_head()
        {
            return;
        }
        let _ = self
            .shared
            .wake
            .condvar
            .wait_timeout(guard, WORKER_IDLE_WAIT);
    }

    fn consume_one(&mut self) -> bool {
        self.start_calibration_if_ready();
        let mut consumed = false;
        let queue = Arc::clone(&self.shared.queue);
        queue.consume_next(|samples, frames, sequence| {
            consumed = true;
            self.consume_samples(samples, frames, sequence);
        });
        #[cfg(test)]
        if consumed {
            CaptureArena::run_calibration_pause(&self.shared.calibration_after_consume_pause);
        }
        consumed
    }

    fn consume_samples(&mut self, samples: &[f32], frames: u64, sequence: u64) {
        if self.worker_fault.is_some() {
            self.fail_calibration();
            let _ = self.shared.queue.add_dropped_frames(frames);
            return;
        }
        let Some(next_absolute) = self.absolute_head.checked_add(frames) else {
            self.drop_worker_frames(frames);
            return;
        };
        let Some(next_written) = self.worker_written_frames.checked_add(frames) else {
            self.drop_worker_frames(frames);
            return;
        };
        if self.epochs[self.active]
            .write_interleaved(samples, self.channels)
            .is_err()
        {
            self.drop_worker_frames(frames);
            return;
        }
        self.absolute_head = next_absolute;
        self.worker_written_frames = next_written;
        self.copy_calibration_samples(samples, frames, sequence);
    }

    fn start_calibration_if_ready(&mut self) {
        #[cfg(test)]
        if self.shared.calibration.state.load(Ordering::Acquire) == CALIBRATION_READY {
            CaptureArena::run_calibration_pause(&self.shared.calibration_before_accept_pause);
        }
        let boundary = match self.shared.queue.calibration_boundary() {
            Ok(Some(boundary)) => boundary,
            Ok(None) => return,
            Err(_) => {
                self.set_worker_fault("capture producer generation exhausted");
                return;
            }
        };
        let Some(request) = self.shared.calibration.take_ready() else {
            return;
        };
        #[cfg(test)]
        CaptureArena::run_calibration_pause(&self.shared.calibration_start_pause);
        if self.shared.calibration.state.load(Ordering::Acquire) != CALIBRATION_CAPTURING {
            self.cancel_calibration_if_requested();
            return;
        }
        self.calibration = Some(CalibrationCapture {
            request,
            first_sequence: boundary.0,
            dropped_at_acceptance: boundary.1,
            copied: 0,
            complete_windows: 0,
            sums: [0.0; 2],
            peaks: [0.0; 2],
            counts: [0; 2],
            starts: [0; 2],
            started: [false; 2],
            invalid: false,
        });
        #[cfg(test)]
        if self
            .shared
            .calibration_fault_after_start
            .swap(false, Ordering::AcqRel)
        {
            self.set_worker_fault("injected calibration worker fault");
        }
    }

    fn cancel_calibration_if_requested(&mut self) {
        if self.shared.calibration.state.load(Ordering::Acquire) == CALIBRATION_CANCELLED {
            self.calibration = None;
            let _guard = self
                .shared
                .calibration
                .wait_mutex
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = self.shared.calibration.state.compare_exchange(
                CALIBRATION_CANCELLED,
                CALIBRATION_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.shared.calibration.notify_waiters();
        }
    }

    fn fail_calibration(&mut self) {
        self.calibration = None;
        let _guard = self
            .shared
            .calibration
            .wait_mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            let state = self.shared.calibration.state.load(Ordering::Acquire);
            let terminal = match state {
                CALIBRATION_READY | CALIBRATION_CAPTURING => CALIBRATION_FAILED,
                CALIBRATION_CANCELLED => CALIBRATION_IDLE,
                _ => break,
            };
            if self
                .shared
                .calibration
                .state
                .compare_exchange(state, terminal, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        self.shared.calibration.notify_waiters();
    }

    fn set_worker_fault(&mut self, fault: &'static str) {
        self.worker_fault = Some(fault);
        self.fail_calibration();
    }

    fn copy_calibration_samples(&mut self, samples: &[f32], frames: u64, sequence: u64) {
        let Some(capture) = self.calibration.as_mut() else {
            return;
        };
        if self.shared.calibration.state.load(Ordering::Acquire) == CALIBRATION_CANCELLED {
            self.calibration = None;
            let _guard = self
                .shared
                .calibration
                .wait_mutex
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = self.shared.calibration.state.compare_exchange(
                CALIBRATION_CANCELLED,
                CALIBRATION_IDLE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.shared.calibration.notify_waiters();
            return;
        }
        if sequence < capture.first_sequence {
            return;
        }
        let channel = capture.request.channel as usize;
        for frame in 0..frames {
            if capture.copied == capture.request.frames {
                break;
            }
            let sample = samples[frame as usize * self.channels as usize + channel];
            unsafe {
                (*self.shared.calibration.samples.get()).as_mut_slice()[capture.copied as usize] =
                    sample;
            }
            if !sample.is_finite() {
                capture.invalid = true;
            }
            let index = capture.copied;
            let hop = self.shared.calibration.hop_frames;
            if index % hop == 0 {
                let accumulator = ((index / hop) % 2) as usize;
                capture.sums[accumulator] = 0.0;
                capture.peaks[accumulator] = 0.0;
                capture.counts[accumulator] = 0;
                capture.starts[accumulator] = index;
                capture.started[accumulator] = true;
            }
            for accumulator in 0..2 {
                if capture.started[accumulator]
                    && capture.counts[accumulator] < self.shared.calibration.window_frames
                {
                    capture.sums[accumulator] += f64::from(sample) * f64::from(sample);
                    capture.peaks[accumulator] = capture.peaks[accumulator].max(sample.abs());
                    capture.counts[accumulator] += 1;
                    if capture.counts[accumulator] == self.shared.calibration.window_frames {
                        let output = capture.complete_windows as usize;
                        unsafe {
                            (*self.shared.calibration.rms.get()).as_mut_slice()[output] =
                                (capture.sums[accumulator] / capture.counts[accumulator] as f64)
                                    .sqrt() as f32;
                            (*self.shared.calibration.peak.get()).as_mut_slice()[output] =
                                capture.peaks[accumulator];
                        }
                        capture.complete_windows += 1;
                        capture.started[accumulator] = false;
                    }
                }
            }
            capture.copied += 1;
        }
        if capture.copied == capture.request.frames {
            self.finish_calibration();
        }
    }

    fn finish_calibration(&mut self) {
        let capture = self.calibration.take().expect("calibration must be active");
        let dropped = self
            .shared
            .queue
            .cumulative_dropped_frames
            .load(Ordering::Acquire)
            .saturating_sub(capture.dropped_at_acceptance);
        let partial = (0..capture.counts.len())
            .filter(|accumulator| {
                capture.started[*accumulator]
                    && capture.counts[*accumulator] > 0
                    && capture.counts[*accumulator] < self.shared.calibration.window_frames
            })
            .max_by_key(|accumulator| capture.starts[*accumulator])
            .map(|accumulator| capture.counts[accumulator])
            .unwrap_or(0);
        unsafe {
            *self.shared.calibration.metadata.get() = CalibrationCaptureMetadata {
                frames: capture.copied,
                sample_rate: self.sample_rate,
                complete_windows: capture.complete_windows,
                partial_final_frames: partial,
                dropped_frames: dropped,
                usable: !capture.invalid && dropped == 0 && capture.complete_windows > 0,
            };
        }
        let _guard = self
            .shared
            .calibration
            .wait_mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.shared.calibration.state.compare_exchange(
            CALIBRATION_CAPTURING,
            CALIBRATION_COMPLETE,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(CALIBRATION_CANCELLED) => {
                let _ = self.shared.calibration.state.compare_exchange(
                    CALIBRATION_CANCELLED,
                    CALIBRATION_IDLE,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            Err(_) => {}
        }
        self.shared.calibration.notify_waiters();
    }

    fn drop_worker_frames(&mut self, frames: u64) {
        if self
            .shared
            .queue
            .add_cumulative_dropped_frames(frames)
            .is_err()
        {
            self.set_worker_fault("capture worker drop counter exhausted");
            return;
        }
        let Some(total) = self.worker_dropped_frames.checked_add(frames) else {
            self.set_worker_fault("capture worker drop counter exhausted");
            return;
        };
        self.worker_dropped_frames = total;
        self.shared
            .final_worker_dropped_frames
            .store(total, Ordering::Release);
    }

    fn process_command(&mut self, command: WorkerCommand) -> Result<WorkerReply> {
        if self
            .shared
            .queue
            .drop_counter_exhausted
            .load(Ordering::Acquire)
            && !matches!(
                &command,
                WorkerCommand::Release { .. } | WorkerCommand::Shutdown { .. }
            )
        {
            return Err(LambError::CaptureInvariant(
                "cumulative capture dropped frame counter exhausted",
            ));
        }
        match command {
            WorkerCommand::Freeze {
                committed_start, ..
            } => self.freeze(committed_start).map(WorkerReply::Frozen),
            WorkerCommand::Release {
                index, generation, ..
            } => {
                self.release(index, generation)?;
                Ok(WorkerReply::Released)
            }
            WorkerCommand::Clear {
                ingress_dropped_frames,
                accounting,
                ..
            } => {
                let active_absolute_range = self.active_absolute_range()?;
                let retention_lost_frames = active_absolute_range
                    .start
                    .saturating_sub(accounting.expected_active_start);
                let recoverable_start = accounting
                    .expected_active_start
                    .max(active_absolute_range.start);
                let cleared_frames = active_absolute_range.end.saturating_sub(recoverable_start);
                let pending_retention_lost_frames = accounting
                    .pending_retention_lost_frames
                    .checked_add(retention_lost_frames)
                    .ok_or(LambError::ControlInvariant(
                        "pending retention loss counter exhausted",
                    ))?;
                let pending_cleared_frames = accounting
                    .pending_cleared_frames
                    .checked_add(cleared_frames)
                    .ok_or(LambError::ControlInvariant(
                        "pending cleared frame counter exhausted",
                    ))?;
                let cumulative_dropped_frames = ingress_dropped_frames
                    .checked_add(self.worker_dropped_frames)
                    .ok_or(LambError::ControlInvariant(
                        "cumulative capture dropped frame counter exhausted",
                    ))?;
                self.epochs[self.active].clear()?;
                Ok(WorkerReply::Cleared(CaptureClearReport {
                    arena_runtime_id: accounting.arena_runtime_id,
                    coordinator_id: accounting.coordinator_id,
                    clear_id: accounting.clear_id,
                    expected_active_start: accounting.expected_active_start,
                    active_absolute_range,
                    pending_retention_lost_frames,
                    pending_cleared_frames,
                    cumulative_dropped_frames,
                }))
            }
            WorkerCommand::Status { .. } => self.status().map(WorkerReply::Status),
            WorkerCommand::Shutdown { .. } => Ok(WorkerReply::Shutdown),
            #[cfg(test)]
            WorkerCommand::DropProbe(_) => unreachable!("test drop command must not reach worker"),
        }
    }

    fn freeze(&mut self, committed_start: Option<u64>) -> Result<Option<FrozenDescriptor>> {
        if self.frozen.is_some() {
            return Err(LambError::ControlInvariant(
                "a frozen capture epoch is already pending",
            ));
        }
        let old = self.active;
        let standby = 1 - old;
        let old_base = self.bases[old];
        let local_end = self.epochs[old].write_head_frame();
        let local_oldest = self.epochs[old].oldest_frame();
        let absolute_end = old_base
            .checked_add(local_end)
            .ok_or(LambError::ControlInvariant(
                "absolute capture frame counter exhausted",
            ))?;
        let absolute_oldest =
            old_base
                .checked_add(local_oldest)
                .ok_or(LambError::ControlInvariant(
                    "absolute capture frame counter exhausted",
                ))?;
        let absolute_start = committed_start
            .unwrap_or(absolute_oldest)
            .max(absolute_oldest);
        let generation = if absolute_start < absolute_end {
            Some(
                self.next_generation
                    .checked_add(1)
                    .ok_or(LambError::ControlInvariant(
                        "frozen epoch generation exhausted",
                    ))?,
            )
        } else {
            None
        };

        self.retire_and_reset(standby)?;
        self.bases[standby] = self.absolute_head;
        self.active = standby;

        let Some(generation) = generation else {
            return Ok(None);
        };
        self.next_generation = generation;
        self.frozen = Some(FrozenRecord {
            index: old,
            generation,
        });
        Ok(Some(FrozenDescriptor {
            ring: Arc::clone(&self.epochs[old]),
            index: old,
            generation,
            base: old_base,
            absolute_range: absolute_start..absolute_end,
            local_range: (absolute_start - old_base)..local_end,
            channels: self.channels,
            sample_rate: self.sample_rate,
            format: self.format,
        }))
    }

    fn release(&mut self, index: usize, generation: u64) -> Result<()> {
        let frozen = self.frozen.as_ref().ok_or(LambError::ControlInvariant(
            "no frozen capture epoch pending",
        ))?;
        if frozen.index != index || frozen.generation != generation {
            return Err(LambError::ControlInvariant(
                "frozen capture capability does not match worker state",
            ));
        }
        let status = self.epochs[index].status();
        let retired = self
            .retired_dropped_frames
            .checked_add(status.dropped_frames)
            .ok_or(LambError::ControlInvariant(
                "cumulative dropped frame counter exhausted",
            ))?;
        self.epochs[index].reset()?;
        self.retired_dropped_frames = retired;
        self.retired_last_overrun = latest_overrun(self.retired_last_overrun, status.last_overrun);
        self.frozen = None;
        Ok(())
    }

    fn active_absolute_range(&self) -> Result<Range<u64>> {
        let start = self.bases[self.active]
            .checked_add(self.epochs[self.active].oldest_frame())
            .ok_or(LambError::ControlInvariant(
                "absolute capture frame counter exhausted",
            ))?;
        let end = self.bases[self.active]
            .checked_add(self.epochs[self.active].write_head_frame())
            .ok_or(LambError::ControlInvariant(
                "absolute capture frame counter exhausted",
            ))?;
        Ok(start..end)
    }

    fn retire_and_reset(&mut self, index: usize) -> Result<()> {
        let status = self.epochs[index].status();
        let retired = self
            .retired_dropped_frames
            .checked_add(status.dropped_frames)
            .ok_or(LambError::ControlInvariant(
                "cumulative dropped frame counter exhausted",
            ))?;
        self.epochs[index].reset()?;
        self.retired_dropped_frames = retired;
        self.retired_last_overrun = latest_overrun(self.retired_last_overrun, status.last_overrun);
        Ok(())
    }

    fn status(&self) -> Result<CaptureArenaStatus> {
        if let Some(fault) = self.worker_fault {
            return Err(LambError::CaptureInvariant(fault));
        }
        let active_status = self.epochs[self.active].status();
        let ingress_enqueued_frames = self.shared.queue.enqueued_frames.load(Ordering::Acquire);
        let ingress_dropped_frames = self.shared.queue.dropped_frames.load(Ordering::Acquire);
        let dropped_frames = self
            .retired_dropped_frames
            .checked_add(active_status.dropped_frames)
            .and_then(|total| total.checked_add(ingress_dropped_frames))
            .and_then(|total| total.checked_add(self.worker_dropped_frames))
            .ok_or(LambError::ControlInvariant(
                "cumulative dropped frame counter exhausted",
            ))?;
        let start = self.bases[self.active]
            .checked_add(self.epochs[self.active].oldest_frame())
            .ok_or(LambError::ControlInvariant(
                "absolute capture frame counter exhausted",
            ))?;
        let end = self.bases[self.active]
            .checked_add(self.epochs[self.active].write_head_frame())
            .ok_or(LambError::ControlInvariant(
                "absolute capture frame counter exhausted",
            ))?;
        Ok(CaptureArenaStatus {
            dropped_frames,
            retained_frames: active_status.retained_frames,
            capacity_frames: active_status.capacity_frames,
            last_overrun: latest_overrun(self.retired_last_overrun, active_status.last_overrun),
            active_absolute_range: start..end,
            ingress_enqueued_frames,
            capture_dropped_frames: ingress_dropped_frames,
            worker_written_frames: self.worker_written_frames,
            worker_dropped_frames: self.worker_dropped_frames,
            frozen_pending: self.frozen.is_some(),
        })
    }
}

impl Drop for WorkerState {
    fn drop(&mut self) {
        self.shared
            .final_worker_written_frames
            .store(self.worker_written_frames, Ordering::Release);
        self.shared
            .final_worker_dropped_frames
            .store(self.worker_dropped_frames, Ordering::Release);
    }
}

fn validate_runtime_plan(plan: &SessionMemoryPlan, runtime: &CaptureRuntimeConfig) -> Result<()> {
    if plan.ring_count() != EPOCH_COUNT as u32 {
        return Err(LambError::Validation(
            "capture runtime requires exactly two rings".to_string(),
        ));
    }
    let checks = [
        (
            "channels",
            u64::from(plan.channels()),
            u64::from(runtime.ring.channels),
        ),
        (
            "sample_rate",
            u64::from(plan.sample_rate()),
            u64::from(runtime.ring.sample_rate),
        ),
        (
            "chunk_frames",
            u64::from(plan.chunk_frames()),
            u64::from(runtime.ring.chunk_frames),
        ),
        (
            "chunk_count",
            u64::from(plan.chunk_count()),
            u64::from(runtime.ring.chunk_count),
        ),
        (
            "max_active_snapshots",
            u64::from(plan.max_active_snapshots()),
            u64::from(runtime.ring.max_active_snapshots),
        ),
        (
            "sample_bytes",
            u64::from(plan.sample_bytes()),
            u64::from(runtime.sample_bytes),
        ),
        (
            "capture_queue_slots",
            u64::from(plan.capture_queue_slots()),
            u64::from(runtime.queue_slots),
        ),
        (
            "capture_slot_frames",
            u64::from(plan.capture_slot_frames()),
            u64::from(runtime.slot_frames),
        ),
        (
            "capture_worker_stack_bytes",
            plan.capture_worker_stack_bytes(),
            runtime.worker_stack_bytes,
        ),
    ];
    for (name, planned, actual) in checks {
        if planned != actual {
            return Err(LambError::Validation(format!(
                "capture runtime {name} does not match memory plan"
            )));
        }
    }
    if runtime.ring.format != plan.sample_format() {
        return Err(LambError::Validation(
            "capture runtime sample_format does not match memory plan".to_string(),
        ));
    }
    let allocated_frames = u64::from(runtime.ring.chunk_frames)
        .checked_mul(u64::from(runtime.ring.chunk_count))
        .ok_or(LambError::Validation(
            "capture runtime allocated frame count overflow".to_string(),
        ))?;
    if allocated_frames != plan.allocated_retention_frames() {
        return Err(LambError::Validation(
            "capture runtime retention allocation does not match memory plan".to_string(),
        ));
    }
    Ok(())
}

fn checked_atomic_add(counter: &AtomicU64, value: u64, message: &'static str) -> Result<()> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(value)
        })
        .map(|_| ())
        .map_err(|_| LambError::CaptureInvariant(message))
}

fn latest_overrun(first: Option<SystemTime>, second: Option<SystemTime>) -> Option<SystemTime> {
    first.into_iter().chain(second).max()
}

fn command_deadline(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or(LambError::ControlInvariant(
            "capture command deadline overflow",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::Barrier;

    const DEADLINE: Duration = Duration::from_secs(2);

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    struct TestAllocator;

    unsafe impl GlobalAlloc for TestAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    #[global_allocator]
    static TEST_ALLOCATOR: TestAllocator = TestAllocator;

    pub(super) struct DropProbe(Arc<AtomicU64>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn allocation_count_during<T>(operation: impl FnOnce() -> T) -> (T, usize) {
        ALLOCATION_COUNT.with(|count| count.set(0));
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
        let result = operation();
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        (result, ALLOCATION_COUNT.with(Cell::get))
    }

    fn ingress(queue: Arc<IngressQueue>) -> CaptureIngress {
        CaptureIngress {
            queue,
            wake: Arc::new(WorkerWake {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }),
            _single_producer: PhantomData,
        }
    }

    fn plan() -> SessionMemoryPlan {
        SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames: 16,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 4,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: 8,
            capture_slot_frames: 4,
            capture_worker_stack_bytes: 256 * 1024,
            io_buffer_bytes_per_channel: 4 * 1024,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap()
    }

    fn ring_config() -> RingConfig {
        RingConfig {
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 4,
            max_active_snapshots: 1,
        }
    }

    fn runtime_config() -> CaptureRuntimeConfig {
        CaptureRuntimeConfig {
            ring: ring_config(),
            queue_slots: 8,
            slot_frames: 4,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        }
    }

    #[test]
    fn runtime_geometry_mismatches_fail_before_allocation_callback() {
        let plan = plan();
        let cases = [
            ("channels", {
                let mut runtime = runtime_config();
                runtime.ring.channels = 2;
                runtime
            }),
            ("chunks", {
                let mut runtime = runtime_config();
                runtime.ring.chunk_frames = 2;
                runtime.ring.chunk_count = 8;
                runtime
            }),
            (
                "equal aggregate bytes with different channels and chunks",
                {
                    let mut runtime = runtime_config();
                    runtime.ring.channels = 2;
                    runtime.ring.chunk_frames = 2;
                    runtime.ring.chunk_count = 4;
                    runtime
                },
            ),
            ("queue slots", {
                let mut runtime = runtime_config();
                runtime.queue_slots = 7;
                runtime
            }),
            ("queue frames", {
                let mut runtime = runtime_config();
                runtime.slot_frames = 3;
                runtime
            }),
            ("sample width", {
                let mut runtime = runtime_config();
                runtime.sample_bytes = 2;
                runtime
            }),
            ("metadata", {
                let mut runtime = runtime_config();
                runtime.ring.max_active_snapshots = 2;
                runtime
            }),
            ("sample rate", {
                let mut runtime = runtime_config();
                runtime.ring.sample_rate = 44_100;
                runtime
            }),
            ("worker stack", {
                let mut runtime = runtime_config();
                runtime.worker_stack_bytes -= 1;
                runtime
            }),
        ];

        for (name, runtime) in cases {
            let allocation_called = Cell::new(false);
            let result = CaptureArena::allocate_validated(&plan, &runtime, || {
                allocation_called.set(true);
                Ok(())
            });
            assert!(result.is_err(), "{name}");
            assert!(!allocation_called.get(), "{name}");
        }
    }

    #[test]
    fn queue_splits_in_order_and_accounts_full_remainder_once() {
        let queue = Arc::new(IngressQueue::new(2, 3, 1).unwrap());
        let ingress = ingress(Arc::clone(&queue));
        let samples = (0..8).map(|frame| frame as f32).collect::<Vec<_>>();

        let outcome = ingress.try_push_interleaved(&samples, 1).unwrap();

        assert_eq!(outcome.enqueued_frames, 6);
        assert_eq!(outcome.dropped_frames, 2);
        assert_eq!(queue.enqueued_frames.load(Ordering::Acquire), 6);
        assert_eq!(queue.dropped_frames.load(Ordering::Acquire), 2);
        let mut blocks = Vec::new();
        assert!(queue.consume_next(|samples, frames, _| {
            blocks.push((samples.to_vec(), frames));
        }));
        assert!(queue.consume_next(|samples, frames, _| {
            blocks.push((samples.to_vec(), frames));
        }));
        assert!(!queue.consume_next(|_, _, _| {}));
        assert_eq!(blocks, [(vec![0.0, 1.0, 2.0], 3), (vec![3.0, 4.0, 5.0], 3)]);
    }

    #[test]
    fn queue_sequence_and_drop_counters_fail_without_wrap() {
        let sequence_queue = IngressQueue::new(1, 2, 1).unwrap();
        sequence_queue
            .published_sequence
            .store(u64::MAX, Ordering::Release);
        sequence_queue
            .consumed_sequence
            .store(u64::MAX, Ordering::Release);
        assert!(matches!(
            sequence_queue.try_push(&[1.0], 1),
            Err(LambError::CaptureInvariant(
                "capture queue sequence exhausted"
            ))
        ));
        assert_eq!(
            sequence_queue.published_sequence.load(Ordering::Acquire),
            u64::MAX
        );

        let drop_queue = IngressQueue::new(1, 2, 1).unwrap();
        drop_queue.try_push(&[1.0], 1).unwrap();
        drop_queue.dropped_frames.store(u64::MAX, Ordering::Release);
        drop_queue
            .cumulative_dropped_frames
            .store(u64::MAX, Ordering::Release);
        assert!(matches!(
            drop_queue.try_push(&[2.0], 1),
            Err(LambError::CaptureInvariant(
                "cumulative capture dropped frame counter exhausted"
            ))
        ));
        assert_eq!(drop_queue.dropped_frames.load(Ordering::Acquire), u64::MAX);
        assert!(drop_queue.drop_counter_exhausted.load(Ordering::Acquire));
        assert!(drop_queue.try_push(&[2.0], 1).is_err());
    }

    #[test]
    fn callback_path_uses_fixed_addresses_and_no_mutex_or_allocation() {
        let queue = Arc::new(IngressQueue::new(2, 4, 1).unwrap());
        let ingress = ingress(Arc::clone(&queue));
        let addresses = queue
            .slots
            .iter()
            .map(|slot| unsafe { (&*slot.storage.get()).samples.as_slice().as_ptr() })
            .collect::<Vec<_>>();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ingress.wake.mutex.lock().unwrap();
            panic!("poison wake mutex");
        }));

        let (result, allocations) =
            allocation_count_during(|| ingress.try_push_interleaved(&[1.0, 2.0], 1));

        assert_eq!(result.unwrap().enqueued_frames, 2);
        assert_eq!(allocations, 0);
        for (slot, address) in queue.slots.iter().zip(addresses) {
            assert_eq!(
                unsafe { (&*slot.storage.get()).samples.as_slice().as_ptr() },
                address
            );
        }
        let (invalid, invalid_allocations) =
            allocation_count_during(|| ingress.try_push_interleaved(&[1.0], 2));
        assert!(matches!(invalid, Err(LambError::CaptureInvariant(_))));
        assert_eq!(invalid_allocations, 0);
        assert_eq!(queue.admitted_producers(), 0);
    }

    #[test]
    fn preparing_freeze_target_excludes_later_published_slots() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        ingress
            .try_push_interleaved(&[0.0, 1.0, 2.0, 3.0], 1)
            .unwrap();
        arena.shared.command.begin(COMMAND_KIND_FREEZE).unwrap();
        let target = arena.shared.queue.published_head();
        ingress
            .try_push_interleaved(&[4.0, 5.0, 6.0, 7.0], 1)
            .unwrap();
        unsafe {
            arena.shared.command.publish(WorkerCommand::Freeze {
                target_sequence: target,
                committed_start: None,
            })
        };
        arena.shared.wake.condvar.notify_one();

        let descriptor = match arena.wait_result(DEADLINE).unwrap() {
            WorkerReply::Frozen(Some(descriptor)) => descriptor,
            _ => panic!("expected frozen descriptor"),
        };
        assert_eq!(descriptor.absolute_range, 0..4);
        assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 4..8);
        let mut frozen = FrozenCaptureEpoch {
            runtime: Arc::clone(&arena.shared),
            ring: descriptor.ring,
            runtime_id: arena.runtime_id,
            index: descriptor.index,
            generation: descriptor.generation,
            base: descriptor.base,
            absolute_range: descriptor.absolute_range,
            local_range: descriptor.local_range,
            channels: descriptor.channels,
            sample_rate: descriptor.sample_rate,
            format: descriptor.format,
            released: false,
            release_pending: false,
        };
        arena.shared.command.begin(COMMAND_KIND_RELEASE).unwrap();
        let release_target = arena.shared.queue.published_head();
        unsafe {
            arena.shared.command.publish(WorkerCommand::Release {
                target_sequence: release_target,
                index: frozen.index,
                generation: frozen.generation,
            })
        };
        frozen.release_pending = true;
        arena
            .shared
            .command
            .abandoned
            .store(true, Ordering::Release);
        assert!(frozen
            .copy_interleaved_range_into(0..1, &mut [0.0; 1])
            .is_err());
        arena.shared.wake.condvar.notify_one();
        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
        assert!(frozen.released);
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn timed_out_non_capability_command_does_not_wedge_command_slot() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        arena.shared.command.begin(COMMAND_KIND_STATUS).unwrap();
        unsafe {
            arena
                .shared
                .command
                .publish(WorkerCommand::Status { target_sequence: 1 })
        };
        arena.shared.wake.condvar.notify_one();

        assert!(matches!(
            arena.wait_result(Duration::ZERO),
            Err(LambError::ControlInvariant("capture command timed out"))
        ));
        ingress.try_push_interleaved(&[1.0], 1).unwrap();

        arena.clear_active(DEADLINE).unwrap();
        assert_eq!(arena.active_absolute_range(DEADLINE).unwrap(), 1..1);
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn timed_out_shutdown_is_recovered_without_submitting_to_exited_worker() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        arena.shared.command.begin(COMMAND_KIND_SHUTDOWN).unwrap();
        unsafe {
            arena
                .shared
                .command
                .publish(WorkerCommand::Shutdown { target_sequence: 1 })
        };
        arena.shared.wake.condvar.notify_one();

        assert!(matches!(
            arena.wait_result(Duration::ZERO),
            Err(LambError::ControlInvariant("capture command timed out"))
        ));
        ingress.try_push_interleaved(&[1.0], 1).unwrap();

        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn acknowledged_shutdown_retry_joins_without_second_submission() {
        let (mut arena, _ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *arena.shared.shutdown_reply_pause.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&release)));

        let first = thread::spawn(move || {
            let result = arena.shutdown(Duration::from_millis(20));
            (arena, result)
        });
        entered.wait();
        let (arena, first_result) = first.join().unwrap();

        assert!(matches!(
            first_result,
            Err(LambError::ControlInvariant("capture shutdown timed out"))
        ));
        assert_eq!(
            arena.command_client.lock().unwrap().shutdown,
            CommandClientShutdown::Acknowledged
        );
        assert_eq!(
            arena
                .shared
                .command
                .submission_count
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            arena.shared.command.state.load(Ordering::Acquire),
            COMMAND_IDLE
        );

        let retry_started = Arc::new(Barrier::new(2));
        let retry_barrier = Arc::clone(&retry_started);
        let retry = thread::spawn(move || {
            retry_barrier.wait();
            let mut arena = arena;
            let result = arena.shutdown(DEADLINE);
            (arena, result)
        });
        retry_started.wait();
        release.wait();
        let (arena, retry_result) = retry.join().unwrap();

        retry_result.unwrap();
        assert_eq!(
            arena.command_client.lock().unwrap().shutdown,
            CommandClientShutdown::Joined
        );
        assert_eq!(
            arena
                .shared
                .command
                .submission_count
                .load(Ordering::Acquire),
            1
        );
        assert_eq!(
            arena.shared.command.state.load(Ordering::Acquire),
            COMMAND_IDLE
        );
        assert_eq!(arena.shared.command.kind.load(Ordering::Acquire), 0);
        assert_eq!(arena.shared.queue.published_head(), 0);
        assert_eq!(arena.shared.queue.consumed_head(), 0);
        assert!(arena.shared.worker_done.load(Ordering::Acquire));
        assert!(arena.worker.is_none());
    }

    #[test]
    fn overflowing_command_deadline_does_not_wedge_command_slot() {
        let (mut arena, _ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();

        assert!(matches!(
            arena.status(Duration::MAX),
            Err(LambError::ControlInvariant(
                "capture command deadline overflow"
            ))
        ));
        arena.clear_active(DEADLINE).unwrap();
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn overflowing_freeze_deadline_preserves_result_for_retry() {
        let (mut arena, _ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();

        assert!(matches!(
            arena.freeze_since(None, Duration::MAX),
            Err(LambError::ControlInvariant(
                "capture command deadline overflow"
            ))
        ));
        assert!(arena.freeze_since(None, DEADLINE).unwrap().is_none());
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn command_result_has_exactly_one_atomic_taker() {
        let slot = Arc::new(CommandSlot::new());
        slot.begin(COMMAND_KIND_STATUS).unwrap();
        unsafe {
            slot.publish(WorkerCommand::Status { target_sequence: 0 });
            let _ = slot.take_command();
            slot.publish_result(Ok(WorkerReply::Cleared(CaptureClearReport {
                arena_runtime_id: 0,
                coordinator_id: 0,
                clear_id: 0,
                expected_active_start: 0,
                active_absolute_range: 0..0,
                pending_retention_lost_frames: 0,
                pending_cleared_frames: 0,
                cumulative_dropped_frames: 0,
            })));
        }
        let barrier = Arc::new(Barrier::new(3));
        let callers = (0..2)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    slot.take_result().is_some()
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        let takers = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .filter(|took_result| *took_result)
            .count();

        assert_eq!(takers, 1);
        assert_eq!(slot.state.load(Ordering::Acquire), COMMAND_IDLE);
    }

    #[test]
    fn concurrent_safe_callers_recover_one_timed_out_result_once() {
        let (arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        arena.shared.command.begin(COMMAND_KIND_STATUS).unwrap();
        unsafe {
            arena
                .shared
                .command
                .publish(WorkerCommand::Status { target_sequence: 1 });
        }
        arena.shared.wake.condvar.notify_one();
        assert!(arena.wait_result(Duration::ZERO).is_err());
        ingress.try_push_interleaved(&[1.0], 1).unwrap();
        let deadline = Instant::now() + DEADLINE;
        while arena.shared.command.state.load(Ordering::Acquire) != COMMAND_RESULT_READY {
            assert!(
                Instant::now() < deadline,
                "stale status result was not published"
            );
            thread::yield_now();
        }
        ingress.try_push_interleaved(&[2.0], 1).unwrap();

        let arena = Arc::new(arena);
        let barrier = Arc::new(Barrier::new(3));
        let callers = (0..2)
            .map(|_| {
                let arena = Arc::clone(&arena);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    arena.status(DEADLINE).unwrap().active_absolute_range
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut ranges = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .collect::<Vec<_>>();
        ranges.sort_by_key(|range| range.end);

        assert_eq!(ranges, [0..1, 0..2]);
        let mut arena = Arc::into_inner(arena).unwrap();
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn command_slot_drop_releases_one_initialized_command() {
        let drops = Arc::new(AtomicU64::new(0));
        let slot = CommandSlot::new();
        slot.begin(COMMAND_KIND_STATUS).unwrap();
        unsafe {
            slot.publish(WorkerCommand::DropProbe(DropProbe(Arc::clone(&drops))));
        }

        drop(slot);

        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn command_slot_drop_releases_one_initialized_result() {
        let drops = Arc::new(AtomicU64::new(0));
        let slot = CommandSlot::new();
        slot.begin(COMMAND_KIND_STATUS).unwrap();
        unsafe {
            slot.publish(WorkerCommand::Status { target_sequence: 0 });
            let _ = slot.take_command();
            slot.publish_result(Ok(WorkerReply::DropProbe(DropProbe(Arc::clone(&drops)))));
        }

        drop(slot);

        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn stale_failed_second_freeze_is_consumed_before_release() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        ingress.try_push_interleaved(&[0.0], 1).unwrap();
        let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        arena.shared.command.begin(COMMAND_KIND_FREEZE).unwrap();
        unsafe {
            arena.shared.command.publish(WorkerCommand::Freeze {
                target_sequence: arena.shared.queue.published_head() + 1,
                committed_start: None,
            })
        };
        arena.shared.wake.condvar.notify_one();
        assert!(arena.wait_result(Duration::ZERO).is_err());
        ingress.try_push_interleaved(&[1.0], 1).unwrap();

        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn late_successful_freeze_is_retained_until_freeze_recovers_it() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        arena.shared.command.begin(COMMAND_KIND_FREEZE).unwrap();
        unsafe {
            arena.shared.command.publish(WorkerCommand::Freeze {
                target_sequence: 1,
                committed_start: None,
            })
        };
        arena.shared.wake.condvar.notify_one();
        assert!(arena.wait_result(Duration::ZERO).is_err());
        ingress.try_push_interleaved(&[7.0], 1).unwrap();

        assert!(matches!(
            arena.status(DEADLINE),
            Err(LambError::ControlInvariant(
                "frozen result pending recovery"
            ))
        ));
        let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        assert_eq!(frozen.absolute_range(), 0..1);
        let mut sample = [0.0];
        assert_eq!(
            frozen
                .copy_interleaved_range_into(0..1, &mut sample)
                .unwrap(),
            1
        );
        assert_eq!(sample, [7.0]);
        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
        arena.shutdown(DEADLINE).unwrap();
    }

    #[test]
    fn shutdown_closes_admission_then_drains_admitted_producer() {
        let (mut arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        let queue = Arc::clone(&ingress.queue);
        let shared = Arc::clone(&arena.shared);
        let admission = ProducerAdmission::acquire(&queue).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let shutdown_barrier = Arc::clone(&barrier);
        let shutdown = thread::spawn(move || {
            shutdown_barrier.wait();
            arena.shutdown(DEADLINE).unwrap();
        });

        barrier.wait();
        let deadline = Instant::now() + DEADLINE;
        while queue.producer_admission.load(Ordering::Acquire) & PRODUCER_OPEN != 0 {
            assert!(
                Instant::now() < deadline,
                "shutdown did not close admission"
            );
            thread::yield_now();
        }
        queue.try_push(&[1.0], 1).unwrap();
        drop(admission);
        shutdown.join().unwrap();

        assert_eq!(queue.published_head(), 1);
        assert_eq!(queue.consumed_head(), 1);
        assert_eq!(
            shared.final_worker_written_frames.load(Ordering::Acquire)
                + shared.final_worker_dropped_frames.load(Ordering::Acquire)
                + queue.dropped_frames.load(Ordering::Acquire),
            1
        );
    }

    #[test]
    fn drop_closes_admission_then_drains_queued_work() {
        let (arena, ingress) = CaptureArena::new(&plan(), runtime_config()).unwrap();
        let queue = Arc::clone(&ingress.queue);
        let shared = Arc::clone(&arena.shared);
        let admission = ProducerAdmission::acquire(&queue).unwrap();
        let dropped = thread::spawn(move || drop(arena));

        let deadline = Instant::now() + DEADLINE;
        while queue.producer_admission.load(Ordering::Acquire) & PRODUCER_OPEN != 0 {
            assert!(Instant::now() < deadline, "drop did not close admission");
            thread::yield_now();
        }
        queue.try_push(&[2.0], 1).unwrap();
        drop(admission);
        dropped.join().unwrap();

        assert_eq!(queue.published_head(), 1);
        assert_eq!(queue.consumed_head(), 1);
        assert_eq!(
            shared.final_worker_written_frames.load(Ordering::Acquire)
                + shared.final_worker_dropped_frames.load(Ordering::Acquire)
                + queue.dropped_frames.load(Ordering::Acquire),
            1
        );
    }

    #[test]
    fn worker_absolute_overflow_drops_slot_without_writing_ring() {
        let queue = Arc::new(IngressQueue::new(2, 4, 1).unwrap());
        queue.try_push(&[1.0, 2.0], 1).unwrap();
        let shared = Arc::new(RuntimeShared {
            queue,
            command: Arc::new(CommandSlot::new()),
            calibration: Arc::new(CalibrationSlot::new(0, 0, 1, 1).unwrap()),
            wake: Arc::new(WorkerWake {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }),
            worker_done: AtomicBool::new(false),
            final_worker_written_frames: AtomicU64::new(0),
            final_worker_dropped_frames: AtomicU64::new(0),
            shutdown_reply_pause: Mutex::new(None),
            clear_reply_pause: Mutex::new(None),
            calibration_start_pause: Mutex::new(None),
            calibration_before_accept_pause: Mutex::new(None),
            calibration_after_consume_pause: Mutex::new(None),
            calibration_timeout_pause: Mutex::new(None),
            calibration_wait_pause: Mutex::new(None),
            calibration_fault_after_start: AtomicBool::new(false),
        });
        let epochs = [
            Arc::new(SampleRing::new(ring_config()).unwrap()),
            Arc::new(SampleRing::new(ring_config()).unwrap()),
        ];
        let mut worker = WorkerState {
            shared,
            epochs,
            active: 0,
            bases: [0, 0],
            absolute_head: u64::MAX - 1,
            next_generation: 1,
            frozen: None,
            retired_dropped_frames: 0,
            retired_last_overrun: None,
            worker_written_frames: 0,
            worker_dropped_frames: 0,
            worker_fault: None,
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            calibration: None,
        };

        assert!(worker.consume_one());
        assert_eq!(worker.epochs[0].write_head_frame(), 0);
        assert_eq!(worker.worker_written_frames, 0);
        assert_eq!(worker.worker_dropped_frames, 2);
        assert_eq!(worker.shared.queue.consumed_head(), 1);
    }

    #[test]
    fn worker_written_counter_overflow_does_not_write_or_double_account_slot() {
        let queue = Arc::new(IngressQueue::new(2, 4, 1).unwrap());
        queue.try_push(&[1.0], 1).unwrap();
        let shared = Arc::new(RuntimeShared {
            queue,
            command: Arc::new(CommandSlot::new()),
            calibration: Arc::new(CalibrationSlot::new(0, 0, 1, 1).unwrap()),
            wake: Arc::new(WorkerWake {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }),
            worker_done: AtomicBool::new(false),
            final_worker_written_frames: AtomicU64::new(0),
            final_worker_dropped_frames: AtomicU64::new(0),
            shutdown_reply_pause: Mutex::new(None),
            clear_reply_pause: Mutex::new(None),
            calibration_start_pause: Mutex::new(None),
            calibration_before_accept_pause: Mutex::new(None),
            calibration_after_consume_pause: Mutex::new(None),
            calibration_timeout_pause: Mutex::new(None),
            calibration_wait_pause: Mutex::new(None),
            calibration_fault_after_start: AtomicBool::new(false),
        });
        let mut worker = WorkerState {
            shared,
            epochs: [
                Arc::new(SampleRing::new(ring_config()).unwrap()),
                Arc::new(SampleRing::new(ring_config()).unwrap()),
            ],
            active: 0,
            bases: [0, 0],
            absolute_head: 0,
            next_generation: 1,
            frozen: None,
            retired_dropped_frames: 0,
            retired_last_overrun: None,
            worker_written_frames: u64::MAX,
            worker_dropped_frames: 0,
            worker_fault: None,
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            calibration: None,
        };

        assert!(worker.consume_one());
        assert_eq!(worker.epochs[0].write_head_frame(), 0);
        assert_eq!(worker.worker_written_frames, u64::MAX);
        assert_eq!(worker.worker_dropped_frames, 1);
    }

    #[test]
    fn worker_drop_counter_overflow_sets_static_fault_without_wrap() {
        let queue = Arc::new(IngressQueue::new(2, 4, 1).unwrap());
        queue.try_push(&[1.0], 1).unwrap();
        let shared = Arc::new(RuntimeShared {
            queue,
            command: Arc::new(CommandSlot::new()),
            calibration: Arc::new(CalibrationSlot::new(0, 0, 1, 1).unwrap()),
            wake: Arc::new(WorkerWake {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }),
            worker_done: AtomicBool::new(false),
            final_worker_written_frames: AtomicU64::new(0),
            final_worker_dropped_frames: AtomicU64::new(0),
            shutdown_reply_pause: Mutex::new(None),
            clear_reply_pause: Mutex::new(None),
            calibration_start_pause: Mutex::new(None),
            calibration_before_accept_pause: Mutex::new(None),
            calibration_after_consume_pause: Mutex::new(None),
            calibration_timeout_pause: Mutex::new(None),
            calibration_wait_pause: Mutex::new(None),
            calibration_fault_after_start: AtomicBool::new(false),
        });
        let mut worker = WorkerState {
            shared,
            epochs: [
                Arc::new(SampleRing::new(ring_config()).unwrap()),
                Arc::new(SampleRing::new(ring_config()).unwrap()),
            ],
            active: 0,
            bases: [0, 0],
            absolute_head: u64::MAX,
            next_generation: 1,
            frozen: None,
            retired_dropped_frames: 0,
            retired_last_overrun: None,
            worker_written_frames: 0,
            worker_dropped_frames: u64::MAX,
            worker_fault: None,
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            calibration: None,
        };

        assert!(worker.consume_one());
        assert_eq!(worker.worker_dropped_frames, u64::MAX);
        assert_eq!(
            worker.worker_fault,
            Some("capture worker drop counter exhausted")
        );
        assert!(matches!(
            worker.status(),
            Err(LambError::CaptureInvariant(
                "capture worker drop counter exhausted"
            ))
        ));
    }

    #[test]
    fn faulted_worker_classifies_remaining_queue_frame_as_capture_dropped() {
        let queue = Arc::new(IngressQueue::new(2, 4, 1).unwrap());
        queue.try_push(&[1.0], 1).unwrap();
        let shared = Arc::new(RuntimeShared {
            queue: Arc::clone(&queue),
            command: Arc::new(CommandSlot::new()),
            calibration: Arc::new(CalibrationSlot::new(0, 0, 1, 1).unwrap()),
            wake: Arc::new(WorkerWake {
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
            }),
            worker_done: AtomicBool::new(false),
            final_worker_written_frames: AtomicU64::new(0),
            final_worker_dropped_frames: AtomicU64::new(0),
            shutdown_reply_pause: Mutex::new(None),
            clear_reply_pause: Mutex::new(None),
            calibration_start_pause: Mutex::new(None),
            calibration_before_accept_pause: Mutex::new(None),
            calibration_after_consume_pause: Mutex::new(None),
            calibration_timeout_pause: Mutex::new(None),
            calibration_wait_pause: Mutex::new(None),
            calibration_fault_after_start: AtomicBool::new(false),
        });
        let mut worker = WorkerState {
            shared,
            epochs: [
                Arc::new(SampleRing::new(ring_config()).unwrap()),
                Arc::new(SampleRing::new(ring_config()).unwrap()),
            ],
            active: 0,
            bases: [0, 0],
            absolute_head: 0,
            next_generation: 1,
            frozen: None,
            retired_dropped_frames: 0,
            retired_last_overrun: None,
            worker_written_frames: 0,
            worker_dropped_frames: 0,
            worker_fault: Some("injected worker fault"),
            channels: 1,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            calibration: None,
        };

        assert!(worker.consume_one());
        assert_eq!(worker.epochs[0].write_head_frame(), 0);
        assert_eq!(queue.consumed_head(), 1);
        assert_eq!(queue.dropped_frames.load(Ordering::Acquire), 1);
        assert_eq!(worker.worker_written_frames, 0);
        assert_eq!(worker.worker_dropped_frames, 0);
    }

    fn calibration_runtime(
        sample_rate: u32,
        seconds: u32,
        queue_slots: u32,
        slot_frames: u32,
    ) -> (Arc<CaptureArena>, CaptureIngress) {
        calibration_runtime_channels(1, sample_rate, seconds, queue_slots, slot_frames)
    }

    fn calibration_runtime_channels(
        channels: u32,
        sample_rate: u32,
        seconds: u32,
        queue_slots: u32,
        slot_frames: u32,
    ) -> (Arc<CaptureArena>, CaptureIngress) {
        let plan = calibration_plan(channels, sample_rate, seconds, queue_slots, slot_frames);
        let chunk_frames = slot_frames.max(1);
        let retention_frames = u64::from(sample_rate.max(chunk_frames));
        let (arena, ingress) = CaptureArena::new(
            &plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels,
                    sample_rate,
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
        .unwrap();
        (Arc::new(arena), ingress)
    }

    fn calibration_plan(
        channels: u32,
        sample_rate: u32,
        seconds: u32,
        queue_slots: u32,
        slot_frames: u32,
    ) -> SessionMemoryPlan {
        let chunk_frames = slot_frames.max(1);
        let retention_frames = u64::from(sample_rate.max(chunk_frames));
        SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames,
            channels,
            sample_rate,
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
            maximum_calibration_seconds: seconds,
            headroom: 1.0,
        })
        .unwrap()
    }

    fn install_pause(
        target: &Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
    ) -> (Arc<Barrier>, Arc<Barrier>) {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        *target.lock().unwrap() = Some((Arc::clone(&entered), Arc::clone(&release)));
        (entered, release)
    }

    fn wait_for_calibration_state(arena: &CaptureArena, expected: u8) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let actual = arena.shared.calibration.state.load(Ordering::Acquire);
            if actual == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "calibration state did not reach {expected} before deadline; actual state was {actual}"
            );
            thread::yield_now();
        }
    }

    fn wait_for_consumed_sequence(arena: &CaptureArena, expected: u64) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let actual = arena.shared.queue.consumed_head();
            if actual >= expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "capture queue did not consume sequence {expected} before deadline; actual sequence was {actual}"
            );
            thread::yield_now();
        }
    }

    fn capture_after_worker_acceptance(
        arena: &Arc<CaptureArena>,
        ingress: &CaptureIngress,
        samples: &[f32],
    ) -> (
        Vec<f32>,
        Vec<f32>,
        Vec<f32>,
        CalibrationCaptureMetadata,
        [usize; 3],
    ) {
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(arena);
        let frames = samples.len() as u64;
        let capture = thread::spawn(move || {
            let lease = request_arena
                .calibrate_channel(CalibrationCaptureRequest { channel: 0, frames }, DEADLINE)
                .unwrap();
            let addresses = [
                lease.samples().as_ptr() as usize,
                lease.rms().as_ptr() as usize,
                lease.peak().as_ptr() as usize,
            ];
            (
                lease.samples().to_vec(),
                lease.rms().to_vec(),
                lease.peak().to_vec(),
                lease.metadata(),
                addresses,
            )
        });
        entered.wait();
        ingress.try_push_interleaved(samples, 1).unwrap();
        release.wait();
        capture.join().unwrap()
    }

    #[test]
    fn calibration_rejects_invalid_channel_zero_frames_and_over_capacity() {
        let (arena, _ingress) = calibration_runtime(20, 1, 2, 20);
        for request in [
            CalibrationCaptureRequest {
                channel: 1,
                frames: 1,
            },
            CalibrationCaptureRequest {
                channel: 0,
                frames: 0,
            },
            CalibrationCaptureRequest {
                channel: 0,
                frames: 21,
            },
        ] {
            assert!(arena.calibrate_channel(request, DEADLINE).is_err());
        }
    }

    #[test]
    fn calibration_observes_only_future_frames_from_the_selected_channel() {
        let (arena, ingress) = calibration_runtime_channels(2, 1_000, 1, 4, 20);
        ingress
            .try_push_interleaved(&[90.0, 90.0, 91.0, 91.0], 2)
            .unwrap();
        let before = arena.status(DEADLINE).unwrap();
        assert_eq!(before.worker_written_frames, 2);

        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let capture = thread::spawn(move || {
            let lease = request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 1,
                        frames: 20,
                    },
                    DEADLINE,
                )
                .unwrap();
            lease.samples().to_vec()
        });
        entered.wait();
        let future = (1..=20)
            .flat_map(|frame| [-(frame as f32), frame as f32])
            .collect::<Vec<_>>();
        ingress.try_push_interleaved(&future, 2).unwrap();
        release.wait();
        assert_eq!(
            capture.join().unwrap(),
            (1..=20).map(|frame| frame as f32).collect::<Vec<_>>()
        );
    }

    #[test]
    fn worker_acceptance_excludes_pre_boundary_audio_and_drop_delta() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 1, 10);
        let (before_entered, before_release) =
            install_pause(&arena.shared.calibration_before_accept_pause);
        let (accepted_entered, accepted_release) =
            install_pause(&arena.shared.calibration_start_pause);
        let (consumed_entered, consumed_release) =
            install_pause(&arena.shared.calibration_after_consume_pause);
        let request_arena = Arc::clone(&arena);
        let capture = thread::spawn(move || {
            let lease = request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 10,
                    },
                    DEADLINE,
                )
                .unwrap();
            (lease.samples().to_vec(), lease.metadata())
        });

        before_entered.wait();
        let pre_acceptance = ingress.try_push_interleaved(&[9.0; 20], 1).unwrap();
        assert_eq!(pre_acceptance.enqueued_frames, 10);
        assert_eq!(pre_acceptance.dropped_frames, 10);
        before_release.wait();
        accepted_entered.wait();
        accepted_release.wait();
        consumed_entered.wait();
        ingress.try_push_interleaved(&[0.25; 10], 1).unwrap();
        consumed_release.wait();

        let (samples, metadata) = capture.join().unwrap();
        assert_eq!(samples, [0.25; 10]);
        assert_eq!(metadata.dropped_frames, 0);
    }

    #[test]
    fn calibration_accepts_one_second_and_configured_thirty_second_boundaries() {
        let (arena, ingress) = calibration_runtime(3, 30, 2, 90);
        let one_second = capture_after_worker_acceptance(&arena, &ingress, &[0.25; 3]);
        assert_eq!(one_second.3.frames, 3);
        assert!(one_second.3.usable);
        let thirty_seconds = capture_after_worker_acceptance(&arena, &ingress, &[0.5; 90]);
        assert_eq!(thirty_seconds.3.frames, 90);
        assert!(thirty_seconds.3.usable);
    }

    #[test]
    fn calibration_complete_and_incomplete_windows_use_checked_ceil_geometry() {
        let (normal, normal_ingress) = calibration_runtime(1_000, 1, 4, 25);
        let (_, rms, peak, metadata, _) =
            capture_after_worker_acceptance(&normal, &normal_ingress, &[1.0; 25]);
        assert_eq!(rms, [1.0]);
        assert_eq!(peak, [1.0]);
        assert_eq!(metadata.complete_windows, 1);
        assert_eq!(metadata.partial_final_frames, 5);
        assert!(metadata.usable);

        let (odd, odd_ingress) = calibration_runtime(441, 1, 4, 14);
        let (_, rms, peak, metadata, _) =
            capture_after_worker_acceptance(&odd, &odd_ingress, &[0.5; 14]);
        assert_eq!(rms, [0.5, 0.5]);
        assert_eq!(peak, [0.5, 0.5]);
        assert_eq!(metadata.complete_windows, 2);
        assert_eq!(metadata.partial_final_frames, 4);
        assert!(metadata.usable);

        let (_, rms, peak, metadata, _) =
            capture_after_worker_acceptance(&odd, &odd_ingress, &[0.25; 8]);
        assert!(rms.is_empty());
        assert!(peak.is_empty());
        assert_eq!(metadata.complete_windows, 0);
        assert_eq!(metadata.partial_final_frames, 3);
        assert!(!metadata.usable);
    }

    #[test]
    fn calibration_buffers_keep_addresses_across_sequential_leases() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 4, 20);
        let first = capture_after_worker_acceptance(&arena, &ingress, &[0.25; 20]);
        let second = capture_after_worker_acceptance(&arena, &ingress, &[0.5; 20]);
        assert_eq!(first.4, second.4);
    }

    #[test]
    fn calibration_preserves_active_and_frozen_capture_state() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 4, 20);
        ingress.try_push_interleaved(&[0.1; 10], 1).unwrap();
        let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        let frozen_range = frozen.absolute_range();
        let mut before = [0.0; 10];
        frozen
            .copy_interleaved_range_into(frozen_range.clone(), &mut before)
            .unwrap();
        let active_before = arena.active_absolute_range(DEADLINE).unwrap();

        let result = capture_after_worker_acceptance(&arena, &ingress, &[0.2; 20]);
        assert_eq!(result.0, [0.2; 20]);
        assert_eq!(frozen.absolute_range(), frozen_range);
        let mut after = [0.0; 10];
        frozen
            .copy_interleaved_range_into(frozen_range, &mut after)
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            arena.active_absolute_range(DEADLINE).unwrap().start,
            active_before.start
        );
        assert_eq!(
            arena.active_absolute_range(DEADLINE).unwrap().end,
            active_before.end + 20
        );
        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    }

    #[test]
    fn calibration_does_not_advance_dump_coordinator_persistence_cursor() {
        use crate::activity::FrozenExportDecision;
        use crate::dump::{DumpCoordinator, FrameRange};
        use crate::persistence_workspace::{
            PersistenceWorkspace, PersistenceWorkspaceConfig, PrepareRequest,
        };

        let plan = calibration_plan(1, 1_000, 1, 4, 20);
        let (arena, ingress) = calibration_runtime(1_000, 1, 4, 20);
        let mut workspace = PersistenceWorkspace::new(
            &plan,
            PersistenceWorkspaceConfig {
                retention_frames: 1_000,
                channels: 1,
                sample_rate: 1_000,
                sample_format: SampleFormat::F32Le,
                chunk_frames: 20,
                sample_bytes: 4,
                split_when_over_bytes: 1_000_000,
                io_buffer_bytes_per_channel: 4 * 1024,
                maximum_path_bytes: 512,
            },
        )
        .unwrap();
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        let root = tempfile::tempdir().unwrap();
        let names = vec!["mic".to_string()];

        ingress.try_push_interleaved(&[0.1; 10], 1).unwrap();
        let first = coordinator
            .persist(
                &arena,
                &mut workspace,
                PrepareRequest::Recall {
                    staging_root: &root.path().join("staging-a"),
                    output_dir: &root.path().join("output"),
                    timestamp: "20260826T120000",
                    channel_names: &names,
                },
                DEADLINE,
            )
            .unwrap();
        assert_eq!(first.range(), Some(FrameRange { start: 0, end: 10 }));

        let calibration = capture_after_worker_acceptance(&arena, &ingress, &[0.2; 20]);
        assert!(calibration.3.usable);

        let next = coordinator
            .persist(
                &arena,
                &mut workspace,
                PrepareRequest::Recall {
                    staging_root: &root.path().join("staging-b"),
                    output_dir: &root.path().join("output"),
                    timestamp: "20260826T120001",
                    channel_names: &names,
                },
                DEADLINE,
            )
            .unwrap();
        assert_eq!(next.range(), Some(FrameRange { start: 10, end: 30 }));
    }

    #[test]
    fn timeout_cancellation_blocks_reuse_until_worker_stops_writing_then_recovers() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 4, 20);
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let timed_out = thread::spawn(move || {
            request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 20,
                    },
                    Duration::from_millis(20),
                )
                .map(|_| ())
        });
        entered.wait();
        assert!(timed_out.join().unwrap().is_err());
        assert_eq!(
            arena.shared.calibration.state.load(Ordering::Acquire),
            CALIBRATION_CANCELLED
        );
        assert!(arena
            .calibrate_channel(
                CalibrationCaptureRequest {
                    channel: 0,
                    frames: 20,
                },
                DEADLINE,
            )
            .is_err());
        release.wait();
        wait_for_calibration_state(&arena, CALIBRATION_IDLE);
        let result = capture_after_worker_acceptance(&arena, &ingress, &[0.5; 20]);
        assert!(result.3.usable);
    }

    #[test]
    fn concurrent_second_calibration_is_rejected_after_first_is_accepted() {
        let (arena, _ingress) = calibration_runtime(1_000, 1, 4, 20);
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let first = thread::spawn(move || {
            request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 20,
                    },
                    Duration::from_millis(20),
                )
                .map(|_| ())
        });
        entered.wait();
        assert!(matches!(
            arena.calibrate_channel(
                CalibrationCaptureRequest {
                    channel: 0,
                    frames: 20,
                },
                DEADLINE,
            ),
            Err(LambError::ControlInvariant("calibration already pending"))
        ));
        assert!(first.join().unwrap().is_err());
        release.wait();
    }

    #[test]
    fn ordinary_status_remains_available_while_calibration_waits_for_frames() {
        let (arena, _ingress) = calibration_runtime(1_000, 1, 4, 20);
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let calibration = thread::spawn(move || {
            request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 20,
                    },
                    Duration::from_millis(100),
                )
                .map(|_| ())
        });
        entered.wait();
        release.wait();
        wait_for_calibration_state(&arena, CALIBRATION_CAPTURING);
        assert!(arena.status(Duration::from_millis(50)).is_ok());
        assert!(calibration.join().unwrap().is_err());
    }

    #[test]
    fn ingress_drop_delta_marks_calibration_unusable() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 1, 10);
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let capture = thread::spawn(move || {
            let lease = request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 20,
                    },
                    DEADLINE,
                )
                .unwrap();
            lease.metadata()
        });
        entered.wait();
        let outcome = ingress.try_push_interleaved(&[0.5; 20], 1).unwrap();
        assert_eq!(outcome.dropped_frames, 10);
        release.wait();
        wait_for_consumed_sequence(&arena, 1);
        ingress.try_push_interleaved(&[0.5; 10], 1).unwrap();
        let metadata = capture.join().unwrap();
        assert_eq!(metadata.dropped_frames, 10);
        assert!(!metadata.usable);
    }

    #[test]
    fn each_non_finite_class_marks_calibration_unusable() {
        let (arena, ingress) = calibration_runtime(1_000, 1, 4, 20);
        for non_finite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut samples = [0.5; 20];
            samples[7] = non_finite;
            let result = capture_after_worker_acceptance(&arena, &ingress, &samples);
            assert!(!result.3.usable, "{non_finite:?}");
        }
    }

    #[test]
    fn completion_wins_when_timeout_observed_stale_capturing_state() {
        let (arena, ingress) = calibration_runtime(20, 1, 2, 1);
        let (start_entered, start_release) = install_pause(&arena.shared.calibration_start_pause);
        let (timeout_entered, timeout_release) =
            install_pause(&arena.shared.calibration_timeout_pause);
        let request_arena = Arc::clone(&arena);
        let capture = thread::spawn(move || {
            let lease = request_arena.calibrate_channel(
                CalibrationCaptureRequest {
                    channel: 0,
                    frames: 1,
                },
                Duration::from_millis(20),
            )?;
            Ok::<_, LambError>(lease.samples().to_vec())
        });
        start_entered.wait();
        ingress.try_push_interleaved(&[0.75], 1).unwrap();
        timeout_entered.wait();
        start_release.wait();
        wait_for_calibration_state(&arena, CALIBRATION_COMPLETE);
        timeout_release.wait();
        assert_eq!(capture.join().unwrap().unwrap(), [0.75]);
    }

    #[test]
    fn completion_notification_cannot_be_lost_before_wait_begins() {
        let (arena, ingress) = calibration_runtime(20, 1, 2, 1);
        let (start_entered, start_release) = install_pause(&arena.shared.calibration_start_pause);
        let (wait_entered, wait_release) = install_pause(&arena.shared.calibration_wait_pause);
        let request_arena = Arc::clone(&arena);
        let (sent, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            sent.send(
                request_arena
                    .calibrate_channel(
                        CalibrationCaptureRequest {
                            channel: 0,
                            frames: 1,
                        },
                        DEADLINE,
                    )
                    .map(|lease| lease.samples().to_vec()),
            )
            .unwrap();
        });
        start_entered.wait();
        wait_entered.wait();
        ingress.try_push_interleaved(&[0.5], 1).unwrap();
        start_release.wait();
        wait_for_calibration_state(&arena, CALIBRATION_COMPLETE);
        wait_release.wait();
        assert_eq!(
            received
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .unwrap(),
            [0.5]
        );
    }

    #[test]
    fn worker_runtime_exit_wakes_calibration_and_releases_terminal_slot() {
        let (arena, _ingress) = calibration_runtime(1_000, 1, 2, 20);
        let (entered, release) = install_pause(&arena.shared.calibration_start_pause);
        let request_arena = Arc::clone(&arena);
        let capture = thread::spawn(move || {
            request_arena
                .calibrate_channel(
                    CalibrationCaptureRequest {
                        channel: 0,
                        frames: 20,
                    },
                    DEADLINE,
                )
                .map(|_| ())
        });
        entered.wait();
        arena.shared.command.begin(COMMAND_KIND_SHUTDOWN).unwrap();
        unsafe {
            arena.shared.command.publish(WorkerCommand::Shutdown {
                target_sequence: arena.shared.queue.consumed_head(),
            });
        }
        release.wait();
        arena.shared.wake.condvar.notify_all();
        assert!(capture.join().unwrap().is_err());
        assert_eq!(
            arena.shared.calibration.state.load(Ordering::Acquire),
            CALIBRATION_IDLE
        );
    }

    #[test]
    fn worker_fault_wakes_calibration_after_writer_ownership_is_relinquished() {
        let (arena, _ingress) = calibration_runtime(1_000, 1, 2, 20);
        arena
            .shared
            .calibration_fault_after_start
            .store(true, Ordering::Release);
        let result = arena.calibrate_channel(
            CalibrationCaptureRequest {
                channel: 0,
                frames: 20,
            },
            DEADLINE,
        );
        assert!(matches!(
            result,
            Err(LambError::CaptureInvariant(
                "capture worker failed during calibration"
            ))
        ));
        assert_eq!(
            arena.shared.calibration.state.load(Ordering::Acquire),
            CALIBRATION_IDLE
        );
    }
}
