use crate::error::{LambError, Result};
use crate::math::wav_part_count;
use crate::sample_ring::SampleFormat;
use std::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use std::mem::size_of;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

const RING_COUNT: u64 = 2;
const WAV_SAMPLE_BYTES: u32 = 3;

pub const OUTPUT_PATH_SLOTS_PER_PART: u64 = 2;
pub const MANIFEST_FIXED_PATH_ENTRIES: u64 = 5;

// Metadata payload ceilings are combined with this allocator reserve and rounded
// to a whole page. Later concrete slot types can assert that they fit the ceiling.
pub const ALLOCATOR_HEADER_RESERVE_BYTES: u64 = 64;
pub const RING_CHUNK_OBJECT_RESERVE_BYTES: u64 = 512;
pub const RING_FIXED_METADATA_RESERVE_BYTES: u64 = 512;
pub const SPLIT_PART_SLOT_BYTES: u64 = 32;
pub const FILE_WRITER_SLOT_BYTES: u64 = 256;
pub const PATH_SLOT_METADATA_BYTES: u64 = 32;
pub const MANIFEST_ENTRY_METADATA_BYTES: u64 = 128;
// A JSON string can encode one input byte as a six-byte `\u00xx` escape.
pub const MANIFEST_PATH_ESCAPE_MULTIPLIER: u64 = 6;
// Covers keys, punctuation, numeric dev/inode identities, and entry separators.
pub const MANIFEST_JSON_ENTRY_OVERHEAD_BYTES: u64 = 256;
// Covers the manifest envelope, version, transaction identity, and fixed fields.
pub const MANIFEST_JSON_FIXED_OVERHEAD_BYTES: u64 = 1_024;
pub const OPERATION_QUEUE_SLOT_BYTES: u64 = 256;
pub const RUNTIME_METADATA_RESERVE_BYTES: u64 = 512;
pub const CAPTURE_QUEUE_SLOT_METADATA_BYTES: u64 = 64;
pub const CAPTURE_COMMAND_RESULT_SLOT_BYTES: u64 = 512;
// ExactArray payload slots. Activity owns compile-time assertions that tie these
// module-cycle-free plan constants to the concrete private detector layouts.
pub const FROZEN_EXPORT_DECISION_SLOT_BYTES: u64 = 24;
pub const ACTIVITY_DETECTOR_CHANNEL_WORKSPACE_BYTES: u64 = 128;

