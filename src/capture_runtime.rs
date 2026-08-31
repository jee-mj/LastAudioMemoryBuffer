use crate::activity::FrozenExportDecision;
use crate::capture_arena::{CaptureArena, CaptureIngress, CaptureRuntimeConfig};
use crate::error::{LambError, Result};
use crate::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use crate::persistence_workspace::{PersistenceWorkspace, PersistenceWorkspaceConfig};
use crate::sample_ring::{RingConfig, SampleFormat};

/// Parameters for building the preallocated capture runtime (memory plan, dual
/// epoch arena, and persistence workspace) from a resolved sample rate and
/// channel count. Derived from legacy `LambConfig` or an app profile.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRuntimeParams {
    pub seconds: u32,
    pub chunk_frames_override: Option<u32>,
    pub memory_max: Option<u64>,
    pub headroom: f64,
    pub split_when_over_bytes: u64,
    pub io_buffer_bytes_per_channel: u64,
    pub maximum_path_bytes: u64,
    pub maximum_calibration_seconds: u32,
    pub capture_queue_slots: u32,
    pub capture_worker_stack_bytes: u64,
    pub control_queue_capacity: u32,
    pub worker_stack_bytes: u64,
}

pub const DEFAULT_CAPTURE_QUEUE_SLOTS: u32 = 64;
pub const DEFAULT_CONTROL_QUEUE_CAPACITY: u32 = 16;
pub const DEFAULT_WORKER_STACK_BYTES: u64 = 512 * 1024;
pub const DEFAULT_CAPTURE_WORKER_STACK_BYTES: u64 = 512 * 1024;
pub const DEFAULT_IO_BUFFER_BYTES_PER_CHANNEL: u64 = 64 * 1024;
pub const DEFAULT_MAXIMUM_PATH_BYTES: u64 = 4096;

/// The preallocated persistence runtime: one dual-epoch capture arena plus the
/// fixed persistence workspace. Both are fully allocated and page-touched by
/// construction, so persistence performs no recording-length heap allocation.
pub struct CaptureRuntime {
    pub arena: CaptureArena,
    pub workspace: PersistenceWorkspace,
    pub frozen_export_decision: FrozenExportDecision,
    calibration_sample_frames: u64,
}

struct PlannedCaptureRuntime {
    plan: SessionMemoryPlan,
    retention_frames: u64,
    chunk_frames: u32,
    chunk_count: u32,
}

fn plan_runtime(
    params: &CaptureRuntimeParams,
    sample_rate: u32,
    channels: u32,
) -> Result<PlannedCaptureRuntime> {
    let retention_frames = u64::from(params.seconds)
        .checked_mul(u64::from(sample_rate))
        .ok_or_else(|| LambError::Validation("retention frame count overflow".to_string()))?;
    let chunk_frames = crate::math::derive_chunk_frames(sample_rate, params.chunk_frames_override)?;
    let chunk_count = retention_frames.div_ceil(u64::from(chunk_frames)).max(1);
    let chunk_count = u32::try_from(chunk_count)
        .map_err(|_| LambError::Validation("chunk count exceeds u32".to_string()))?;
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames,
        channels,
        sample_rate,
        sample_format: SampleFormat::F32Le,
        chunk_frames,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: params.split_when_over_bytes,
        control_queue_capacity: params.control_queue_capacity,
        worker_stack_bytes: params.worker_stack_bytes,
        capture_queue_slots: params.capture_queue_slots,
        capture_slot_frames: chunk_frames,
        capture_worker_stack_bytes: params.capture_worker_stack_bytes,
        io_buffer_bytes_per_channel: params.io_buffer_bytes_per_channel,
        maximum_path_bytes: params.maximum_path_bytes,
        maximum_calibration_seconds: params.maximum_calibration_seconds,
        headroom: params.headroom,
    })?;
    plan.validate_max(params.memory_max)?;
    Ok(PlannedCaptureRuntime {
        plan,
        retention_frames,
        chunk_frames,
        chunk_count,
    })
}

impl CaptureRuntime {
    pub fn calibration_sample_frames(&self) -> u64 {
        self.calibration_sample_frames
    }

    pub(crate) fn validate_plan(
        params: CaptureRuntimeParams,
        sample_rate: u32,
        channels: u32,
    ) -> Result<()> {
        plan_runtime(&params, sample_rate, channels).map(|_| ())
    }

    pub fn build(
        params: CaptureRuntimeParams,
        sample_rate: u32,
        channels: u32,
    ) -> Result<(Self, CaptureIngress)> {
        let planned = plan_runtime(&params, sample_rate, channels)?;

        let (arena, ingress) = CaptureArena::new(
            &planned.plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels,
                    sample_rate,
                    format: SampleFormat::F32Le,
                    chunk_frames: planned.chunk_frames,
                    chunk_count: planned.chunk_count,
                    max_active_snapshots: 1,
                },
                queue_slots: params.capture_queue_slots,
                slot_frames: planned.chunk_frames,
                sample_bytes: 4,
                worker_stack_bytes: params.capture_worker_stack_bytes,
            },
        )?;
        let workspace = PersistenceWorkspace::new(
            &planned.plan,
            PersistenceWorkspaceConfig {
                retention_frames: planned.retention_frames,
                channels,
                sample_rate,
                sample_format: SampleFormat::F32Le,
                chunk_frames: planned.chunk_frames,
                sample_bytes: 4,
                split_when_over_bytes: params.split_when_over_bytes,
                io_buffer_bytes_per_channel: params.io_buffer_bytes_per_channel,
                maximum_path_bytes: params.maximum_path_bytes,
            },
        )?;
        let frozen_export_decision = FrozenExportDecision::new(&planned.plan)?;
        Ok((
            Self {
                arena,
                workspace,
                frozen_export_decision,
                calibration_sample_frames: planned.plan.calibration_sample_frames(),
            },
            ingress,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime_params() -> CaptureRuntimeParams {
        CaptureRuntimeParams {
            seconds: 30,
            chunk_frames_override: None,
            memory_max: None,
            headroom: 1.2,
            split_when_over_bytes: 1_073_741_824,
            io_buffer_bytes_per_channel: DEFAULT_IO_BUFFER_BYTES_PER_CHANNEL,
            maximum_path_bytes: DEFAULT_MAXIMUM_PATH_BYTES,
            maximum_calibration_seconds: 0,
            capture_queue_slots: DEFAULT_CAPTURE_QUEUE_SLOTS,
            capture_worker_stack_bytes: DEFAULT_CAPTURE_WORKER_STACK_BYTES,
            control_queue_capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
            worker_stack_bytes: DEFAULT_WORKER_STACK_BYTES,
        }
    }

    #[test]
    fn validate_plan_rejects_memory_limit_without_allocating() {
        let mut params = test_runtime_params();
        params.memory_max = Some(1);

        let error = CaptureRuntime::validate_plan(params, 48_000, 4).unwrap_err();
        assert!(error.to_string().contains("memory"));
    }

    #[test]
    fn validate_plan_and_build_accept_the_same_geometry() {
        let params = test_runtime_params();
        CaptureRuntime::validate_plan(params, 48_000, 4).unwrap();
        let (_runtime, _ingress) = CaptureRuntime::build(params, 48_000, 4).unwrap();
    }
}
