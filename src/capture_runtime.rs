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
}

impl CaptureRuntime {
    pub fn build(
        params: CaptureRuntimeParams,
        sample_rate: u32,
        channels: u32,
    ) -> Result<(Self, CaptureIngress)> {
        let retention_frames = u64::from(params.seconds)
            .checked_mul(u64::from(sample_rate))
            .ok_or_else(|| LambError::Validation("retention frame count overflow".to_string()))?;
        let chunk_frames =
            crate::math::derive_chunk_frames(sample_rate, params.chunk_frames_override)?;
        let chunk_count = retention_frames.div_ceil(u64::from(chunk_frames)).max(1);
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
            headroom: params.headroom,
        })?;
        plan.validate_max(params.memory_max)?;

        let (arena, ingress) = CaptureArena::new(
            &plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels,
                    sample_rate,
                    format: SampleFormat::F32Le,
                    chunk_frames,
                    chunk_count: u32::try_from(chunk_count).map_err(|_| {
                        LambError::Validation("chunk count exceeds u32".to_string())
                    })?,
                    max_active_snapshots: 1,
                },
                queue_slots: params.capture_queue_slots,
                slot_frames: chunk_frames,
                sample_bytes: 4,
                worker_stack_bytes: params.capture_worker_stack_bytes,
            },
        )?;
        let workspace = PersistenceWorkspace::new(
            &plan,
            PersistenceWorkspaceConfig {
                retention_frames,
                channels,
                sample_rate,
                sample_format: SampleFormat::F32Le,
                chunk_frames,
                sample_bytes: 4,
                split_when_over_bytes: params.split_when_over_bytes,
                io_buffer_bytes_per_channel: params.io_buffer_bytes_per_channel,
                maximum_path_bytes: params.maximum_path_bytes,
            },
        )?;
        let frozen_export_decision = FrozenExportDecision::new(&plan)?;
        Ok((
            Self {
                arena,
                workspace,
                frozen_export_decision,
            },
            ingress,
        ))
    }
}