#[derive(Debug, Clone, Copy)]
pub struct SessionMemoryInputs {
    pub retention_frames: u64,
    pub channels: u32,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
    pub chunk_frames: u32,
    pub max_active_snapshots: u32,
    pub sample_bytes: u32,
    pub split_when_over_bytes: u64,
    pub control_queue_capacity: u32,
    pub worker_stack_bytes: u64,
    pub capture_queue_slots: u32,
    pub capture_slot_frames: u32,
    pub capture_worker_stack_bytes: u64,
    pub io_buffer_bytes_per_channel: u64,
    pub maximum_path_bytes: u64,
    pub headroom: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryComponent {
    pub name: &'static str,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SessionMemoryPlan {
    components: Vec<MemoryComponent>,
    committed_bytes: u64,
    required_with_headroom: u64,
    retention_frames: u64,
    allocated_retention_frames: u64,
    channels: u32,
    sample_rate: u32,
    sample_format: SampleFormat,
    chunk_frames: u32,
    chunk_count: u32,
    max_active_snapshots: u32,
    sample_bytes: u32,
    capture_queue_slots: u32,
    capture_slot_frames: u32,
    capture_worker_stack_bytes: u64,
    split_when_over_bytes: u64,
    io_buffer_bytes_per_channel: u64,
    maximum_path_bytes: u64,
    maximum_wav_parts_per_channel: u64,
    output_file_slots: u64,
    path_slots: u64,
    manifest_paths_bytes: u64,
}

impl SessionMemoryPlan {
    pub fn calculate(inputs: SessionMemoryInputs) -> Result<Self> {
        validate_inputs(inputs)?;

        let chunk_count = inputs
            .retention_frames
            .div_ceil(u64::from(inputs.chunk_frames));
        let chunk_count = u32::try_from(chunk_count).map_err(|_| {
            LambError::Validation("ring chunk count exceeds supported maximum".to_string())
        })?;
        let allocated_frames = checked_mul(
            "ring frame allocation overflow",
            u64::from(chunk_count),
            u64::from(inputs.chunk_frames),
        )?;
        let samples_per_ring = checked_mul(
            "ring sample count overflow",
            allocated_frames,
            u64::from(inputs.channels),
        )?;
        let sample_bytes_per_ring = checked_mul(
            "ring sample byte count overflow",
            samples_per_ring,
            u64::from(inputs.sample_bytes),
        )?;
        let ring_samples = checked_mul(
            "dual-ring sample byte count overflow",
            sample_bytes_per_ring,
            RING_COUNT,
        )?;
        let chunk_samples = checked_mul(
            "chunk sample count overflow",
            u64::from(inputs.chunk_frames),
            u64::from(inputs.channels),
        )?;
        let chunk_sample_bytes = checked_mul(
            "chunk sample byte count overflow",
            chunk_samples,
            u64::from(inputs.sample_bytes),
        )?;
        let ring_metadata = ring_metadata_budget(u64::from(chunk_count), chunk_sample_bytes)?;
        let ring_sample_allocator_padding = checked_mul(
            "dual-ring sample allocator padding overflow",
            ring_metadata.sample_allocator_padding,
            RING_COUNT,
        )?;
        let ring_chunk_objects = checked_mul(
            "dual-ring chunk object budget overflow",
            ring_metadata.chunk_objects,
            RING_COUNT,
        )?;
        let ring_chunk_index = checked_mul(
            "dual-ring chunk index budget overflow",
            ring_metadata.chunk_index,
            RING_COUNT,
        )?;
        let ring_fixed_metadata = checked_mul(
            "dual-ring fixed metadata budget overflow",
            ring_metadata.fixed,
            RING_COUNT,
        )?;

        let capture_slot_sample_bytes = checked_mul(
            "capture slot sample count overflow",
            u64::from(inputs.capture_slot_frames),
            u64::from(inputs.channels),
        )
        .and_then(|samples| {
            checked_mul(
                "capture slot sample byte count overflow",
                samples,
                u64::from(inputs.sample_bytes),
            )
        })?;
        let capture_queue_samples = checked_mul(
            "capture queue sample storage overflow",
            u64::from(inputs.capture_queue_slots),
            allocation_budget_bytes(capture_slot_sample_bytes)?,
        )?;
        let capture_queue_slot_metadata = checked_mul(
            "capture queue slot metadata overflow",
            u64::from(inputs.capture_queue_slots),
            CAPTURE_QUEUE_SLOT_METADATA_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let capture_command_result_slot =
            allocation_budget_bytes(CAPTURE_COMMAND_RESULT_SLOT_BYTES)?;
        let capture_worker_stack = allocation_budget_bytes(inputs.capture_worker_stack_bytes)?;

        let interleaved_scratch = checked_mul(
            "interleaved scratch sample count overflow",
            u64::from(inputs.chunk_frames),
            u64::from(inputs.channels),
        )
        .and_then(|samples| {
            checked_mul(
                "interleaved scratch byte count overflow",
                samples,
                u64::from(inputs.sample_bytes),
            )
        })?;
        let channel_io = checked_mul(
            "channel I/O workspace overflow",
            u64::from(inputs.channels),
            inputs.io_buffer_bytes_per_channel,
        )?;
        let persistence_workspace = checked_add(
            "persistence workspace overflow",
            interleaved_scratch,
            channel_io,
        )?;
        let scratch_allocator_padding = allocation_budget_bytes(interleaved_scratch)?
            .checked_sub(interleaved_scratch)
            .ok_or_else(|| {
                LambError::Validation("scratch allocator padding underflow".to_string())
            })?;
        let channel_allocator_padding =
            allocation_budget_bytes(inputs.io_buffer_bytes_per_channel)?
                .checked_sub(inputs.io_buffer_bytes_per_channel)
                .ok_or_else(|| {
                    LambError::Validation("channel allocator padding underflow".to_string())
                })?;
        let all_channel_allocator_padding = checked_mul(
            "channel allocator padding overflow",
            u64::from(inputs.channels),
            channel_allocator_padding,
        )?;
        let workspace_allocator_padding = checked_add(
            "workspace allocator padding overflow",
            scratch_allocator_padding,
            all_channel_allocator_padding,
        )?;

        let parts_per_channel = wav_part_count(
            inputs.retention_frames,
            WAV_SAMPLE_BYTES,
            inputs.split_when_over_bytes,
        )?;
        let part_slots = checked_mul(
            "WAV part slot count overflow",
            parts_per_channel,
            u64::from(inputs.channels),
        )?;
        let split_part_slots = checked_mul(
            "split-part slot byte count overflow",
            part_slots,
            SPLIT_PART_SLOT_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let file_writer_slots = checked_mul(
            "file/writer slot byte count overflow",
            u64::from(inputs.channels),
            FILE_WRITER_SLOT_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let output_path_slots = checked_mul(
            "output path slot count overflow",
            part_slots,
            OUTPUT_PATH_SLOTS_PER_PART,
        )?;
        let path_slot_count = checked_add(
            "path slot count overflow",
            output_path_slots,
            MANIFEST_FIXED_PATH_ENTRIES,
        )?;
        let path_bytes = checked_mul(
            "path byte count overflow",
            path_slot_count,
            inputs.maximum_path_bytes,
        )?;
        let path_allocator_padding_per_slot = allocation_budget_bytes(inputs.maximum_path_bytes)?
            .checked_sub(inputs.maximum_path_bytes)
            .ok_or_else(|| LambError::Validation("path allocator padding underflow".to_string()))?;
        let path_allocator_padding = checked_mul(
            "path allocator padding overflow",
            path_slot_count,
            path_allocator_padding_per_slot,
        )?;
        let path_slot_metadata = checked_mul(
            "path slot metadata byte count overflow",
            path_slot_count,
            PATH_SLOT_METADATA_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let manifest_entries = checked_mul(
            "manifest entry byte count overflow",
            part_slots,
            MANIFEST_ENTRY_METADATA_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let manifest_path_entries = path_slot_count;
        let escaped_manifest_path_bytes = checked_mul(
            "escaped manifest path byte count overflow",
            inputs.maximum_path_bytes,
            MANIFEST_PATH_ESCAPE_MULTIPLIER,
        )?;
        let manifest_path_entry_bytes = checked_add(
            "manifest path entry byte count overflow",
            escaped_manifest_path_bytes,
            MANIFEST_JSON_ENTRY_OVERHEAD_BYTES,
        )?;
        let all_manifest_path_entries = checked_mul(
            "manifest path entries byte count overflow",
            manifest_path_entries,
            manifest_path_entry_bytes,
        )?;
        let manifest_serialization_payload = checked_add(
            "manifest serialization payload overflow",
            MANIFEST_JSON_FIXED_OVERHEAD_BYTES,
            all_manifest_path_entries,
        )?;
        let manifest_serialization = allocation_budget_bytes(manifest_serialization_payload)?;
        let manifest_paths_payload = checked_mul(
            "manifest path arena byte count overflow",
            checked_add(
                "manifest path arena slot count overflow",
                checked_mul("manifest entry path slot count overflow", part_slots, 3)?,
                MANIFEST_FIXED_PATH_ENTRIES,
            )?,
            inputs.maximum_path_bytes,
        )?;
        let manifest_paths = allocation_budget_bytes(manifest_paths_payload)?;
        let operation_queue = checked_mul(
            "operation queue byte count overflow",
            u64::from(inputs.control_queue_capacity),
            OPERATION_QUEUE_SLOT_BYTES,
        )
        .and_then(allocation_budget_bytes)?;
        let operation_worker_stack = allocation_budget_bytes(inputs.worker_stack_bytes)?;
        let runtime_fixed_metadata = allocation_budget_bytes(RUNTIME_METADATA_RESERVE_BYTES)?;
        let frozen_export_decisions = checked_mul(
            "frozen export decision storage overflow",
            u64::from(inputs.channels),
            FROZEN_EXPORT_DECISION_SLOT_BYTES,
        )?;
        let activity_detector_workspace = checked_add(
            "activity detector workspace overflow",
            checked_mul(
                "activity detector channel workspace overflow",
                u64::from(inputs.channels),
                ACTIVITY_DETECTOR_CHANNEL_WORKSPACE_BYTES,
            )?,
            interleaved_scratch,
        )?;

        let components = vec![
            MemoryComponent {
                name: "ring_samples",
                bytes: ring_samples,
            },
            MemoryComponent {
                name: "ring_sample_allocator_padding",
                bytes: ring_sample_allocator_padding,
            },
            MemoryComponent {
                name: "ring_chunk_objects",
                bytes: ring_chunk_objects,
            },
            MemoryComponent {
                name: "ring_chunk_index",
                bytes: ring_chunk_index,
            },
            MemoryComponent {
                name: "ring_fixed_metadata",
                bytes: ring_fixed_metadata,
            },
            MemoryComponent {
                name: "capture_queue_samples",
                bytes: capture_queue_samples,
            },
            MemoryComponent {
                name: "capture_queue_slot_metadata",
                bytes: capture_queue_slot_metadata,
            },
            MemoryComponent {
                name: "capture_command_result_slot",
                bytes: capture_command_result_slot,
            },
            MemoryComponent {
                name: "capture_worker_stack",
                bytes: capture_worker_stack,
            },
            MemoryComponent {
                name: "persistence_workspace",
                bytes: persistence_workspace,
            },
            MemoryComponent {
                name: "workspace_allocator_padding",
                bytes: workspace_allocator_padding,
            },
            MemoryComponent {
                name: "split_part_slots",
                bytes: split_part_slots,
            },
            MemoryComponent {
                name: "file_writer_slots",
                bytes: file_writer_slots,
            },
            MemoryComponent {
                name: "path_bytes",
                bytes: path_bytes,
            },
            MemoryComponent {
                name: "path_allocator_padding",
                bytes: path_allocator_padding,
            },
            MemoryComponent {
                name: "path_slot_metadata",
                bytes: path_slot_metadata,
            },
            MemoryComponent {
                name: "manifest_entries",
                bytes: manifest_entries,
            },
            MemoryComponent {
                name: "manifest_serialization",
                bytes: manifest_serialization,
            },
            MemoryComponent {
                name: "manifest_paths",
                bytes: manifest_paths,
            },
            MemoryComponent {
                name: "operation_worker_stack",
                bytes: operation_worker_stack,
            },
            MemoryComponent {
                name: "operation_queue",
                bytes: operation_queue,
            },
            MemoryComponent {
                name: "runtime_fixed_metadata",
                bytes: runtime_fixed_metadata,
            },
            MemoryComponent {
                name: "frozen_export_decisions",
                bytes: frozen_export_decisions,
            },
            MemoryComponent {
                name: "activity_detector_workspace",
                bytes: activity_detector_workspace,
            },
        ];
        let committed_bytes = components.iter().try_fold(0_u64, |total, component| {
            checked_add(
                "committed memory byte count overflow",
                total,
                component.bytes,
            )
        })?;
        let required_with_headroom =
            required_bytes_with_headroom(committed_bytes, inputs.headroom)?;

        Ok(Self {
            components,
            committed_bytes,
            required_with_headroom,
            retention_frames: inputs.retention_frames,
            allocated_retention_frames: allocated_frames,
            channels: inputs.channels,
            sample_rate: inputs.sample_rate,
            sample_format: inputs.sample_format,
            chunk_frames: inputs.chunk_frames,
            chunk_count,
            max_active_snapshots: inputs.max_active_snapshots,
            sample_bytes: inputs.sample_bytes,
            capture_queue_slots: inputs.capture_queue_slots,
            capture_slot_frames: inputs.capture_slot_frames,
            capture_worker_stack_bytes: inputs.capture_worker_stack_bytes,
            split_when_over_bytes: inputs.split_when_over_bytes,
            io_buffer_bytes_per_channel: inputs.io_buffer_bytes_per_channel,
            maximum_path_bytes: inputs.maximum_path_bytes,
            maximum_wav_parts_per_channel: parts_per_channel,
            output_file_slots: part_slots,
            path_slots: path_slot_count,
            manifest_paths_bytes: manifest_paths,
        })
    }

    pub fn ring_count(&self) -> u32 {
        RING_COUNT as u32
    }

    pub fn retention_frames(&self) -> u64 {
        self.retention_frames
    }

    pub fn allocated_retention_frames(&self) -> u64 {
        self.allocated_retention_frames
    }

    pub fn channels(&self) -> u32 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    pub fn chunk_frames(&self) -> u32 {
        self.chunk_frames
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunk_count
    }

    pub fn max_active_snapshots(&self) -> u32 {
        self.max_active_snapshots
    }

    pub fn sample_bytes(&self) -> u32 {
        self.sample_bytes
    }

    pub fn components(&self) -> &[MemoryComponent] {
        &self.components
    }

    pub fn component(&self, name: &str) -> Option<&MemoryComponent> {
        self.components
            .iter()
            .find(|component| component.name == name)
    }

    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    pub fn required_with_headroom(&self) -> u64 {
        self.required_with_headroom
    }

    pub fn capture_queue_slots(&self) -> u32 {
        self.capture_queue_slots
    }

    pub fn capture_slot_frames(&self) -> u32 {
        self.capture_slot_frames
    }

    pub fn capture_worker_stack_bytes(&self) -> u64 {
        self.capture_worker_stack_bytes
    }

    pub fn split_when_over_bytes(&self) -> u64 {
        self.split_when_over_bytes
    }

    pub fn io_buffer_bytes_per_channel(&self) -> u64 {
        self.io_buffer_bytes_per_channel
    }

    pub fn maximum_path_bytes(&self) -> u64 {
        self.maximum_path_bytes
    }

    pub fn maximum_wav_parts_per_channel(&self) -> u64 {
        self.maximum_wav_parts_per_channel
    }

    pub fn output_file_slots(&self) -> u64 {
        self.output_file_slots
    }

    pub fn path_slots(&self) -> u64 {
        self.path_slots
    }

    pub fn manifest_paths_bytes(&self) -> u64 {
        self.manifest_paths_bytes
    }

    pub fn validate_max(&self, maximum: Option<u64>) -> Result<()> {
        if let Some(limit) = maximum.filter(|limit| self.required_with_headroom > *limit) {
            return Err(LambError::Validation(self.describe_limit_failure(limit)));
        }
        Ok(())
    }

    pub fn allocate_within<T>(
        &self,
        maximum: Option<u64>,
        allocate: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.validate_max(maximum)?;
        allocate()
    }

    fn describe_limit_failure(&self, limit: u64) -> String {
        let components = self
            .components
            .iter()
            .map(|component| format!("{}={} bytes", component.name, component.bytes))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "session memory requirement {} bytes exceeds configured maximum {} bytes (committed={} bytes; {})",
            self.required_with_headroom, limit, self.committed_bytes, components
        )
    }
}

mod sealed {
    pub trait Sealed {}

    impl Sealed for u8 {}
    impl Sealed for f32 {}
}

#[derive(Debug)]
pub struct ExactArray<T> {
    pointer: NonNull<T>,
    length: usize,
    layout: Layout,
}

impl<T> ExactArray<T> {
    pub fn try_from_fn(
        length: usize,
        mut initialize: impl FnMut(usize) -> Result<T>,
    ) -> Result<Self> {
        let layout = Layout::array::<T>(length)
            .map_err(|_| LambError::Validation("exact array layout overflow".to_string()))?;
        let pointer = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            let allocation = unsafe { alloc(layout) }.cast::<T>();
            NonNull::new(allocation).ok_or_else(|| {
                LambError::Validation(format!(
                    "unable to allocate {}-byte exact array",
                    layout.size()
                ))
            })?
        };
        let mut guard = ExactArrayInitGuard {
            pointer,
            initialized: 0,
            layout,
        };
        for index in 0..length {
            let value = initialize(index)?;
            unsafe { guard.pointer.as_ptr().add(index).write(value) };
            guard.initialized += 1;
        }

        let array = Self {
            pointer,
            length,
            layout,
        };
        std::mem::forget(guard);
        Ok(array)
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn allocated_bytes(&self) -> usize {
        self.layout.size()
    }

    pub fn as_slice(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }
}

impl<T> Drop for ExactArray<T> {
    fn drop(&mut self) {
        let _allocation = ExactArrayAllocationGuard {
            pointer: self.pointer,
            layout: self.layout,
        };
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.pointer.as_ptr(),
                self.length,
            ));
        }
    }
}

impl<T> Deref for ExactArray<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T> DerefMut for ExactArray<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

unsafe impl<T: Send> Send for ExactArray<T> {}
unsafe impl<T: Sync> Sync for ExactArray<T> {}

struct ExactArrayInitGuard<T> {
    pointer: NonNull<T>,
    initialized: usize,
    layout: Layout,
}

impl<T> Drop for ExactArrayInitGuard<T> {
    fn drop(&mut self) {
        let _allocation = ExactArrayAllocationGuard {
            pointer: self.pointer,
            layout: self.layout,
        };
        unsafe {
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                self.pointer.as_ptr(),
                self.initialized,
            ));
        }
    }
}

