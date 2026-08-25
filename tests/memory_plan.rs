use lamb::error::LambError;
use lamb::memory_plan::{
    allocation_budget_bytes, required_bytes_with_headroom, ExactArray, Materializable,
    MaterializedBuffer, SessionMemoryInputs, SessionMemoryPlan, ACTIVITY_DETECTOR_STATE_SLOT_BYTES,
    ALLOCATOR_HEADER_RESERVE_BYTES, CAPTURE_COMMAND_RESULT_SLOT_BYTES,
    CAPTURE_QUEUE_SLOT_METADATA_BYTES, FILE_WRITER_SLOT_BYTES, FROZEN_EXPORT_DECISION_SLOT_BYTES,
    MANIFEST_DIRECTORY_METADATA_BYTES, MANIFEST_ENTRY_METADATA_BYTES, MANIFEST_FIXED_PATH_ENTRIES,
    MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES, MANIFEST_JSON_ENTRY_OVERHEAD_BYTES,
    MANIFEST_JSON_FIXED_OVERHEAD_BYTES, MANIFEST_PATH_ESCAPE_MULTIPLIER,
    OPERATION_QUEUE_SLOT_BYTES, OUTPUT_PATH_SLOTS_PER_PART, PATH_SLOT_METADATA_BYTES,
    RING_CHUNK_OBJECT_RESERVE_BYTES, RING_FIXED_METADATA_RESERVE_BYTES,
    RUNTIME_METADATA_RESERVE_BYTES, SPLIT_PART_SLOT_BYTES,
};
use lamb::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::cell::Cell;
use std::mem::{align_of, size_of};
use std::rc::Rc;