struct ExactArrayAllocationGuard<T> {
    pointer: NonNull<T>,
    layout: Layout,
}

impl<T> Drop for ExactArrayAllocationGuard<T> {
    fn drop(&mut self) {
        if self.layout.size() != 0 {
            unsafe { deallocate_exact_array(self.pointer, self.layout) };
        }
    }
}

unsafe fn deallocate_exact_array<T>(pointer: NonNull<T>, layout: Layout) {
    unsafe { dealloc(pointer.as_ptr().cast::<u8>(), layout) };
    #[cfg(test)]
    EXACT_ARRAY_DEALLOCATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
std::thread_local! {
    static EXACT_ARRAY_DEALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn exact_array_deallocation_count() -> usize {
    EXACT_ARRAY_DEALLOCATIONS.with(std::cell::Cell::get)
}

/// Types whose all-zero bit pattern is valid and whose values may be safely
/// copied through volatile accesses while an arena is materialized.
///
/// # Safety
///
/// Implementations must have a valid all-zero representation and no uninitialized
/// padding. This trait is sealed to the audited implementations in this module.
pub unsafe trait Materializable:
    sealed::Sealed + Copy + Default + Send + Sync + 'static
{
}

// SAFETY: Every bit pattern is valid for u8 and it has no padding.
unsafe impl Materializable for u8 {}
// SAFETY: IEEE-754 positive zero is all-zero and f32 has no padding.
unsafe impl Materializable for f32 {}

#[derive(Debug)]
pub struct MaterializedBuffer<T: Materializable> {
    pointer: NonNull<T>,
    length: usize,
    layout: Layout,
}

impl<T: Materializable> MaterializedBuffer<T> {
    pub fn new_zeroed(len: usize) -> Result<Self> {
        let layout = Layout::array::<T>(len).map_err(|_| {
            LambError::Validation("materialized buffer layout overflow".to_string())
        })?;
        let pointer = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            let allocation = unsafe { alloc_zeroed(layout) }.cast::<T>();
            NonNull::new(allocation).ok_or_else(|| {
                LambError::Validation(format!(
                    "unable to allocate {}-byte materialized buffer",
                    layout.size()
                ))
            })?
        };
        let mut buffer = Self {
            pointer,
            length: len,
            layout,
        };
        buffer.materialize_pages()?;
        Ok(buffer)
    }

    pub fn as_slice(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.layout.size()
    }

    pub fn materialize_pages(&mut self) -> Result<()> {
        let byte_len = self.layout.size();
        if byte_len == 0 {
            return Ok(());
        }

        let start = self.pointer.as_ptr() as usize;
        let end = start.checked_add(byte_len).ok_or_else(|| {
            LambError::Validation("materialized buffer address overflow".to_string())
        })?;
        let page_size = system_page_size();
        let mut page = start - start % page_size;
        while page < end {
            let byte_offset = start.max(page).checked_sub(start).ok_or_else(|| {
                LambError::Validation("materialized page offset overflow".to_string())
            })?;
            let element_index = byte_offset / size_of::<T>();
            let element = unsafe { self.pointer.as_ptr().add(element_index) };
            unsafe {
                ptr::write_volatile(element, ptr::read_volatile(element));
            }
            page = page.checked_add(page_size).ok_or_else(|| {
                LambError::Validation("materialized page address overflow".to_string())
            })?;
        }

        let final_element = unsafe { self.pointer.as_ptr().add(self.length - 1) };
        unsafe {
            ptr::write_volatile(final_element, ptr::read_volatile(final_element));
        }
        Ok(())
    }
}

impl<T: Materializable> Drop for MaterializedBuffer<T> {
    fn drop(&mut self) {
        if self.layout.size() != 0 {
            unsafe { dealloc(self.pointer.as_ptr().cast::<u8>(), self.layout) };
        }
    }
}

unsafe impl<T: Materializable> Send for MaterializedBuffer<T> {}
unsafe impl<T: Materializable> Sync for MaterializedBuffer<T> {}

impl<T: Materializable> Deref for MaterializedBuffer<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T: Materializable> DerefMut for MaterializedBuffer<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut_slice()
    }
}

impl<T: Materializable> AsRef<[T]> for MaterializedBuffer<T> {
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: Materializable> AsMut<[T]> for MaterializedBuffer<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

#[cfg(unix)]
fn system_page_size() -> usize {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page_size)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4_096)
}

#[cfg(not(unix))]
fn system_page_size() -> usize {
    4_096
}

fn validate_inputs(inputs: SessionMemoryInputs) -> Result<()> {
    for (name, value) in [
        ("retention_frames", inputs.retention_frames),
        ("channels", u64::from(inputs.channels)),
        ("sample_rate", u64::from(inputs.sample_rate)),
        ("chunk_frames", u64::from(inputs.chunk_frames)),
        (
            "max_active_snapshots",
            u64::from(inputs.max_active_snapshots),
        ),
        ("sample_bytes", u64::from(inputs.sample_bytes)),
        (
            "control_queue_capacity",
            u64::from(inputs.control_queue_capacity),
        ),
        ("worker_stack_bytes", inputs.worker_stack_bytes),
        ("capture_queue_slots", u64::from(inputs.capture_queue_slots)),
        ("capture_slot_frames", u64::from(inputs.capture_slot_frames)),
        (
            "capture_worker_stack_bytes",
            inputs.capture_worker_stack_bytes,
        ),
        (
            "io_buffer_bytes_per_channel",
            inputs.io_buffer_bytes_per_channel,
        ),
        ("maximum_path_bytes", inputs.maximum_path_bytes),
    ] {
        if value == 0 {
            return Err(LambError::Validation(format!("{name} must be > 0")));
        }
    }
    if inputs.sample_bytes != size_of::<f32>() as u32 {
        return Err(LambError::Validation(format!(
            "sample_bytes must be {} for the supported f32 ring layout",
            size_of::<f32>()
        )));
    }
    if inputs.sample_format != SampleFormat::F32Le {
        return Err(LambError::Validation(
            "sample_format must be F32Le for the supported ring layout".to_string(),
        ));
    }
    if inputs.headroom < 1.0 || !inputs.headroom.is_finite() {
        return Err(LambError::Validation(
            "headroom must be finite and >= 1.0".to_string(),
        ));
    }
    Ok(())
}