#[derive(Debug)]
struct DropProbe {
    value: usize,
    drops: Rc<Cell<usize>>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

fn inputs() -> SessionMemoryInputs {
    SessionMemoryInputs {
        retention_frames: 10,
        channels: 2,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 4,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 50,
        control_queue_capacity: 3,
        worker_stack_bytes: 1_024,
        capture_queue_slots: 3,
        capture_slot_frames: 4,
        capture_worker_stack_bytes: 2_048,
        io_buffer_bytes_per_channel: 64,
        maximum_path_bytes: 128,
        maximum_calibration_seconds: 0,
        headroom: 1.0,
    }
}

#[test]
fn calibration_memory_reserves_exact_profile_sample_and_window_storage() {
    // Catches production omitting calibration buffers from memory.max or using
    // non-overlapping windows. At 1 kHz, 30 seconds is 30_000 mono samples;
    // 20-ms windows with a 10-ms hop yield 2_999 complete windows.
    let mut profile = inputs();
    profile.sample_rate = 1_000;
    profile.maximum_calibration_seconds = 30;
    let plan = SessionMemoryPlan::calculate(profile).unwrap();

    assert_eq!(plan.calibration_sample_frames(), 30_000);
    assert_eq!(plan.calibration_complete_windows(), 2_999);
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    let allocation =
        |payload: u64| (payload + ALLOCATOR_HEADER_RESERVE_BYTES).div_ceil(page) * page;
    assert_eq!(
        plan.component("calibration_samples").unwrap().bytes,
        allocation(30_000 * 4)
    );
    assert_eq!(
        plan.component("calibration_rms").unwrap().bytes,
        allocation(2_999 * 4)
    );
    assert_eq!(
        plan.component("calibration_peak").unwrap().bytes,
        allocation(2_999 * 4)
    );
    assert!(plan.committed_bytes() >= allocation(30_000 * 4) + 2 * allocation(2_999 * 4));

    let mut legacy = profile;
    legacy.maximum_calibration_seconds = 0;
    let legacy = SessionMemoryPlan::calculate(legacy).unwrap();
    assert_eq!(legacy.calibration_sample_frames(), 0);
    assert_eq!(legacy.calibration_complete_windows(), 0);
}

#[test]
fn calibration_duration_boundaries_and_detector_v1_ceil_geometry_are_checked() {
    let mut one_second = inputs();
    one_second.sample_rate = 44_101;
    one_second.maximum_calibration_seconds = 1;
    let one_second = SessionMemoryPlan::calculate(one_second).unwrap();
    assert_eq!(one_second.calibration_sample_frames(), 44_101);
    assert_eq!(one_second.calibration_window_frames(), 883);
    assert_eq!(one_second.calibration_hop_frames(), 442);
    assert_eq!(one_second.calibration_complete_windows(), 98);

    let mut thirty_seconds = inputs();
    thirty_seconds.sample_rate = 3;
    thirty_seconds.maximum_calibration_seconds = 30;
    let thirty_seconds = SessionMemoryPlan::calculate(thirty_seconds).unwrap();
    assert_eq!(thirty_seconds.calibration_sample_frames(), 90);
    assert_eq!(thirty_seconds.calibration_window_frames(), 1);
    assert_eq!(thirty_seconds.calibration_hop_frames(), 1);
    assert_eq!(thirty_seconds.calibration_complete_windows(), 90);

    let mut over_maximum = inputs();
    over_maximum.maximum_calibration_seconds = 31;
    assert!(SessionMemoryPlan::calculate(over_maximum).is_err());
}

#[test]
fn exact_array_has_stable_exact_storage_and_drops_every_element() {
    let drops = Rc::new(Cell::new(0));
    let mut array = ExactArray::try_from_fn(4, |index| {
        Ok(DropProbe {
            value: index,
            drops: Rc::clone(&drops),
        })
    })
    .unwrap();
    let address = array.as_slice().as_ptr();

    assert_eq!(array.len(), 4);
    assert_eq!(array.allocated_bytes(), 4 * size_of::<DropProbe>());
    assert_eq!(
        array
            .as_slice()
            .iter()
            .map(|probe| probe.value)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    array.as_mut_slice()[2].value = 20;
    assert_eq!(array.as_slice().as_ptr(), address);
    assert_eq!(array[2].value, 20);
    drop(array);
    assert_eq!(drops.get(), 4);
}

#[test]
fn exact_array_drops_initialized_prefix_when_initializer_fails() {
    let drops = Rc::new(Cell::new(0));
    let result: Result<ExactArray<DropProbe>, LambError> = ExactArray::try_from_fn(5, |index| {
        if index == 3 {
            return Err(LambError::Validation("injected failure".to_string()));
        }
        Ok(DropProbe {
            value: index,
            drops: Rc::clone(&drops),
        })
    });

    assert!(result.is_err());
    assert_eq!(drops.get(), 3);
}

#[test]
fn plan_components_match_concrete_small_input_formulas() {
    let plan = SessionMemoryPlan::calculate(inputs()).unwrap();
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
    let allocation = |payload: u64| {
        payload
            .checked_add(ALLOCATOR_HEADER_RESERVE_BYTES)
            .unwrap()
            .div_ceil(page)
            * page
    };
    let chunks = 3;
    let parts_per_channel = 5;
    let part_slots = parts_per_channel * 2;
    let path_slots = part_slots * OUTPUT_PATH_SLOTS_PER_PART + MANIFEST_FIXED_PATH_ENTRIES;
    let directory_slots = part_slots * 128_u64.div_ceil(2);
    let chunk_sample_bytes = 4 * 2 * 4;
    let scratch_bytes = 4 * 2 * 4;
    let capture_slot_sample_bytes = 4 * 2 * 4;

    assert_eq!(plan.ring_count(), 2);
    assert_eq!(plan.retention_frames(), 10);
    assert_eq!(plan.allocated_retention_frames(), 12);
    assert_eq!(plan.channels(), 2);
    assert_eq!(plan.sample_rate(), 48_000);
    assert_eq!(plan.sample_format(), SampleFormat::F32Le);
    assert_eq!(plan.chunk_frames(), 4);
    assert_eq!(plan.chunk_count(), 3);
    assert_eq!(plan.max_active_snapshots(), 1);
    assert_eq!(plan.sample_bytes(), 4);
    assert_eq!(plan.capture_queue_slots(), 3);
    assert_eq!(plan.capture_slot_frames(), 4);
    assert_eq!(plan.capture_worker_stack_bytes(), 2_048);
    assert_eq!(plan.component("ring_samples").unwrap().bytes, 192);
    assert_eq!(
        plan.component("ring_sample_allocator_padding")
            .unwrap()
            .bytes,
        2 * chunks * (allocation(chunk_sample_bytes) - chunk_sample_bytes)
    );
    assert_eq!(
        plan.component("ring_chunk_objects").unwrap().bytes,
        2 * chunks * allocation(RING_CHUNK_OBJECT_RESERVE_BYTES)
    );
    assert_eq!(
        plan.component("ring_chunk_index").unwrap().bytes,
        2 * allocation(chunks * size_of::<std::sync::Arc<()>>() as u64)
    );
    assert_eq!(
        plan.component("ring_fixed_metadata").unwrap().bytes,
        2 * allocation(RING_FIXED_METADATA_RESERVE_BYTES)
    );
    assert_eq!(plan.component("persistence_workspace").unwrap().bytes, 160);
    assert_eq!(
        plan.component("capture_queue_samples").unwrap().bytes,
        3 * allocation(capture_slot_sample_bytes)
    );
    assert_eq!(
        plan.component("capture_queue_slot_metadata").unwrap().bytes,
        allocation(3 * CAPTURE_QUEUE_SLOT_METADATA_BYTES)
    );
    assert_eq!(
        plan.component("capture_command_result_slot").unwrap().bytes,
        allocation(CAPTURE_COMMAND_RESULT_SLOT_BYTES)
    );
    assert_eq!(
        plan.component("capture_worker_stack").unwrap().bytes,
        allocation(2_048)
    );
    assert_eq!(
        plan.component("workspace_allocator_padding").unwrap().bytes,
        allocation(scratch_bytes) - scratch_bytes + 2 * (allocation(64) - 64)
    );
    assert_eq!(
        plan.component("split_part_slots").unwrap().bytes,
        allocation(part_slots * SPLIT_PART_SLOT_BYTES)
    );
    assert_eq!(
        plan.component("file_writer_slots").unwrap().bytes,
        allocation(2 * FILE_WRITER_SLOT_BYTES)
    );
    assert_eq!(
        plan.component("path_bytes").unwrap().bytes,
        path_slots * 128
    );
    assert_eq!(
        plan.component("path_allocator_padding").unwrap().bytes,
        path_slots * (allocation(128) - 128)
    );
    assert_eq!(
        plan.component("path_slot_metadata").unwrap().bytes,
        allocation(path_slots * PATH_SLOT_METADATA_BYTES)
    );
    assert_eq!(
        plan.component("manifest_entries").unwrap().bytes,
        allocation(part_slots * MANIFEST_ENTRY_METADATA_BYTES)
    );
    assert_eq!(
        plan.component("manifest_serialization").unwrap().bytes,
        allocation(
            MANIFEST_JSON_FIXED_OVERHEAD_BYTES
                + (3 * part_slots + MANIFEST_FIXED_PATH_ENTRIES)
                    * (128 * MANIFEST_PATH_ESCAPE_MULTIPLIER + MANIFEST_JSON_ENTRY_OVERHEAD_BYTES)
                + directory_slots * MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES
        )
    );
    assert_eq!(
        plan.component("manifest_paths").unwrap().bytes,
        allocation((3 * part_slots + MANIFEST_FIXED_PATH_ENTRIES) * 128)
    );
    assert_eq!(plan.manifest_directory_slots(), directory_slots);
    assert_eq!(
        plan.component("manifest_directories").unwrap().bytes,
        allocation(directory_slots * MANIFEST_DIRECTORY_METADATA_BYTES)
    );
    assert_eq!(
        plan.component("operation_worker_stack").unwrap().bytes,
        allocation(1_024)
    );
    assert_eq!(
        plan.component("operation_queue").unwrap().bytes,
        allocation(3 * OPERATION_QUEUE_SLOT_BYTES)
    );
    assert_eq!(
        plan.component("runtime_fixed_metadata").unwrap().bytes,
        allocation(RUNTIME_METADATA_RESERVE_BYTES)
    );
    assert_eq!(
        plan.component("frozen_export_decisions").unwrap().bytes,
        allocation(2 * 24)
    );
    assert_eq!(
        plan.component("activity_detector_workspace").unwrap().bytes,
        allocation(2 * 104) + allocation(2 * 24) + allocation(scratch_bytes)
    );
    assert_eq!(
        plan.committed_bytes(),
        plan.components()
            .iter()
            .map(|component| component.bytes)
            .sum::<u64>()
    );
    assert!(plan.required_with_headroom() >= plan.committed_bytes());
}

#[test]
fn detector_and_frozen_decision_memory_is_channel_bounded_and_included_in_maximum() {
    let base_inputs = inputs();
    let base = SessionMemoryPlan::calculate(base_inputs).unwrap();
    let mut more_channel_inputs = inputs();
    more_channel_inputs.channels = 256;
    let more_channels = SessionMemoryPlan::calculate(more_channel_inputs).unwrap();
    let allocation_budgets = |values: SessionMemoryInputs| {
        let channels = u64::from(values.channels);
        let frozen_decisions = allocation_budget_bytes(channels * 24).unwrap();
        let detector_states = allocation_budget_bytes(channels * 104).unwrap();
        let workspace_decisions = allocation_budget_bytes(channels * 24).unwrap();
        let scratch = allocation_budget_bytes(
            u64::from(values.chunk_frames) * channels * u64::from(values.sample_bytes),
        )
        .unwrap();
        (
            frozen_decisions,
            detector_states,
            workspace_decisions,
            scratch,
        )
    };
    let (frozen_decisions, detector_states, workspace_decisions, scratch) =
        allocation_budgets(base_inputs);
    let (more_frozen_decisions, more_detector_states, more_workspace_decisions, more_scratch) =
        allocation_budgets(more_channel_inputs);

    assert_eq!(FROZEN_EXPORT_DECISION_SLOT_BYTES, 24);
    assert_eq!(ACTIVITY_DETECTOR_STATE_SLOT_BYTES, 104);
    assert_eq!(
        base.component("frozen_export_decisions").unwrap().bytes,
        frozen_decisions
    );
    assert_eq!(
        base.component("activity_detector_workspace").unwrap().bytes,
        detector_states + workspace_decisions + scratch
    );
    assert_eq!(
        more_channels
            .component("frozen_export_decisions")
            .unwrap()
            .bytes,
        more_frozen_decisions
    );
    assert_eq!(
        more_channels
            .component("activity_detector_workspace")
            .unwrap()
            .bytes,
        more_detector_states + more_workspace_decisions + more_scratch
    );
    assert!(more_frozen_decisions > frozen_decisions);
    assert!(more_detector_states > detector_states);
    assert!(more_workspace_decisions > workspace_decisions);
    assert!(more_scratch > scratch);
    let detector_components = base.component("frozen_export_decisions").unwrap().bytes
        + base.component("activity_detector_workspace").unwrap().bytes;
    assert_eq!(
        base.committed_bytes(),
        base.components()
            .iter()
            .filter(|component| {
                component.name != "frozen_export_decisions"
                    && component.name != "activity_detector_workspace"
            })
            .map(|component| component.bytes)
            .sum::<u64>()
            + detector_components
    );
    assert!(base.validate_max(Some(base.committed_bytes())).is_ok());
    assert!(base.validate_max(Some(base.committed_bytes() - 1)).is_err());

    let mut overflowing = inputs();
    overflowing.channels = u32::MAX;
    overflowing.chunk_frames = u32::MAX;
    overflowing.sample_bytes = 4;
    assert!(SessionMemoryPlan::calculate(overflowing).is_err());
}

#[test]
fn detector_scratch_geometry_overflow_is_rejected() {
    let mut values = inputs();
    values.channels = u32::MAX;
    values.chunk_frames = u32::MAX;
    values.sample_bytes = 4;

    assert!(SessionMemoryPlan::calculate(values).is_err());
}

#[test]
fn component_budgets_scale_with_channels_parts_and_queue_capacity() {
    let base = SessionMemoryPlan::calculate(inputs()).unwrap();

    let mut more_channels = inputs();
    more_channels.channels = 32;
    let more_channels = SessionMemoryPlan::calculate(more_channels).unwrap();
    assert_eq!(
        more_channels.component("ring_samples").unwrap().bytes,
        base.component("ring_samples").unwrap().bytes * 16
    );
    assert!(
        more_channels.component("file_writer_slots").unwrap().bytes
            > base.component("file_writer_slots").unwrap().bytes
    );
    assert!(
        more_channels.component("path_slot_metadata").unwrap().bytes
            > base.component("path_slot_metadata").unwrap().bytes
    );
    assert!(
        more_channels
            .component("manifest_serialization")
            .unwrap()
            .bytes
            > base.component("manifest_serialization").unwrap().bytes
    );

    let mut many_parts = inputs();
    many_parts.retention_frames = 10_000;
    let many_parts = SessionMemoryPlan::calculate(many_parts).unwrap();
    let mut fewer_parts = inputs();
    fewer_parts.retention_frames = 10_000;
    fewer_parts.split_when_over_bytes = 10_000;
    let fewer_parts = SessionMemoryPlan::calculate(fewer_parts).unwrap();
    assert!(
        many_parts.component("split_part_slots").unwrap().bytes
            > fewer_parts.component("split_part_slots").unwrap().bytes
    );
    assert!(
        many_parts.component("manifest_entries").unwrap().bytes
            > fewer_parts.component("manifest_entries").unwrap().bytes
    );
    assert!(
        many_parts
            .component("manifest_serialization")
            .unwrap()
            .bytes
            > fewer_parts
                .component("manifest_serialization")
                .unwrap()
                .bytes
    );

    let mut longer_paths = inputs();
    longer_paths.maximum_path_bytes = 4_096;
    let longer_paths = SessionMemoryPlan::calculate(longer_paths).unwrap();
    assert!(
        longer_paths
            .component("manifest_serialization")
            .unwrap()
            .bytes
            > base.component("manifest_serialization").unwrap().bytes
    );

    let mut larger_queue = inputs();
    larger_queue.control_queue_capacity = 100;
    let larger_queue = SessionMemoryPlan::calculate(larger_queue).unwrap();
    assert!(
        larger_queue.component("operation_queue").unwrap().bytes
            > base.component("operation_queue").unwrap().bytes
    );
    assert_eq!(
        larger_queue
            .component("operation_worker_stack")
            .unwrap()
            .bytes,
        base.component("operation_worker_stack").unwrap().bytes
    );

    let mut larger_capture_queue = inputs();
    larger_capture_queue.capture_queue_slots = 100;
    let larger_capture_queue = SessionMemoryPlan::calculate(larger_capture_queue).unwrap();
    assert!(
        larger_capture_queue
            .component("capture_queue_samples")
            .unwrap()
            .bytes
            > base.component("capture_queue_samples").unwrap().bytes
    );
    assert!(
        larger_capture_queue
            .component("capture_queue_slot_metadata")
            .unwrap()
            .bytes
            > base.component("capture_queue_slot_metadata").unwrap().bytes
    );

    let mut larger_capture_slots = inputs();
    larger_capture_slots.capture_slot_frames = 1_024;
    let larger_capture_slots = SessionMemoryPlan::calculate(larger_capture_slots).unwrap();
    assert!(
        larger_capture_slots
            .component("capture_queue_samples")
            .unwrap()
            .bytes
            > base.component("capture_queue_samples").unwrap().bytes
    );
}

#[test]
fn manifest_budget_is_linear_in_output_count_and_maximum_path_bytes() {
    let mut values = inputs();
    values.maximum_path_bytes = 4_096;
    let plan = SessionMemoryPlan::calculate(values).unwrap();
    let output_parts = 5 * 2;
    let directory_slots = output_parts * 4_096_u64.div_ceil(2);
    let manifest_path_entries = output_parts * 3 + MANIFEST_FIXED_PATH_ENTRIES;
    let payload = MANIFEST_JSON_FIXED_OVERHEAD_BYTES
        + manifest_path_entries
            * (4_096 * MANIFEST_PATH_ESCAPE_MULTIPLIER + MANIFEST_JSON_ENTRY_OVERHEAD_BYTES)
        + directory_slots * MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES;

    assert_eq!(directory_slots, 20_480);
    assert_eq!(
        plan.component("manifest_serialization").unwrap().bytes,
        allocation_budget_bytes(payload).unwrap()
    );
    let old_quadratic_payload = MANIFEST_JSON_FIXED_OVERHEAD_BYTES
        + (manifest_path_entries + output_parts * 4_096)
            * (4_096 * MANIFEST_PATH_ESCAPE_MULTIPLIER + MANIFEST_JSON_ENTRY_OVERHEAD_BYTES)
        + output_parts
            * 4_096
            * (4_096 * MANIFEST_PATH_ESCAPE_MULTIPLIER + MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES);
    assert!(payload < old_quadratic_payload / 100);
}

#[test]
fn allocation_budgets_round_payload_plus_header_at_page_boundaries() {
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();

    for (payload, expected_padding) in [(page - 32, page + 32), (page, page), (page + 1, page - 1)]
    {
        let total = allocation_budget_bytes(payload).unwrap();
        assert_eq!(total, 2 * page, "payload={payload}");
        assert_eq!(total - payload, expected_padding, "payload={payload}");
    }
}

#[test]
fn plan_uses_checked_arithmetic_and_validates_headroom() {
    let mut values = inputs();
    values.retention_frames = u64::MAX;
    assert!(SessionMemoryPlan::calculate(values).is_err());

    for headroom in [0.99, f64::NAN, f64::INFINITY] {
        let mut values = inputs();
        values.headroom = headroom;
        assert!(SessionMemoryPlan::calculate(values).is_err());
    }

    let mut values = inputs();
    values.retention_frames = u64::MAX / 32;
    values.channels = 1;
    values.chunk_frames = u32::MAX;
    values.sample_bytes = 4;
    values.split_when_over_bytes = u64::MAX;
    values.headroom = 8.0;
    assert!(SessionMemoryPlan::calculate(values).is_err());

    for field in [
        "sample_rate",
        "max_active_snapshots",
        "capture_queue_slots",
        "capture_slot_frames",
        "capture_worker_stack_bytes",
    ] {
        let mut values = inputs();
        match field {
            "sample_rate" => values.sample_rate = 0,
            "max_active_snapshots" => values.max_active_snapshots = 0,
            "capture_queue_slots" => values.capture_queue_slots = 0,
            "capture_slot_frames" => values.capture_slot_frames = 0,
            _ => values.capture_worker_stack_bytes = 0,
        }
        assert!(SessionMemoryPlan::calculate(values).is_err(), "{field}");
    }

    let mut values = inputs();
    values.channels = u32::MAX;
    values.capture_queue_slots = u32::MAX;
    values.capture_slot_frames = u32::MAX;
    assert!(SessionMemoryPlan::calculate(values).is_err());
}

#[test]
fn headroom_rounds_up_exactly_above_f64_integer_precision() {
    let committed = (1_u64 << 53) + 1;
    let next_after_one = f64::from_bits(1.0_f64.to_bits() + 1);

    assert_eq!(
        required_bytes_with_headroom(committed, next_after_one).unwrap(),
        committed + 3
    );
    assert_eq!(
        required_bytes_with_headroom(committed, 1.5).unwrap(),
        u64::try_from((u128::from(committed) * 3).div_ceil(2)).unwrap()
    );
    assert_eq!(
        required_bytes_with_headroom(u64::MAX, 1.0).unwrap(),
        u64::MAX
    );
    assert!(required_bytes_with_headroom(u64::MAX, next_after_one).is_err());
}

#[test]
fn plan_rejects_more_chunks_than_the_ring_can_represent() {
    let mut values = inputs();
    values.retention_frames = u64::from(u32::MAX) + 1;
    values.channels = 1;
    values.chunk_frames = 1;
    values.sample_bytes = 4;
    values.split_when_over_bytes = u64::MAX;
    values.headroom = 1.0;

    let error = SessionMemoryPlan::calculate(values)
        .unwrap_err()
        .to_string();
    assert!(error.contains("chunk count"), "{error}");
}

#[test]
fn plan_rejects_sample_width_that_cannot_match_the_ring_layout() {
    let mut values = inputs();
    values.sample_bytes = 1;

    let error = SessionMemoryPlan::calculate(values)
        .unwrap_err()
        .to_string();
    assert!(error.contains("sample_bytes"), "{error}");
}

#[test]
fn allocate_within_rejects_before_invoking_the_allocation_callback() {
    let plan = SessionMemoryPlan::calculate(inputs()).unwrap();
    let allocation_called = Cell::new(false);

    let result = plan.allocate_within(Some(plan.required_with_headroom() - 1), || {
        allocation_called.set(true);
        SampleRing::new(RingConfig {
            channels: 2,
            sample_rate: 48_000,
            format: SampleFormat::F32Le,
            chunk_frames: 4,
            chunk_count: 3,
            max_active_snapshots: 1,
        })
        .map(|_| ())
    });

    let error = result.unwrap_err().to_string();
    assert!(!allocation_called.get());
    assert!(error.contains("ring_samples"), "{error}");
    assert!(error.contains("exceeds configured maximum"), "{error}");
}

#[test]
fn materialized_buffers_have_exact_supported_layouts_and_are_reusable() {
    fn assert_materializable<T: Materializable>() {}
    assert_materializable::<u8>();
    assert_materializable::<f32>();

    let mut bytes = MaterializedBuffer::<u8>::new_zeroed(1_025).unwrap();
    let byte_allocation = bytes.as_slice().as_ptr();
    assert_eq!(bytes.allocated_bytes(), 1_025);
    assert_eq!(bytes.as_slice(), &[0; 1_025]);
    bytes.as_mut_slice().fill(7);
    assert_eq!(bytes.as_slice().as_ptr(), byte_allocation);
    assert!(bytes.as_slice().iter().all(|byte| *byte == 7));

    let floats = MaterializedBuffer::<f32>::new_zeroed(1_025).unwrap();
    assert_eq!(floats.allocated_bytes(), 1_025 * size_of::<f32>());
    assert_eq!(
        floats.as_slice().as_ptr().align_offset(align_of::<f32>()),
        0
    );
    assert!(floats.as_slice().iter().all(|sample| *sample == 0.0));
    assert!(MaterializedBuffer::<u8>::new_zeroed(usize::MAX).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn materialized_buffer_has_every_linux_page_resident() {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert!(page_size > 0);
    let buffer = MaterializedBuffer::<u8>::new_zeroed(page_size * 3 + 17).unwrap();
    let address = buffer.as_slice().as_ptr() as usize;
    let page_start = address / page_size * page_size;
    let byte_end = address + buffer.as_slice().len();
    let page_end = byte_end.div_ceil(page_size) * page_size;
    let page_count = (page_end - page_start) / page_size;
    let mut residency = vec![0_u8; page_count];

    let result = unsafe {
        libc::mincore(
            page_start as *mut libc::c_void,
            page_end - page_start,
            residency.as_mut_ptr(),
        )
    };

    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    assert!(
        residency.iter().all(|entry| entry & 1 == 1),
        "{residency:?}"
    );
}

#[test]
fn ring_reports_exact_samples_and_conservative_metadata_budget() {
    let plan = SessionMemoryPlan::calculate(inputs()).unwrap();
    let ring = SampleRing::new(RingConfig {
        channels: 2,
        sample_rate: 48_000,
        format: SampleFormat::F32Le,
        chunk_frames: 4,
        chunk_count: 3,
        max_active_snapshots: 1,
    })
    .unwrap();

    assert_eq!(ring.allocated_sample_bytes(), 3 * 4 * 2 * 4);
    assert_eq!(
        plan.component("ring_samples").unwrap().bytes,
        ring.allocated_sample_bytes() * 2
    );
    assert_eq!(
        plan.component("ring_chunk_objects").unwrap().bytes
            + plan.component("ring_chunk_index").unwrap().bytes
            + plan.component("ring_fixed_metadata").unwrap().bytes
            + plan
                .component("ring_sample_allocator_padding")
                .unwrap()
                .bytes,
        ring.metadata_budget_bytes() * 2
    );

    ring.write_interleaved(&[1.0, 2.0, 3.0, 4.0], 2).unwrap();
    ring.materialize_pages().unwrap();
    let snapshot = ring.snapshot_last_frames(2).unwrap();
    assert_eq!(snapshot.read_channel_samples(0).unwrap(), [1.0, 3.0]);
    assert_eq!(snapshot.read_channel_samples(1).unwrap(), [2.0, 4.0]);
}