fn checked_add(context: &str, left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| LambError::Validation(context.to_string()))
}

fn checked_mul(context: &str, left: u64, right: u64) -> Result<u64> {
    left.checked_mul(right)
        .ok_or_else(|| LambError::Validation(context.to_string()))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RingMetadataBudget {
    pub sample_allocator_padding: u64,
    pub chunk_objects: u64,
    pub chunk_index: u64,
    pub fixed: u64,
}

impl RingMetadataBudget {
    pub fn total(self) -> Result<u64> {
        checked_add(
            "ring metadata budget overflow",
            self.sample_allocator_padding,
            self.chunk_objects,
        )
        .and_then(|subtotal| {
            checked_add("ring metadata budget overflow", subtotal, self.chunk_index)
        })
        .and_then(|subtotal| checked_add("ring metadata budget overflow", subtotal, self.fixed))
    }
}

pub(crate) fn ring_metadata_budget(
    chunk_object_count: u64,
    chunk_sample_bytes: u64,
) -> Result<RingMetadataBudget> {
    let chunk_object_allocation = allocation_budget_bytes(RING_CHUNK_OBJECT_RESERVE_BYTES)?;
    let chunk_objects = checked_mul(
        "ring chunk object budget overflow",
        chunk_object_count,
        chunk_object_allocation,
    )?;
    let chunk_index_payload = checked_mul(
        "ring chunk index payload overflow",
        chunk_object_count,
        size_of::<usize>() as u64,
    )?;
    let sample_allocator_padding = allocation_budget_bytes(chunk_sample_bytes)?
        .checked_sub(chunk_sample_bytes)
        .ok_or_else(|| {
            LambError::Validation("ring sample allocator padding underflow".to_string())
        })?;
    Ok(RingMetadataBudget {
        sample_allocator_padding: checked_mul(
            "ring sample allocator padding overflow",
            chunk_object_count,
            sample_allocator_padding,
        )?,
        chunk_objects,
        chunk_index: allocation_budget_bytes(chunk_index_payload)?,
        fixed: allocation_budget_bytes(RING_FIXED_METADATA_RESERVE_BYTES)?,
    })
}

pub fn allocation_budget_bytes(payload_bytes: u64) -> Result<u64> {
    let reserved = checked_add(
        "allocation reserve overflow",
        payload_bytes,
        ALLOCATOR_HEADER_RESERVE_BYTES,
    )?;
    let page_size = u64::try_from(system_page_size())
        .map_err(|_| LambError::Validation("system page size overflow".to_string()))?;
    checked_mul(
        "page-rounded allocation budget overflow",
        reserved.div_ceil(page_size),
        page_size,
    )
}

pub fn required_bytes_with_headroom(committed_bytes: u64, headroom: f64) -> Result<u64> {
    if headroom < 1.0 || !headroom.is_finite() {
        return Err(LambError::Validation(
            "headroom must be finite and >= 1.0".to_string(),
        ));
    }

    let bits = headroom.to_bits();
    let exponent = i32::from(((bits >> 52) & 0x7ff) as u16) - 1023 - 52;
    let mut numerator = u128::from((bits & ((1_u64 << 52) - 1)) | (1_u64 << 52));
    let denominator = if exponent < 0 {
        let mut denominator_exponent = u32::try_from(-exponent)
            .map_err(|_| LambError::Validation("headroom ratio exponent overflow".to_string()))?;
        let common_power = numerator.trailing_zeros().min(denominator_exponent);
        numerator >>= common_power;
        denominator_exponent -= common_power;
        1_u128
            .checked_shl(denominator_exponent)
            .ok_or_else(|| LambError::Validation("headroom ratio exponent overflow".to_string()))?
    } else {
        numerator = numerator
            .checked_shl(u32::try_from(exponent).map_err(|_| {
                LambError::Validation("headroom ratio exponent overflow".to_string())
            })?)
            .ok_or_else(|| LambError::Validation("headroom ratio overflow".to_string()))?;
        1
    };

    let product = u128::from(committed_bytes)
        .checked_mul(numerator)
        .ok_or_else(|| LambError::Validation("memory requirement overflow".to_string()))?;
    let quotient = product / denominator;
    let required = quotient
        .checked_add(u128::from(product % denominator != 0))
        .ok_or_else(|| LambError::Validation("memory requirement overflow".to_string()))?;
    u64::try_from(required)
        .map_err(|_| LambError::Validation("memory requirement with headroom overflow".to_string()))
}

#[cfg(test)]
mod exact_array_panic_tests {
    use super::{exact_array_deallocation_count, ExactArray};
    use crate::error::LambError;
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::rc::Rc;

    struct PanicDrop {
        panic: bool,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for PanicDrop {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            assert!(!self.panic, "injected drop panic");
        }
    }

    #[test]
    fn exact_array_deallocates_when_full_drop_panics() {
        let before = exact_array_deallocation_count();
        let drops = Rc::new(Cell::new(0));
        let array = ExactArray::try_from_fn(2, |index| {
            Ok(PanicDrop {
                panic: index == 0,
                drops: Rc::clone(&drops),
            })
        })
        .unwrap();

        let result = catch_unwind(AssertUnwindSafe(|| drop(array)));

        assert!(result.is_err());
        assert_eq!(exact_array_deallocation_count(), before + 1);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn exact_array_deallocates_when_partial_cleanup_drop_panics() {
        let before = exact_array_deallocation_count();
        let drops = Rc::new(Cell::new(0));

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _: Result<ExactArray<PanicDrop>, LambError> = ExactArray::try_from_fn(2, |index| {
                if index == 1 {
                    return Err(LambError::Validation("injected failure".to_string()));
                }
                Ok(PanicDrop {
                    panic: true,
                    drops: Rc::clone(&drops),
                })
            });
        }));

        assert!(result.is_err());
        assert_eq!(exact_array_deallocation_count(), before + 1);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn exact_array_normal_drop_deallocates_once() {
        let before = exact_array_deallocation_count();
        let array = ExactArray::try_from_fn(2, |_| Ok(7_u64)).unwrap();

        drop(array);

        assert_eq!(exact_array_deallocation_count(), before + 1);
    }
}
