use crate::activity::{
    classify_frozen_epoch, ChannelDisposition, DetectorWorkspace, FrozenExportDecision,
};
use crate::capture_arena::FrozenCaptureEpoch;
use crate::dump::PublishedOutput;
use crate::error::{io_error, LambError, Result};
use crate::export_policy::{
    render_policy_output_into, validate_rendered_output_path, ExportCommand, PublicationStrategy,
    RenderContext, ResolvedExportPolicy,
};
use crate::export_wav::{
    f32_to_s24_bytes, validate_filename_component, write_wav_basename, WavBasename,
};
use crate::math::{wav_part_count, WAV_HEADER_BYTES};
use crate::memory_plan::{
    allocation_budget_bytes, ExactArray, MaterializedBuffer, SessionMemoryPlan,
    MANIFEST_DIRECTORY_METADATA_BYTES, MANIFEST_ENTRY_METADATA_BYTES, MANIFEST_FIXED_PATH_ENTRIES,
    MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES, MANIFEST_JSON_ENTRY_OVERHEAD_BYTES,
    MANIFEST_JSON_FIXED_OVERHEAD_BYTES, MANIFEST_PATH_ESCAPE_MULTIPLIER,
    OUTPUT_PATH_SLOTS_PER_PART, PUBLICATION_ARTIFACT_SLOT_BYTES, PUBLICATION_SYNC_SLOT_BYTES,
};
use crate::recovery::{
    recover_dump_parent, recover_dump_root, recover_recall_root_with_directories,
    recover_recall_staging_root_with_directories, FileIdentity, ManifestDirectorySlot,
    ManifestEntrySlot, PathRef, RecoveryOutcome, RecoveryScanSummary, TransactionKind,
};
use crate::sample_ring::SampleFormat;
use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const WAV_BYTES_PER_SAMPLE: u64 = 3;
const FIXED_PATH_SLOTS: usize = MANIFEST_FIXED_PATH_ENTRIES as usize;
const TRANSACTION_ROOT_PATH: usize = 0;
const STAGING_PARENT_PATH: usize = 1;
const FINAL_ROOT_PATH: usize = 2;
const PARTIAL_SCRATCH_PATH: usize = 3;
const MANIFEST_SCRATCH_PATH: usize = 4;
const OUTPUT_PATH_START: usize = FIXED_PATH_SLOTS;
const STAGED_PATH_OFFSET: usize = 0;
const FINAL_PATH_OFFSET: usize = 1;

static NEXT_WORKSPACE_TRANSACTION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistenceWorkspaceConfig {
    pub retention_frames: u64,
    pub channels: u32,
    pub sample_rate: u32,
    pub sample_format: SampleFormat,
    pub chunk_frames: u32,
    pub sample_bytes: u32,
    pub split_when_over_bytes: u64,
    pub io_buffer_bytes_per_channel: u64,
    pub maximum_path_bytes: u64,
}

pub enum PrepareRequest<'a> {
    /// Canonical policy-shaped preparation request.  The caller owns the frozen
    /// decision; a valid decision is deliberately reused on retries.
    Policy {
        command: ExportCommand,
        policy: &'a ResolvedExportPolicy,
        profile: &'a str,
        staging_root: &'a Path,
        timestamp: &'a str,
        decision: &'a mut FrozenExportDecision,
    },
    Recall {
        staging_root: &'a Path,
        output_dir: &'a Path,
        timestamp: &'a str,
        channel_names: &'a [String],
    },
    Dump {
        output_parent: &'a Path,
        timestamp: &'a str,
        channel_names: &'a [String],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistenceKind {
    Recall,
    Dump,
}

struct RequestDetails<'a> {
    kind: PersistenceKind,
    staging_parent: &'a Path,
    final_parent: &'a Path,
    timestamp: &'a str,
    channel_names: &'a [String],
    staging_prefix: &'static str,
}

impl<'a> From<PrepareRequest<'a>> for RequestDetails<'a> {
    fn from(request: PrepareRequest<'a>) -> Self {
        match request {
            PrepareRequest::Recall {
                staging_root,
                output_dir,
                timestamp,
                channel_names,
            } => Self {
                kind: PersistenceKind::Recall,
                staging_parent: staging_root,
                final_parent: output_dir,
                timestamp,
                channel_names,
                staging_prefix: "",
            },
            PrepareRequest::Dump {
                output_parent,
                timestamp,
                channel_names,
            } => Self {
                kind: PersistenceKind::Dump,
                staging_parent: output_parent,
                final_parent: output_parent,
                timestamp,
                channel_names,
                staging_prefix: ".tmp-lamb-",
            },
            PrepareRequest::Policy { .. } => {
                unreachable!("canonical policy requests are handled before legacy adaptation")
            }
        }
    }
}

pub trait WavIo {
    fn open(&mut self, path: &Path) -> io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn flush(&mut self, file: &mut File) -> io::Result<()>;
    fn sync_all(&mut self, file: &File) -> io::Result<()>;
}

pub trait CleanupIo: Send {
    fn symlink_metadata(&mut self, path: &Path) -> io::Result<fs::Metadata>;
    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }
}

struct FileCleanupIo;

impl CleanupIo for FileCleanupIo {
    fn symlink_metadata(&mut self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs::remove_dir_all(path)
    }
}

struct FileWavIo;

impl WavIo for FileWavIo {
    fn write_all(&mut self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn flush(&mut self, file: &mut File) -> io::Result<()> {
        file.flush()
    }

    fn sync_all(&mut self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceAllocationAddresses {
    scratch: usize,
    scratch_len: usize,
    detector_states: usize,
    detector_state_slots: usize,
    detector_scratch: usize,
    detector_scratch_len: usize,
    detector_decisions: usize,
    detector_decision_slots: usize,
    pcm_hash: usize,
    pcm_buffers: usize,
    pcm_buffer_len: usize,
    writers: usize,
    writer_slots: usize,
    outputs: usize,
    output_slots: usize,
    paths_hash: usize,
    path_slots: usize,
    path_capacity: usize,
    render_directory: usize,
    render_directory_capacity: usize,
    render_filename: usize,
    render_filename_capacity: usize,
    manifest_entries: usize,
    manifest_entry_slots: usize,
    manifest_directories: usize,
    manifest_directory_slots: usize,
    manifest_paths: usize,
    manifest_paths_len: usize,
    manifest_serialization: usize,
    manifest_serialization_len: usize,
    pub publication_sync_slots: usize,
    pub publication_sync_slot_count: usize,
    pub publication_current_artifact: usize,
    pub publication_current_artifact_slots: usize,
    pub publication_component_a: usize,
    pub publication_component_a_capacity: usize,
    pub publication_component_b: usize,
    pub publication_component_b_capacity: usize,
}

#[derive(Clone, Copy, Default)]
#[allow(dead_code)] // Reserved for allocation-free publication in the next integration step.
pub(crate) struct DirectorySyncSlot {
    pub entry_index: u32,
    pub prefix_len: u32,
    pub active: bool,
}

#[derive(Clone, Copy, Default)]
#[allow(dead_code)] // Reserved for allocation-free publication in the next integration step.
pub(crate) struct CurrentArtifactSlot {
    pub path: PathRef,
    pub identity: Option<FileIdentity>,
    pub final_name: bool,
}

const _: () =
    assert!(std::mem::size_of::<DirectorySyncSlot>() <= PUBLICATION_SYNC_SLOT_BYTES as usize);
const _: () =
    assert!(std::mem::size_of::<CurrentArtifactSlot>() <= PUBLICATION_ARTIFACT_SLOT_BYTES as usize);

pub(crate) struct PublicationScratch {
    sync_slots: ExactArray<DirectorySyncSlot>,
    current_artifact: ExactArray<CurrentArtifactSlot>,
    component_a: MaterializedBuffer<u8>,
    component_b: MaterializedBuffer<u8>,
    sync_count: usize,
}

#[derive(Clone, Copy)]
struct OutputFileSlot {
    start_frame: u64,
    frame_count: u64,
    channel: u32,
    part: u32,
    staged_path: u32,
    final_path: u32,
}

impl OutputFileSlot {
    const EMPTY: Self = Self {
        start_frame: 0,
        frame_count: 0,
        channel: 0,
        part: 0,
        staged_path: 0,
        final_path: 0,
    };
}

const _: () = assert!(std::mem::size_of::<OutputFileSlot>() <= 32);

struct WriterSlot {
    file: Option<File>,
    output_index: usize,
    frames_written: u64,
    output_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagingIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingCleanup {
    Unidentified {
        path_slot: u32,
    },
    Identified {
        path_slot: u32,
        device: u64,
        inode: u64,
    },
}

impl PendingCleanup {
    pub fn device(&self) -> Option<u64> {
        match self {
            Self::Unidentified { .. } => None,
            Self::Identified { device, .. } => Some(*device),
        }
    }

    pub fn inode(&self) -> Option<u64> {
        match self {
            Self::Unidentified { .. } => None,
            Self::Identified { inode, .. } => Some(*inode),
        }
    }

    fn path_slot(&self) -> usize {
        match self {
            Self::Unidentified { path_slot } | Self::Identified { path_slot, .. } => {
                *path_slot as usize
            }
        }
    }
}

impl WriterSlot {
    fn empty() -> Self {
        Self {
            file: None,
            output_index: 0,
            frames_written: 0,
            output_end: 0,
        }
    }
}

const _: () = assert!(std::mem::size_of::<WriterSlot>() <= 256);

struct ReusablePath {
    bytes: MaterializedBuffer<u8>,
}

impl ReusablePath {
    fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            bytes: MaterializedBuffer::new_zeroed(capacity)?,
        })
    }

    fn clear(&mut self) {
        if let Some(first) = self.bytes.as_mut_slice().first_mut() {
            *first = 0;
        }
    }

    fn len(&self) -> usize {
        self.bytes
            .as_slice()
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(self.bytes.len())
    }

    fn capacity(&self) -> usize {
        self.bytes.len()
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes.as_slice()[..self.len()]
    }

    fn as_path(&self) -> &Path {
        Path::new(std::ffi::OsStr::from_bytes(self.as_bytes()))
    }

    fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(self.as_bytes())
            .map_err(|_| LambError::ExportInvariant("workspace rendered path is not UTF-8"))
    }

    fn set_path(&mut self, path: &Path) -> Result<()> {
        self.clear();
        self.push_bytes(path.as_os_str().as_bytes())
    }

    fn push_separator(&mut self) -> Result<()> {
        let bytes = self.as_bytes();
        if !bytes.is_empty() && bytes.last() != Some(&b'/') {
            self.push_bytes(b"/")?;
        }
        Ok(())
    }

    fn push_bytes(&mut self, value: &[u8]) -> Result<()> {
        if value.contains(&0) {
            return Err(LambError::Export(
                "path contains an embedded NUL byte".to_string(),
            ));
        }
        let start = self.len();
        let end = start
            .checked_add(value.len())
            .ok_or(LambError::ExportInvariant("workspace path length overflow"))?;
        if end > self.capacity() {
            return Err(LambError::Export(format!(
                "path requires {end} bytes but workspace capacity is {}",
                self.capacity()
            )));
        }
        self.bytes.as_mut_slice()[start..end].copy_from_slice(value);
        if end < self.capacity() {
            self.bytes.as_mut_slice()[end] = 0;
        }
        Ok(())
    }
}

impl fmt::Write for ReusablePath {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.push_bytes(value.as_bytes()).map_err(|_| fmt::Error)
    }
}

const _: () = assert!(std::mem::size_of::<ReusablePath>() <= 32);
const _: () = assert!(
    std::mem::size_of::<ManifestEntrySlot>() <= MANIFEST_ENTRY_METADATA_BYTES as usize,
    "ManifestEntrySlot must fit the reserved per-entry metadata budget"
);
const _: () = assert!(
    std::mem::size_of::<ManifestDirectorySlot>() <= MANIFEST_DIRECTORY_METADATA_BYTES as usize,
    "ManifestDirectorySlot must fit the reserved per-directory metadata budget"
);

pub struct PersistenceWorkspace {
    config: PersistenceWorkspaceConfig,
    interleaved_scratch: MaterializedBuffer<f32>,
    detector: DetectorWorkspace,
    channel_pcm: ExactArray<MaterializedBuffer<u8>>,
    writers: ExactArray<WriterSlot>,
    outputs: ExactArray<OutputFileSlot>,
    paths: ExactArray<ReusablePath>,
    render_directory: ReusablePath,
    render_filename: ReusablePath,
    manifest_entries: ExactArray<ManifestEntrySlot>,
    manifest_directories: ExactArray<ManifestDirectorySlot>,
    manifest_paths: MaterializedBuffer<u8>,
    manifest_serialization: MaterializedBuffer<u8>,
    publication: PublicationScratch,
    output_count: usize,
    staging_identity: Option<StagingIdentity>,
    pending_cleanup: Option<PendingCleanup>,
    completed_publication: Option<CompletedPublicationCleanup>,
    cleanup_io: Box<dyn CleanupIo>,
}

#[derive(Clone, Copy)]
struct CompletedPublicationCleanup {
    kind: PersistenceKind,
}

impl PersistenceWorkspace {
    pub fn new(plan: &SessionMemoryPlan, config: PersistenceWorkspaceConfig) -> Result<Self> {
        let validation = config;
        Self::allocate_validated(plan, &validation, || {
            Self::allocate(plan, config, Box::new(FileCleanupIo))
        })
    }

    pub fn new_with_cleanup_io(
        plan: &SessionMemoryPlan,
        config: PersistenceWorkspaceConfig,
        cleanup_io: Box<dyn CleanupIo>,
    ) -> Result<Self> {
        let validation = config;
        Self::allocate_validated(plan, &validation, || {
            Self::allocate(plan, config, cleanup_io)
        })
    }

    pub fn allocate_validated<T>(
        plan: &SessionMemoryPlan,
        config: &PersistenceWorkspaceConfig,
        allocate: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        validate_workspace_plan(plan, config)?;
        allocate()
    }

    fn allocate(
        plan: &SessionMemoryPlan,
        config: PersistenceWorkspaceConfig,
        cleanup_io: Box<dyn CleanupIo>,
    ) -> Result<Self> {
        let channels = usize::try_from(config.channels)
            .map_err(|_| LambError::Validation("workspace channel count overflow".to_string()))?;
        let scratch_len = usize::try_from(
            u64::from(config.chunk_frames)
                .checked_mul(u64::from(config.channels))
                .ok_or_else(|| {
                    LambError::Validation("workspace scratch length overflow".to_string())
                })?,
        )
        .map_err(|_| LambError::Validation("workspace scratch length overflow".to_string()))?;
        let pcm_len = usize::try_from(config.io_buffer_bytes_per_channel).map_err(|_| {
            LambError::Validation("workspace PCM buffer length overflow".to_string())
        })?;
        let output_slots = usize::try_from(plan.output_file_slots()).map_err(|_| {
            LambError::Validation("workspace output slot count overflow".to_string())
        })?;
        let path_slots = usize::try_from(plan.path_slots())
            .map_err(|_| LambError::Validation("workspace path slot count overflow".to_string()))?;
        let path_capacity = usize::try_from(config.maximum_path_bytes)
            .map_err(|_| LambError::Validation("workspace path capacity overflow".to_string()))?;
        let directory_slots = usize::try_from(plan.manifest_directory_slots()).map_err(|_| {
            LambError::Validation("workspace directory slot count overflow".to_string())
        })?;
        let publication_component_len = config
            .maximum_path_bytes
            .checked_add(1)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| {
                LambError::Validation("publication component buffer capacity overflow".to_string())
            })?;
        let escaped_path_bytes = config
            .maximum_path_bytes
            .checked_mul(MANIFEST_PATH_ESCAPE_MULTIPLIER)
            .ok_or_else(|| {
                LambError::Validation("manifest escaped path capacity overflow".to_string())
            })?;
        let manifest_entry_bytes = escaped_path_bytes
            .checked_add(MANIFEST_JSON_ENTRY_OVERHEAD_BYTES)
            .ok_or_else(|| LambError::Validation("manifest entry capacity overflow".to_string()))?;
        let manifest_serialization_len = u64::try_from(output_slots)
            .ok()
            .and_then(|slots| slots.checked_mul(3))
            .and_then(|slots| slots.checked_add(MANIFEST_FIXED_PATH_ENTRIES))
            .and_then(|slots| slots.checked_mul(manifest_entry_bytes))
            .and_then(|entries| {
                u64::try_from(directory_slots).ok().and_then(|dirs| {
                    dirs.checked_mul(MANIFEST_JSON_DIRECTORY_OVERHEAD_BYTES)
                        .and_then(|directory_bytes| entries.checked_add(directory_bytes))
                })
            })
            .and_then(|entries| entries.checked_add(MANIFEST_JSON_FIXED_OVERHEAD_BYTES))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| {
                LambError::Validation("manifest serialization capacity overflow".to_string())
            })?;
        let manifest_paths_len = u64::try_from(output_slots)
            .ok()
            .and_then(|slots| slots.checked_mul(3))
            .and_then(|slots| slots.checked_add(MANIFEST_FIXED_PATH_ENTRIES))
            .and_then(|slots| slots.checked_mul(config.maximum_path_bytes))
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| {
                LambError::Validation("manifest path arena capacity overflow".to_string())
            })?;
        // `SessionMemoryPlan` used the identical checked formula before any
        // allocation is attempted.  The concrete usize conversion above is
        // the second overflow boundary for this platform.

        Ok(Self {
            config,
            interleaved_scratch: MaterializedBuffer::new_zeroed(scratch_len)?,
            detector: DetectorWorkspace::new(plan)?,
            channel_pcm: ExactArray::try_from_fn(channels, |_| {
                MaterializedBuffer::new_zeroed(pcm_len)
            })?,
            writers: ExactArray::try_from_fn(channels, |_| Ok(WriterSlot::empty()))?,
            outputs: ExactArray::try_from_fn(output_slots, |_| Ok(OutputFileSlot::EMPTY))?,
            paths: ExactArray::try_from_fn(path_slots, |_| ReusablePath::new(path_capacity))?,
            render_directory: ReusablePath::new(path_capacity)?,
            render_filename: ReusablePath::new(path_capacity)?,
            manifest_entries: ExactArray::try_from_fn(output_slots, |_| {
                Ok(ManifestEntrySlot::default())
            })?,
            manifest_directories: ExactArray::try_from_fn(directory_slots, |_| {
                Ok(ManifestDirectorySlot::default())
            })?,
            manifest_paths: MaterializedBuffer::new_zeroed(manifest_paths_len)?,
            manifest_serialization: MaterializedBuffer::new_zeroed(manifest_serialization_len)?,
            publication: PublicationScratch {
                sync_slots: ExactArray::try_from_fn(directory_slots, |_| {
                    Ok(DirectorySyncSlot::default())
                })?,
                current_artifact: ExactArray::try_from_fn(1, |_| {
                    Ok(CurrentArtifactSlot::default())
                })?,
                component_a: MaterializedBuffer::new_zeroed(publication_component_len)?,
                component_b: MaterializedBuffer::new_zeroed(publication_component_len)?,
                sync_count: 0,
            },
            output_count: 0,
            staging_identity: None,
            pending_cleanup: None,
            completed_publication: None,
            cleanup_io,
        })
    }

    pub fn allocation_addresses(&self) -> WorkspaceAllocationAddresses {
        let (
            detector_states,
            detector_state_slots,
            detector_scratch,
            detector_scratch_len,
            detector_decisions,
            detector_decision_slots,
        ) = self.detector.allocation_addresses();
        let pcm_hash = self.channel_pcm.iter().fold(0_usize, |hash, buffer| {
            mix_address(hash, buffer.as_slice().as_ptr() as usize, buffer.len())
        });
        let paths_hash = self.paths.iter().fold(0_usize, |hash, path| {
            mix_address(
                hash,
                path.bytes.as_slice().as_ptr() as usize,
                path.capacity(),
            )
        });
        WorkspaceAllocationAddresses {
            scratch: self.interleaved_scratch.as_slice().as_ptr() as usize,
            scratch_len: self.interleaved_scratch.len(),
            detector_states,
            detector_state_slots,
            detector_scratch,
            detector_scratch_len,
            detector_decisions,
            detector_decision_slots,
            pcm_hash,
            pcm_buffers: self.channel_pcm.len(),
            pcm_buffer_len: self.channel_pcm.first().map_or(0, |buffer| buffer.len()),
            writers: self.writers.as_slice().as_ptr() as usize,
            writer_slots: self.writers.len(),
            outputs: self.outputs.as_slice().as_ptr() as usize,
            output_slots: self.outputs.len(),
            paths_hash,
            path_slots: self.paths.len(),
            path_capacity: self.paths.first().map_or(0, ReusablePath::capacity),
            render_directory: self.render_directory.bytes.as_slice().as_ptr() as usize,
            render_directory_capacity: self.render_directory.capacity(),
            render_filename: self.render_filename.bytes.as_slice().as_ptr() as usize,
            render_filename_capacity: self.render_filename.capacity(),
            manifest_entries: self.manifest_entries.as_slice().as_ptr() as usize,
            manifest_entry_slots: self.manifest_entries.len(),
            manifest_directories: self.manifest_directories.as_slice().as_ptr() as usize,
            manifest_directory_slots: self.manifest_directories.len(),
            manifest_paths: self.manifest_paths.as_slice().as_ptr() as usize,
            manifest_paths_len: self.manifest_paths.len(),
            manifest_serialization: self.manifest_serialization.as_slice().as_ptr() as usize,
            manifest_serialization_len: self.manifest_serialization.len(),
            publication_sync_slots: self.publication.sync_slots.as_slice().as_ptr() as usize,
            publication_sync_slot_count: self.publication.sync_slots.len(),
            publication_current_artifact: self.publication.current_artifact.as_slice().as_ptr()
                as usize,
            publication_current_artifact_slots: self.publication.current_artifact.len(),
            publication_component_a: self.publication.component_a.as_slice().as_ptr() as usize,
            publication_component_a_capacity: self.publication.component_a.len(),
            publication_component_b: self.publication.component_b.as_slice().as_ptr() as usize,
            publication_component_b_capacity: self.publication.component_b.len(),
        }
    }

    pub fn prepare<'a>(
        &'a mut self,
        frozen: &FrozenCaptureEpoch,
        request: PrepareRequest<'_>,
    ) -> Result<PreparedPersistence<'a>> {
        let mut io = FileWavIo;
        self.prepare_with_io(frozen, request, &mut io)
    }

    pub fn prepare_with_io<'a>(
        &'a mut self,
        frozen: &FrozenCaptureEpoch,
        request: PrepareRequest<'_>,
        io: &mut impl WavIo,
    ) -> Result<PreparedPersistence<'a>> {
        self.finish_completed_publication();
        self.retry_pending_cleanup()?;
        self.reset_slots();
        validate_frozen(&self.config, frozen)?;
        let details = match request {
            PrepareRequest::Policy {
                command,
                policy,
                profile,
                staging_root,
                timestamp,
                decision,
            } => {
                if staging_root.to_str().is_none() {
                    return Err(LambError::Validation(
                        "export staging root must be valid UTF-8".to_string(),
                    ));
                }
                validate_policy_geometry(&self.config, frozen, policy, decision)?;
                if decision.valid() && !decision.matches_frozen_epoch(frozen) {
                    return Err(LambError::ExportInvariant(
                        "frozen export decision does not match frozen epoch",
                    ));
                }
                if !decision.valid() {
                    classify_frozen_epoch(frozen, &policy.activity, &mut self.detector, decision)?;
                }
                let all_never = decision.channels().iter().all(|channel| {
                    matches!(channel.mode, crate::activity::ChannelExportMode::Never)
                });
                if all_never {
                    return Ok(PreparedPersistence::SkippedByPolicy);
                }
                if !decision
                    .channels()
                    .iter()
                    .any(|channel| channel.disposition == ChannelDisposition::Retain)
                {
                    return Ok(PreparedPersistence::SkippedSilent);
                }
                let kind = match policy.layout().publication_strategy(command) {
                    PublicationStrategy::FileSet => PersistenceKind::Recall,
                    PublicationStrategy::AtomicDirectory => PersistenceKind::Dump,
                };
                let policy_staging_parent = if kind == PersistenceKind::Dump {
                    policy.output_dir()
                } else {
                    staging_root
                };
                let details = RequestDetails {
                    kind,
                    staging_parent: policy_staging_parent,
                    final_parent: policy.output_dir(),
                    timestamp,
                    channel_names: &[],
                    staging_prefix: if kind == PersistenceKind::Dump {
                        ".tmp-lamb-"
                    } else {
                        ""
                    },
                };
                let context = RenderContext {
                    command,
                    profile,
                    timestamp,
                    sample_rate: self.config.sample_rate,
                    export_start_frame: decision.export_range().start,
                    export_end_frame: decision.export_range().end,
                    split_when_over_bytes: self.config.split_when_over_bytes,
                    maximum_path_bytes: self.config.maximum_path_bytes,
                };
                self.plan_policy_paths(&details, policy, &context, decision)?;
                if let Err(error) = self.preflight_planned_finals(kind) {
                    self.reset_slots();
                    return Err(error);
                }
                if kind == PersistenceKind::Dump {
                    fs::create_dir_all(staging_root)
                        .map_err(|source| io_error(staging_root, source))?;
                }
                return self.finish_policy_prepare(
                    frozen,
                    decision.export_range(),
                    kind,
                    policy_staging_parent,
                    io,
                );
            }
            legacy => RequestDetails::from(legacy),
        };
        let kind = details.kind;
        if let Err(error) = self.plan_paths(frozen.total_frames(), &details) {
            self.reset_slots();
            return Err(error);
        }
        if let Err(error) = self.preflight_planned_finals(kind) {
            self.reset_slots();
            return Err(error);
        }
        if let Err(error) = self.create_staging(&details) {
            if self.pending_cleanup.is_none() {
                self.reset_slots();
            }
            return Err(error);
        }
        let any_nonzero = match self.stream_wavs(frozen, io) {
            Ok(any_nonzero) => any_nonzero,
            Err(error) => {
                return Err(self.error_after_cleanup(error));
            }
        };
        if !any_nonzero {
            self.cleanup_transaction()?;
            return Ok(PreparedPersistence::Silent);
        }

        let staging = OwnedTransactionArtifacts { workspace: self };
        Ok(match kind {
            PersistenceKind::Recall => PreparedPersistence::Recall { staging },
            PersistenceKind::Dump => PreparedPersistence::Dump { staging },
        })
    }

    fn finish_policy_prepare<'a>(
        &'a mut self,
        frozen: &FrozenCaptureEpoch,
        export_range: std::ops::Range<u64>,
        kind: PersistenceKind,
        staging_parent: &Path,
        io: &mut impl WavIo,
    ) -> Result<PreparedPersistence<'a>> {
        if let Err(error) = self.create_staging(&RequestDetails {
            kind,
            staging_parent,
            final_parent: staging_parent,
            timestamp: "",
            channel_names: &[],
            staging_prefix: if kind == PersistenceKind::Dump {
                ".tmp-lamb-"
            } else {
                ""
            },
        }) {
            if self.pending_cleanup.is_none() {
                self.reset_slots();
            }
            return Err(error);
        }
        if let Err(error) = self.stream_selected_wavs(frozen, export_range, io) {
            return Err(self.error_after_cleanup(error));
        }
        let staging = OwnedTransactionArtifacts { workspace: self };
        Ok(match kind {
            PersistenceKind::Recall => PreparedPersistence::FileSet { staging },
            PersistenceKind::Dump => PreparedPersistence::AtomicDirectory { staging },
        })
    }

    fn preflight_planned_finals(&self, kind: PersistenceKind) -> Result<()> {
        match kind {
            PersistenceKind::Recall => {
                for index in 0..self.output_count {
                    let final_path = self.paths[self.outputs[index].final_path as usize].as_path();
                    preflight_absent_final_chain(
                        final_path,
                        "prepared file-set final path already exists",
                    )?;
                }
            }
            PersistenceKind::Dump => {
                if self.output_count == 0 {
                    return Err(LambError::ExportInvariant(
                        "prepared atomic plan has no outputs",
                    ));
                }
                let final_path = self.paths[self.outputs[0].final_path as usize].as_path();
                let final_directory = final_path.parent().ok_or(LambError::ExportInvariant(
                    "prepared atomic final path has no parent",
                ))?;
                preflight_absent_final_chain(
                    final_directory,
                    "prepared atomic final directory already exists",
                )?;
            }
        }
        Ok(())
    }

    fn plan_policy_paths(
        &mut self,
        details: &RequestDetails<'_>,
        policy: &ResolvedExportPolicy,
        context: &RenderContext<'_>,
        decision: &FrozenExportDecision,
    ) -> Result<()> {
        let part_count = wav_part_count(
            context.export_end_frame - context.export_start_frame,
            WAV_BYTES_PER_SAMPLE as u32,
            self.config.split_when_over_bytes,
        )?;
        let retained = decision
            .channels()
            .iter()
            .filter(|channel| channel.disposition == ChannelDisposition::Retain)
            .count();
        let output_count = retained
            .checked_mul(
                usize::try_from(part_count)
                    .map_err(|_| LambError::ExportInvariant("workspace part count overflow"))?,
            )
            .ok_or(LambError::ExportInvariant(
                "selected range output count overflow",
            ))?;
        if output_count > self.outputs.len() {
            return Err(LambError::ExportInvariant(
                "selected range exceeds workspace output slots",
            ));
        }
        self.output_count = output_count;
        self.paths[STAGING_PARENT_PATH].set_path(details.staging_parent)?;
        self.paths[FINAL_ROOT_PATH].set_path(policy.output_dir())?;
        set_transaction_path(
            &mut self.paths[TRANSACTION_ROOT_PATH],
            details.staging_parent,
            details.staging_prefix,
        )?;
        let mut index = 0usize;
        for (channel_index, channel) in decision.channels().iter().enumerate() {
            if channel.disposition != ChannelDisposition::Retain {
                continue;
            }
            let name = policy
                .activity
                .channels
                .get(channel_index)
                .ok_or(LambError::ExportInvariant(
                    "decision channel is absent from policy",
                ))?
                .name
                .as_str();
            for zero_part in 0..part_count {
                let part = zero_part
                    .checked_add(1)
                    .ok_or(LambError::ExportInvariant("workspace part overflow"))?;
                let frames_per_part =
                    (self.config.split_when_over_bytes - WAV_HEADER_BYTES) / WAV_BYTES_PER_SAMPLE;
                let part_range = absolute_split_range(
                    context.export_start_frame..context.export_end_frame,
                    zero_part,
                    frames_per_part,
                )?;
                let start_frame = part_range.start;
                let end_frame = part_range.end;
                let mut suffix = [0_u8; 24];
                let part_suffix = if part_count == 1 {
                    ""
                } else {
                    suffix[..5].copy_from_slice(b"-part");
                    let mut value = part;
                    let mut digits = 1usize;
                    while value >= 10 {
                        value /= 10;
                        digits += 1;
                    }
                    let width = digits.max(3);
                    value = part;
                    for position in (0..width).rev() {
                        suffix[5 + position] = b'0' + (value % 10) as u8;
                        value /= 10;
                    }
                    std::str::from_utf8(&suffix[..5 + width])
                        .map_err(|_| LambError::ExportInvariant("part suffix is not UTF-8"))?
                };
                let staged_index = index * OUTPUT_PATH_SLOTS_PER_PART as usize;
                self.render_directory.clear();
                self.render_filename.clear();
                render_policy_output_into(
                    policy,
                    context,
                    name,
                    part,
                    part_suffix,
                    start_frame,
                    end_frame,
                    &mut self.render_directory,
                    &mut self.render_filename,
                )?;
                let rendered_directory = self.render_directory.as_str()?;
                let rendered_filename = self.render_filename.as_str()?;
                let final_index = OUTPUT_PATH_START + staged_index + FINAL_PATH_OFFSET;
                self.paths[final_index].set_path(policy.output_dir())?;
                if !rendered_directory.is_empty() {
                    self.paths[final_index].push_separator()?;
                    self.paths[final_index].push_bytes(rendered_directory.as_bytes())?;
                }
                self.paths[final_index].push_separator()?;
                self.paths[final_index].push_bytes(rendered_filename.as_bytes())?;
                validate_rendered_output_path(
                    policy.output_dir(),
                    rendered_directory,
                    rendered_filename,
                    self.paths[final_index].as_path(),
                    context.maximum_path_bytes,
                )?;
                let staged_path = OUTPUT_PATH_START + staged_index + STAGED_PATH_OFFSET;
                if details.kind == PersistenceKind::Recall {
                    self.render_directory
                        .set_path(self.paths[TRANSACTION_ROOT_PATH].as_path())?;
                    self.paths[staged_path].set_path(self.render_directory.as_path())?;
                    self.paths[staged_path].push_separator()?;
                    write!(&mut self.paths[staged_path], "output-{index:08}").map_err(|_| {
                        LambError::ExportInvariant("staged name exceeds workspace capacity")
                    })?;
                }
                self.outputs[index] = OutputFileSlot {
                    start_frame,
                    frame_count: end_frame - start_frame,
                    channel: channel_index as u32,
                    part: zero_part as u32,
                    staged_path: staged_path as u32,
                    final_path: final_index as u32,
                };
                for prior in 0..index {
                    let previous = self.paths[self.outputs[prior].final_path as usize].as_bytes();
                    let current = self.paths[final_index].as_bytes();
                    if previous == current {
                        return Err(LambError::Validation(
                            "duplicate rendered export path".to_string(),
                        ));
                    }
                    if (current.starts_with(previous) && current.get(previous.len()) == Some(&b'/'))
                        || (previous.starts_with(current)
                            && previous.get(current.len()) == Some(&b'/'))
                    {
                        return Err(LambError::Validation(
                            "rendered export file/parent path conflict".to_string(),
                        ));
                    }
                }
                index += 1;
            }
        }
        if details.kind == PersistenceKind::Dump {
            self.plan_atomic_staged_paths()?;
        }
        Ok(())
    }

    fn plan_atomic_staged_paths(&mut self) -> Result<()> {
        let first_final = self
            .outputs
            .first()
            .filter(|_| self.output_count != 0)
            .ok_or(LambError::ExportInvariant(
                "atomic directory has no planned outputs",
            ))?
            .final_path as usize;
        let first_parent =
            self.paths[first_final]
                .as_path()
                .parent()
                .ok_or(LambError::ExportInvariant(
                    "atomic final path has no parent",
                ))?;
        self.render_filename.set_path(first_parent)?;
        self.paths[FINAL_ROOT_PATH].set_path(self.render_filename.as_path())?;
        self.render_directory
            .set_path(self.paths[TRANSACTION_ROOT_PATH].as_path())?;

        for index in 0..self.output_count {
            let final_path = self.outputs[index].final_path as usize;
            let staged_path = self.outputs[index].staged_path as usize;
            let final_file = self.paths[final_path].as_path();
            if final_file.parent() != Some(self.paths[FINAL_ROOT_PATH].as_path()) {
                return Err(LambError::ExportInvariant(
                    "atomic final files must share one direct parent",
                ));
            }
            let basename = final_file
                .file_name()
                .ok_or(LambError::ExportInvariant(
                    "atomic final path must name a direct-child file",
                ))?
                .as_bytes();
            self.render_filename.clear();
            self.render_filename.push_bytes(basename)?;
            self.paths[staged_path].set_path(self.render_directory.as_path())?;
            self.paths[staged_path].push_separator()?;
            self.paths[staged_path].push_bytes(self.render_filename.as_bytes())?;
        }
        Ok(())
    }

    fn stream_selected_wavs(
        &mut self,
        frozen: &FrozenCaptureEpoch,
        export_range: std::ops::Range<u64>,
        io: &mut impl WavIo,
    ) -> Result<()> {
        let mut next = 0usize;
        for channel in 0..self.writers.len() {
            if next == self.output_count || self.outputs[next].channel as usize != channel {
                continue;
            }
            let first = next;
            while next < self.output_count && self.outputs[next].channel as usize == channel {
                next += 1;
            }
            open_writer(
                &mut self.writers[channel],
                first,
                next,
                &self.outputs,
                &self.paths,
                self.config.sample_rate,
                io,
            )?;
        }
        let channels = self.config.channels as usize;
        let mut cursor = export_range.start;
        while cursor < export_range.end {
            let copied = frozen.copy_interleaved_range_into(
                cursor..export_range.end,
                self.interleaved_scratch.as_mut_slice(),
            )? as usize;
            if copied == 0 {
                return Err(LambError::ExportInvariant("frozen read made no progress"));
            }
            let samples = &self.interleaved_scratch.as_slice()[..copied * channels];
            for channel in 0..self.writers.len() {
                if self.writers[channel].file.is_none() {
                    continue;
                }
                write_channel_block(
                    samples,
                    copied,
                    channels,
                    channel,
                    &mut self.channel_pcm[channel],
                    &mut self.writers[channel],
                    &self.outputs,
                    &self.paths,
                    self.config.sample_rate,
                    0,
                    io,
                )?;
            }
            cursor += copied as u64;
        }
        for channel in 0..self.writers.len() {
            if self.writers[channel].file.is_none() {
                continue;
            }
            finalize_writer(&mut self.writers[channel], &self.outputs, &self.paths, io)?;
        }
        Ok(())
    }

    fn plan_paths(&mut self, total_frames: u64, details: &RequestDetails<'_>) -> Result<()> {
        if details.kind == PersistenceKind::Dump {
            validate_filename_component(details.timestamp, "dump timestamp")?;
        }
        let parts_per_channel = wav_part_count(
            total_frames,
            WAV_BYTES_PER_SAMPLE as u32,
            self.config.split_when_over_bytes,
        )?;
        let output_count = parts_per_channel
            .checked_mul(u64::from(self.config.channels))
            .and_then(|count| usize::try_from(count).ok())
            .ok_or(LambError::ExportInvariant(
                "workspace output count overflow",
            ))?;
        if output_count > self.outputs.len() {
            return Err(LambError::ExportInvariant(
                "selected range exceeds workspace output slots",
            ));
        }
        self.output_count = output_count;

        self.paths[STAGING_PARENT_PATH].set_path(details.staging_parent)?;
        self.paths[FINAL_ROOT_PATH].set_path(details.final_parent)?;
        if details.kind == PersistenceKind::Dump {
            self.paths[FINAL_ROOT_PATH].push_separator()?;
            self.paths[FINAL_ROOT_PATH].push_bytes(details.timestamp.as_bytes())?;
        }
        set_transaction_path(
            &mut self.paths[TRANSACTION_ROOT_PATH],
            details.staging_parent,
            details.staging_prefix,
        )?;

        let frames_per_part =
            (self.config.split_when_over_bytes - WAV_HEADER_BYTES) / WAV_BYTES_PER_SAMPLE;
        let parts_per_channel_usize = usize::try_from(parts_per_channel)
            .map_err(|_| LambError::ExportInvariant("workspace part index overflow"))?;
        let (fixed_paths, output_paths) = self.paths.as_mut_slice().split_at_mut(OUTPUT_PATH_START);
        let transaction_root = &fixed_paths[TRANSACTION_ROOT_PATH];
        let final_root = &fixed_paths[FINAL_ROOT_PATH];
        for channel in 0..usize::try_from(self.config.channels).unwrap_or(0) {
            for part_index in 0..parts_per_channel {
                let output_index = channel
                    .checked_mul(parts_per_channel_usize)
                    .and_then(|index| index.checked_add(part_index as usize))
                    .ok_or(LambError::ExportInvariant(
                        "workspace output index overflow",
                    ))?;
                let start_frame = part_index
                    .checked_mul(frames_per_part)
                    .ok_or(LambError::ExportInvariant("WAV part frame offset overflow"))?;
                let frame_count = (total_frames - start_frame).min(frames_per_part);
                let staged_index = output_index * OUTPUT_PATH_SLOTS_PER_PART as usize;
                let final_index = staged_index + FINAL_PATH_OFFSET;
                write_output_path(
                    &mut output_paths[staged_index + STAGED_PATH_OFFSET],
                    transaction_root.as_path(),
                    WavBasename {
                        simple_names: details.kind == PersistenceKind::Dump,
                        timestamp: details.timestamp,
                        channel_name: details.channel_names.get(channel).map(String::as_str),
                        channel_index: channel,
                        sample_rate: self.config.sample_rate,
                        part_count: parts_per_channel,
                        part_index,
                        start_frame,
                        frame_count,
                    },
                )?;
                write_output_path(
                    &mut output_paths[final_index],
                    final_root.as_path(),
                    WavBasename {
                        simple_names: details.kind == PersistenceKind::Dump,
                        timestamp: details.timestamp,
                        channel_name: details.channel_names.get(channel).map(String::as_str),
                        channel_index: channel,
                        sample_rate: self.config.sample_rate,
                        part_count: parts_per_channel,
                        part_index,
                        start_frame,
                        frame_count,
                    },
                )?;
                for prior_channel in 0..channel {
                    let prior_output = prior_channel * parts_per_channel_usize
                        + usize::try_from(part_index).map_err(|_| {
                            LambError::ExportInvariant("workspace part index overflow")
                        })?;
                    let prior_path =
                        prior_output * OUTPUT_PATH_SLOTS_PER_PART as usize + FINAL_PATH_OFFSET;
                    if output_paths[prior_path].as_bytes() == output_paths[final_index].as_bytes() {
                        return Err(LambError::Export(format!(
                            "duplicate WAV filename: {}",
                            output_paths[final_index]
                                .as_path()
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        )));
                    }
                }
                self.outputs[output_index] = OutputFileSlot {
                    start_frame,
                    frame_count,
                    channel: channel as u32,
                    part: part_index as u32,
                    staged_path: (OUTPUT_PATH_START + staged_index) as u32,
                    final_path: (OUTPUT_PATH_START + final_index) as u32,
                };
            }
        }
        Ok(())
    }

    fn create_staging(&mut self, details: &RequestDetails<'_>) -> Result<()> {
        fs::create_dir_all(details.staging_parent)
            .map_err(|source| io_error(details.staging_parent, source))?;
        loop {
            let path = self.paths[TRANSACTION_ROOT_PATH].as_path();
            match fs::create_dir(path) {
                Ok(()) => {
                    self.pending_cleanup = Some(PendingCleanup::Unidentified {
                        path_slot: TRANSACTION_ROOT_PATH as u32,
                    });
                    return match self.cleanup_io.symlink_metadata(path) {
                        Ok(metadata) => {
                            self.staging_identity = Some(StagingIdentity {
                                device: metadata.dev(),
                                inode: metadata.ino(),
                            });
                            self.pending_cleanup = None;
                            Ok(())
                        }
                        Err(identity_source) => {
                            let identity_error = io_error(path, identity_source);
                            match self.cleanup_io.remove_dir_all(path) {
                                Ok(()) => {
                                    self.pending_cleanup = None;
                                    Err(identity_error)
                                }
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                    self.pending_cleanup = None;
                                    Err(identity_error)
                                }
                                Err(removal_source) => Err(LambError::PersistenceCleanup {
                                    operation: Box::new(identity_error),
                                    cleanup: Box::new(io_error(path, removal_source)),
                                }),
                            }
                        }
                    };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    set_transaction_path(
                        &mut self.paths[TRANSACTION_ROOT_PATH],
                        details.staging_parent,
                        details.staging_prefix,
                    )?;
                    self.refresh_staged_paths()?;
                }
                Err(error) => return Err(io_error(path, error)),
            }
        }
    }

    fn refresh_staged_paths(&mut self) -> Result<()> {
        let (fixed_paths, output_paths) = self.paths.as_mut_slice().split_at_mut(OUTPUT_PATH_START);
        let transaction_root = fixed_paths[TRANSACTION_ROOT_PATH].as_path();
        for output_index in 0..self.output_count {
            let staged_index = output_index * OUTPUT_PATH_SLOTS_PER_PART as usize;
            let final_index = staged_index + FINAL_PATH_OFFSET;
            let (before_final, final_and_after) = output_paths.split_at_mut(final_index);
            let staged = &mut before_final[staged_index];
            let basename = final_and_after[0]
                .as_path()
                .file_name()
                .ok_or(LambError::ExportInvariant(
                    "planned WAV path has no basename",
                ))?
                .as_bytes();
            staged.set_path(transaction_root)?;
            staged.push_separator()?;
            staged.push_bytes(basename)?;
        }
        Ok(())
    }

    fn stream_wavs(&mut self, frozen: &FrozenCaptureEpoch, io: &mut impl WavIo) -> Result<bool> {
        let parts_per_channel = usize::try_from(wav_part_count(
            frozen.total_frames(),
            WAV_BYTES_PER_SAMPLE as u32,
            self.config.split_when_over_bytes,
        )?)
        .map_err(|_| LambError::ExportInvariant("workspace part count overflow"))?;
        for channel in 0..self.writers.len() {
            let output_index =
                channel
                    .checked_mul(parts_per_channel)
                    .ok_or(LambError::ExportInvariant(
                        "workspace writer index overflow",
                    ))?;
            open_writer(
                &mut self.writers[channel],
                output_index,
                output_index + parts_per_channel,
                &self.outputs,
                &self.paths,
                self.config.sample_rate,
                io,
            )?;
        }

        let mut cursor = frozen.absolute_range().start;
        let end = frozen.absolute_range().end;
        let channels = usize::try_from(self.config.channels)
            .map_err(|_| LambError::ExportInvariant("workspace channel count overflow"))?;
        let mut any_nonzero = false;
        while cursor < end {
            let copied = frozen.copy_interleaved_range_into(
                cursor..end,
                self.interleaved_scratch.as_mut_slice(),
            )?;
            if copied == 0 {
                return Err(LambError::ExportInvariant("frozen read made no progress"));
            }
            let copied = usize::try_from(copied)
                .map_err(|_| LambError::ExportInvariant("copied frame count overflow"))?;
            let sample_count = copied
                .checked_mul(channels)
                .ok_or(LambError::ExportInvariant("copied sample count overflow"))?;
            let samples = &self.interleaved_scratch.as_slice()[..sample_count];
            any_nonzero |= samples.iter().any(|sample| *sample != 0.0);
            for channel in 0..channels {
                write_channel_block(
                    samples,
                    copied,
                    channels,
                    channel,
                    &mut self.channel_pcm[channel],
                    &mut self.writers[channel],
                    &self.outputs,
                    &self.paths,
                    self.config.sample_rate,
                    parts_per_channel,
                    io,
                )?;
            }
            cursor = cursor
                .checked_add(copied as u64)
                .ok_or(LambError::ExportInvariant("frozen stream cursor overflow"))?;
        }
        for writer in self.writers.iter_mut() {
            finalize_writer(writer, &self.outputs, &self.paths, io)?;
        }

        Ok(any_nonzero)
    }

    fn retry_pending_cleanup(&mut self) -> Result<()> {
        if self.pending_cleanup.is_some() {
            self.cleanup_transaction()?;
        }
        Ok(())
    }

    fn error_after_cleanup(&mut self, operation: LambError) -> LambError {
        match self.cleanup_transaction() {
            Ok(()) => operation,
            Err(cleanup) => LambError::PersistenceCleanup {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup),
            },
        }
    }

    fn cleanup_transaction(&mut self) -> Result<()> {
        for writer in self.writers.iter_mut() {
            writer.file.take();
        }

        if let Some(PendingCleanup::Unidentified { path_slot }) = self.pending_cleanup {
            let path = self.paths[path_slot as usize].as_path();
            return match self.cleanup_io.symlink_metadata(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.pending_cleanup = None;
                    self.reset_slots();
                    Ok(())
                }
                Err(_) | Ok(_) => Err(LambError::UnidentifiedStagingCleanup {
                    path: path.to_path_buf(),
                }),
            };
        }

        let identity = match self.pending_cleanup {
            Some(PendingCleanup::Identified { device, inode, .. }) => {
                Some(StagingIdentity { device, inode })
            }
            Some(PendingCleanup::Unidentified { .. }) => unreachable!(),
            None => self.staging_identity,
        };
        let Some(identity) = identity else {
            self.reset_slots();
            return Ok(());
        };
        let path = self.paths[TRANSACTION_ROOT_PATH].as_path();
        let cleanup_result = match self.cleanup_io.symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(path, error)),
            Ok(metadata)
                if (metadata.dev(), metadata.ino()) != (identity.device, identity.inode) =>
            {
                Ok(())
            }
            Ok(metadata) if !metadata.file_type().is_dir() => Err(LambError::ExportInvariant(
                "owned staging identity no longer names a directory",
            )),
            Ok(_) => match self.cleanup_io.remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error(path, error)),
            },
        };

        if let Err(error) = cleanup_result {
            self.staging_identity = None;
            self.pending_cleanup = Some(PendingCleanup::Identified {
                path_slot: TRANSACTION_ROOT_PATH as u32,
                device: identity.device,
                inode: identity.inode,
            });
            return Err(error);
        }

        self.staging_identity = None;
        self.pending_cleanup = None;
        self.reset_slots();
        Ok(())
    }

    fn reset_slots(&mut self) {
        for writer in self.writers.iter_mut() {
            writer.file.take();
            writer.output_index = 0;
            writer.frames_written = 0;
        }
        for output in self.outputs.iter_mut() {
            *output = OutputFileSlot::EMPTY;
        }
        for path in self.paths.iter_mut() {
            path.clear();
        }
        self.paths[PARTIAL_SCRATCH_PATH].clear();
        self.paths[MANIFEST_SCRATCH_PATH].clear();
        for entry in self.manifest_entries.iter_mut() {
            *entry = ManifestEntrySlot::default();
        }
        self.manifest_paths.as_mut_slice().fill(0);
        self.manifest_serialization.as_mut_slice().fill(0);
        for slot in self.publication.sync_slots.iter_mut() {
            *slot = DirectorySyncSlot::default();
        }
        self.publication.current_artifact[0] = CurrentArtifactSlot::default();
        self.publication.component_a.as_mut_slice().fill(0);
        self.publication.component_b.as_mut_slice().fill(0);
        self.publication.sync_count = 0;
        self.output_count = 0;
        if let Some(first) = self.interleaved_scratch.as_mut_slice().first_mut() {
            *first = 0.0;
        }
    }

    pub fn pending_cleanup(&self) -> Option<&PendingCleanup> {
        self.pending_cleanup.as_ref()
    }

    pub fn pending_cleanup_path(&self) -> Option<&Path> {
        self.pending_cleanup
            .as_ref()
            .map(|pending| self.paths[pending.path_slot()].as_path())
    }

    pub fn recover_indeterminate_publication(
        &mut self,
        publication: &mut IndeterminatePublication,
    ) -> Result<PublicationRecovery> {
        if let Some(durable) = publication.durable.as_ref() {
            let outcome = match durable {
                DurablePublication::Recall {
                    transaction_root,
                    output_root,
                    ..
                } => recover_recall_root_with_directories(
                    transaction_root,
                    output_root,
                    self.manifest_serialization.as_mut_slice(),
                    self.manifest_entries.as_mut_slice(),
                    self.manifest_directories.as_mut_slice(),
                    self.manifest_paths.as_mut_slice(),
                )?,
                DurablePublication::Dump {
                    output_parent,
                    manifest_path,
                    ..
                } => recover_dump_parent(
                    output_parent,
                    manifest_path,
                    self.manifest_serialization.as_mut_slice(),
                    self.manifest_entries.as_mut_slice(),
                    self.manifest_paths.as_mut_slice(),
                )?,
            };
            return match outcome {
                RecoveryOutcome::Complete => {
                    let durable = publication
                        .durable
                        .take()
                        .expect("durable publication remains present after recovery");
                    self.clear_recovered_transaction();
                    Ok(PublicationRecovery::Complete(durable.output()))
                }
                RecoveryOutcome::RolledBack => {
                    publication.durable = None;
                    self.clear_recovered_transaction();
                    Ok(PublicationRecovery::RolledBack)
                }
                RecoveryOutcome::Pending => Ok(PublicationRecovery::Pending),
            };
        }

        let mut first_error = None;
        let mut index = 0;
        while index < publication.artifacts.len() {
            let artifact = &publication.artifacts[index];
            let resolved = match self.cleanup_io.symlink_metadata(&artifact.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(io_error(&artifact.path, error));
                    }
                    false
                }
                Ok(metadata)
                    if (metadata.dev(), metadata.ino()) != (artifact.device, artifact.inode) =>
                {
                    true
                }
                Ok(_) => match self.cleanup_io.remove_file(&artifact.path) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => {
                        if first_error.is_none() {
                            first_error = Some(io_error(&artifact.path, error));
                        }
                        false
                    }
                },
            };
            if resolved {
                publication.artifacts.swap_remove(index);
            } else {
                index += 1;
            }
        }
        match first_error {
            Some(error) => Err(error),
            None if publication.artifacts.is_empty() => {
                self.cleanup_transaction()?;
                Ok(PublicationRecovery::RolledBack)
            }
            None => Ok(PublicationRecovery::Pending),
        }
    }

    /// Recovers marked recall transactions under `staging_root` using this
    /// workspace's reserved manifest parse arenas, at startup before persistence
    /// is admitted.
    pub fn recover_recall_staging(
        &mut self,
        staging_root: &Path,
        output_root: &Path,
    ) -> RecoveryScanSummary {
        recover_recall_staging_root_with_directories(
            staging_root,
            output_root,
            self.manifest_entries.as_mut_slice(),
            self.manifest_directories.as_mut_slice(),
            self.manifest_paths.as_mut_slice(),
            self.manifest_serialization.as_mut_slice(),
        )
    }

    /// Recovers marked dump transactions under `dump_parent` using this
    /// workspace's reserved manifest parse arenas.
    pub fn recover_dumps(&mut self, dump_parent: &Path) -> RecoveryScanSummary {
        recover_dump_root(
            dump_parent,
            self.manifest_entries.as_mut_slice(),
            self.manifest_paths.as_mut_slice(),
            self.manifest_serialization.as_mut_slice(),
        )
    }

    pub(crate) fn finish_completed_publication(&mut self) {
        let Some(completed) = self.completed_publication.take() else {
            return;
        };
        let transaction_root = self.paths[TRANSACTION_ROOT_PATH].as_path().to_path_buf();
        let final_root = self.paths[FINAL_ROOT_PATH].as_path().to_path_buf();
        let output_parent = if completed.kind == PersistenceKind::Dump {
            self.outputs
                .first()
                .and_then(|output| self.paths.get(output.final_path as usize))
                .and_then(|path| path.as_path().parent())
                .and_then(Path::parent)
                .unwrap_or_else(|| self.paths[FINAL_ROOT_PATH].as_path())
                .to_path_buf()
        } else {
            self.paths[FINAL_ROOT_PATH]
                .as_path()
                .parent()
                .unwrap_or_else(|| self.paths[FINAL_ROOT_PATH].as_path())
                .to_path_buf()
        };
        let manifest_path = match completed.kind {
            PersistenceKind::Recall => transaction_root.join("manifest.json"),
            PersistenceKind::Dump => {
                let staging_name = transaction_root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let transaction_id = staging_name
                    .strip_prefix(".tmp-lamb-")
                    .unwrap_or(&staging_name);
                output_parent.join(format!(".{transaction_id}.manifest.json"))
            }
        };
        if completed.kind == PersistenceKind::Recall {
            let _ = recover_recall_root_with_directories(
                &transaction_root,
                &final_root,
                self.manifest_serialization.as_mut_slice(),
                self.manifest_entries.as_mut_slice(),
                self.manifest_directories.as_mut_slice(),
                self.manifest_paths.as_mut_slice(),
            );
            self.clear_recovered_transaction();
            return;
        }
        let _ = recover_dump_parent(
            &output_parent,
            &manifest_path,
            self.manifest_serialization.as_mut_slice(),
            self.manifest_entries.as_mut_slice(),
            self.manifest_paths.as_mut_slice(),
        );
        self.clear_recovered_transaction();
    }

    fn clear_recovered_transaction(&mut self) {
        self.staging_identity = None;
        self.pending_cleanup = None;
        self.completed_publication = None;
        self.reset_slots();
    }
}

pub enum PreparedPersistence<'a> {
    Silent,
    SkippedSilent,
    SkippedByPolicy,
    FileSet {
        staging: OwnedTransactionArtifacts<'a>,
    },
    AtomicDirectory {
        staging: OwnedTransactionArtifacts<'a>,
    },
    Recall {
        staging: OwnedTransactionArtifacts<'a>,
    },
    Dump {
        staging: OwnedTransactionArtifacts<'a>,
    },
}

impl PreparedPersistence<'_> {
    pub fn files(&self) -> Option<FilePlan<'_>> {
        match self {
            Self::Silent | Self::SkippedSilent | Self::SkippedByPolicy => None,
            Self::Recall { staging }
            | Self::Dump { staging }
            | Self::FileSet { staging }
            | Self::AtomicDirectory { staging } => Some(staging.files()),
        }
    }

    pub fn staging_directory(&self) -> Option<&Path> {
        match self {
            Self::Silent | Self::SkippedSilent | Self::SkippedByPolicy => None,
            Self::Recall { staging }
            | Self::Dump { staging }
            | Self::FileSet { staging }
            | Self::AtomicDirectory { staging } => {
                Some(staging.workspace.paths[TRANSACTION_ROOT_PATH].as_path())
            }
        }
    }
}

pub struct OwnedTransactionArtifacts<'a> {
    workspace: &'a mut PersistenceWorkspace,
}

/// Disjoint mutable views over the preallocated manifest arenas of the
/// workspace, so a publication transaction can build/serialize/recover a
/// manifest without performing heap allocation proportional to entry count.
pub(crate) struct ManifestScratch<'a> {
    pub slots: &'a mut [ManifestEntrySlot],
    pub directories: &'a mut [ManifestDirectorySlot],
    pub path_bytes: &'a mut [u8],
    pub serialization: &'a mut [u8],
}

impl OwnedTransactionArtifacts<'_> {
    pub fn files(&self) -> FilePlan<'_> {
        FilePlan {
            workspace: self.workspace,
        }
    }

    pub(crate) fn recover_publication(
        &mut self,
        publication: &mut IndeterminatePublication,
    ) -> Result<PublicationRecovery> {
        self.workspace
            .recover_indeterminate_publication(publication)
    }

    pub(crate) fn manifest_scratch(&mut self) -> ManifestScratch<'_> {
        ManifestScratch {
            slots: self.workspace.manifest_entries.as_mut_slice(),
            directories: self.workspace.manifest_directories.as_mut_slice(),
            path_bytes: self.workspace.manifest_paths.as_mut_slice(),
            serialization: self.workspace.manifest_serialization.as_mut_slice(),
        }
    }

    pub(crate) fn staging_identity(&self) -> (u64, u64) {
        let identity = self
            .workspace
            .staging_identity
            .expect("prepared artifacts retain their staging identity");
        (identity.device, identity.inode)
    }

    pub(crate) fn final_root(&self) -> &Path {
        self.workspace.paths[FINAL_ROOT_PATH].as_path()
    }

    pub(crate) fn defer_recovery(self) {
        std::mem::forget(self);
    }

    pub(crate) fn defer_completed_cleanup(self, kind: TransactionKind) {
        self.workspace.completed_publication = Some(CompletedPublicationCleanup {
            kind: match kind {
                TransactionKind::FileSet | TransactionKind::Recall => PersistenceKind::Recall,
                TransactionKind::AtomicDirectory | TransactionKind::Dump => PersistenceKind::Dump,
            },
        });
        std::mem::forget(self);
    }
}

#[derive(Debug)]
pub struct IndeterminatePublication {
    artifacts: Vec<PublicationArtifact>,
    durable: Option<DurablePublication>,
}

#[derive(Debug)]
struct PublicationArtifact {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Debug)]
enum DurablePublication {
    Recall {
        transaction_root: PathBuf,
        output_root: PathBuf,
        output: PublishedOutput,
    },
    Dump {
        output_parent: PathBuf,
        manifest_path: PathBuf,
        output: PublishedOutput,
    },
}

impl DurablePublication {
    fn output(self) -> PublishedOutput {
        match self {
            Self::Recall { output, .. } | Self::Dump { output, .. } => output,
        }
    }
}

pub enum PublicationRecovery {
    Complete(PublishedOutput),
    RolledBack,
    Pending,
}

impl IndeterminatePublication {
    pub(crate) fn new() -> Self {
        Self {
            artifacts: Vec::new(),
            durable: None,
        }
    }

    pub(crate) fn recall(
        transaction_root: PathBuf,
        output_root: PathBuf,
        output: PublishedOutput,
    ) -> Self {
        Self {
            artifacts: Vec::new(),
            durable: Some(DurablePublication::Recall {
                transaction_root,
                output_root,
                output,
            }),
        }
    }

    pub(crate) fn dump(
        output_parent: PathBuf,
        manifest_path: PathBuf,
        output: PublishedOutput,
    ) -> Self {
        Self {
            artifacts: Vec::new(),
            durable: Some(DurablePublication::Dump {
                output_parent,
                manifest_path,
                output,
            }),
        }
    }

    pub(crate) fn track(&mut self, path: PathBuf, device: u64, inode: u64) {
        self.artifacts.push(PublicationArtifact {
            path,
            device,
            inode,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.artifacts.is_empty() && self.durable.is_none()
    }
}

impl Drop for OwnedTransactionArtifacts<'_> {
    fn drop(&mut self) {
        let _ = self.workspace.cleanup_transaction();
    }
}

#[derive(Clone, Copy)]
pub struct FilePlan<'a> {
    workspace: &'a PersistenceWorkspace,
}

impl FilePlan<'_> {
    pub fn len(&self) -> usize {
        self.workspace.output_count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.workspace.outputs.len()
    }

    pub fn get(&self, index: usize) -> Option<PreparedFile<'_>> {
        (index < self.len()).then_some(PreparedFile {
            workspace: self.workspace,
            index,
        })
    }
}

#[derive(Clone, Copy)]
pub struct PreparedFile<'a> {
    workspace: &'a PersistenceWorkspace,
    index: usize,
}

impl PreparedFile<'_> {
    fn slot(&self) -> &OutputFileSlot {
        &self.workspace.outputs[self.index]
    }

    pub fn staged_path(&self) -> &Path {
        self.workspace.paths[self.slot().staged_path as usize].as_path()
    }

    pub fn final_path(&self) -> &Path {
        self.workspace.paths[self.slot().final_path as usize].as_path()
    }

    pub fn channel(&self) -> u32 {
        self.slot().channel
    }

    pub fn part(&self) -> u32 {
        self.slot().part + 1
    }

    pub fn start_frame(&self) -> u64 {
        self.slot().start_frame
    }

    pub fn frame_count(&self) -> u64 {
        self.slot().frame_count
    }
}

fn preflight_absent_final_chain(path: &Path, collision: &'static str) -> Result<()> {
    for (depth, ancestor) in path.ancestors().enumerate() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) if depth == 0 => return Err(LambError::ExportInvariant(collision)),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LambError::ExportInvariant(
                    "prepared final path has a symlink ancestor",
                ))
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(LambError::ExportInvariant(
                    "prepared final path has a non-directory ancestor",
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(ancestor, source)),
        }
    }
    Ok(())
}

fn validate_workspace_plan(
    plan: &SessionMemoryPlan,
    config: &PersistenceWorkspaceConfig,
) -> Result<()> {
    let matches = plan.retention_frames() == config.retention_frames
        && plan.channels() == config.channels
        && plan.sample_rate() == config.sample_rate
        && plan.sample_format() == config.sample_format
        && plan.chunk_frames() == config.chunk_frames
        && plan.sample_bytes() == config.sample_bytes
        && plan.split_when_over_bytes() == config.split_when_over_bytes
        && plan.io_buffer_bytes_per_channel() == config.io_buffer_bytes_per_channel
        && plan.maximum_path_bytes() == config.maximum_path_bytes;
    if !matches {
        return Err(LambError::Validation(
            "persistence workspace geometry does not match session memory plan".to_string(),
        ));
    }
    let component_bytes = config.maximum_path_bytes.checked_add(1).ok_or_else(|| {
        LambError::Validation("publication component buffer overflow".to_string())
    })?;
    let sync_payload = plan
        .manifest_directory_slots()
        .checked_mul(PUBLICATION_SYNC_SLOT_BYTES)
        .ok_or_else(|| {
            LambError::Validation("publication sync slot storage overflow".to_string())
        })?;
    let component_storage = allocation_budget_bytes(component_bytes)?
        .checked_mul(2)
        .ok_or_else(|| {
            LambError::Validation("publication component buffer storage overflow".to_string())
        })?;
    let artifact_storage = allocation_budget_bytes(PUBLICATION_ARTIFACT_SLOT_BYTES)?;
    let expected_publication_scratch = allocation_budget_bytes(sync_payload)?
        .checked_add(component_storage)
        .and_then(|bytes| bytes.checked_add(artifact_storage))
        .ok_or_else(|| LambError::Validation("publication scratch overflow".to_string()))?;
    if plan.publication_scratch_bytes() != expected_publication_scratch {
        return Err(LambError::Validation(
            "persistence workspace publication scratch does not match session memory plan"
                .to_string(),
        ));
    }
    if config.io_buffer_bytes_per_channel < WAV_BYTES_PER_SAMPLE {
        return Err(LambError::Validation(
            "persistence PCM buffer must hold at least one 24-bit sample".to_string(),
        ));
    }
    if plan.maximum_wav_parts_per_channel() > u64::from(u32::MAX)
        || plan.path_slots() > u64::from(u32::MAX)
    {
        return Err(LambError::Validation(
            "persistence workspace slot indexes exceed supported maximum".to_string(),
        ));
    }
    Ok(())
}

fn absolute_split_range(
    export_range: Range<u64>,
    zero_part: u64,
    frames_per_part: u64,
) -> Result<Range<u64>> {
    let total_frames =
        export_range
            .end
            .checked_sub(export_range.start)
            .ok_or(LambError::ExportInvariant(
                "workspace export range is reversed",
            ))?;
    let relative_start = zero_part
        .checked_mul(frames_per_part)
        .ok_or(LambError::ExportInvariant("workspace part start overflow"))?;
    if frames_per_part == 0 || relative_start >= total_frames {
        return Err(LambError::ExportInvariant(
            "workspace part is outside export range",
        ));
    }
    let frame_count = (total_frames - relative_start).min(frames_per_part);
    let start = export_range
        .start
        .checked_add(relative_start)
        .ok_or(LambError::ExportInvariant("workspace part start overflow"))?;
    let end = start
        .checked_add(frame_count)
        .ok_or(LambError::ExportInvariant("workspace part end overflow"))?;
    Ok(start..end)
}

fn validate_frozen(config: &PersistenceWorkspaceConfig, frozen: &FrozenCaptureEpoch) -> Result<()> {
    if frozen.channels() != config.channels
        || frozen.sample_rate() != config.sample_rate
        || frozen.format() != config.sample_format
        || frozen.total_frames() == 0
        || frozen.total_frames() > config.retention_frames
    {
        return Err(LambError::ExportInvariant(
            "frozen capture geometry does not match persistence workspace",
        ));
    }
    Ok(())
}

fn validate_policy_geometry(
    config: &PersistenceWorkspaceConfig,
    frozen: &FrozenCaptureEpoch,
    policy: &ResolvedExportPolicy,
    decision: &FrozenExportDecision,
) -> Result<()> {
    if policy.activity.channels.len() != config.channels as usize
        || decision.channels().len() != config.channels as usize
        || frozen.channels() != config.channels
        || frozen.sample_rate() != config.sample_rate
    {
        return Err(LambError::ExportInvariant(
            "export policy does not match frozen geometry",
        ));
    }
    Ok(())
}

fn set_transaction_path(path: &mut ReusablePath, parent: &Path, prefix: &str) -> Result<()> {
    path.set_path(parent)?;
    path.push_separator()?;
    let sequence = NEXT_WORKSPACE_TRANSACTION.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    write!(
        path,
        "{prefix}{:x}-{nanos:x}-{sequence:x}",
        std::process::id()
    )
    .map_err(|_| LambError::Export("transaction path exceeds workspace capacity".to_string()))
}

fn write_output_path(path: &mut ReusablePath, parent: &Path, name: WavBasename<'_>) -> Result<()> {
    path.set_path(parent)?;
    path.push_separator()?;
    let basename_start = path.len();
    write_wav_basename(path, name)
        .map_err(|_| LambError::Export("WAV basename exceeds workspace capacity".to_string()))?;
    let basename = std::str::from_utf8(&path.as_bytes()[basename_start..])
        .map_err(|_| LambError::ExportInvariant("planned WAV path has no UTF-8 basename"))?;
    validate_filename_component(basename, "WAV basename")
}

fn open_writer(
    writer: &mut WriterSlot,
    output_index: usize,
    output_end: usize,
    outputs: &[OutputFileSlot],
    paths: &[ReusablePath],
    sample_rate: u32,
    io: &mut impl WavIo,
) -> Result<()> {
    let output = outputs
        .get(output_index)
        .ok_or(LambError::ExportInvariant("WAV output slot is missing"))?;
    let path = paths[output.staged_path as usize].as_path();
    let mut file = io.open(path).map_err(|source| io_error(path, source))?;
    let header = wav_header(output.frame_count, sample_rate)?;
    if let Err(source) = io.write_all(&mut file, &header) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(io_error(path, source));
    }
    writer.file = Some(file);
    writer.output_index = output_index;
    writer.frames_written = 0;
    writer.output_end = output_end;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_channel_block(
    samples: &[f32],
    frames: usize,
    channels: usize,
    channel: usize,
    pcm: &mut MaterializedBuffer<u8>,
    writer: &mut WriterSlot,
    outputs: &[OutputFileSlot],
    paths: &[ReusablePath],
    sample_rate: u32,
    _parts_per_channel: usize,
    io: &mut impl WavIo,
) -> Result<()> {
    let mut frame_offset = 0;
    let pcm_frames = pcm.len() / WAV_BYTES_PER_SAMPLE as usize;
    while frame_offset < frames {
        let output = outputs[writer.output_index];
        let remaining = usize::try_from(output.frame_count - writer.frames_written)
            .map_err(|_| LambError::ExportInvariant("WAV remaining frame count overflow"))?;
        let batch = (frames - frame_offset).min(remaining).min(pcm_frames);
        for index in 0..batch {
            let sample = samples[(frame_offset + index) * channels + channel];
            let bytes = f32_to_s24_bytes(sample);
            pcm.as_mut_slice()[index * 3..index * 3 + 3].copy_from_slice(&bytes);
        }
        let path = paths[output.staged_path as usize].as_path();
        let file = writer
            .file
            .as_mut()
            .ok_or(LambError::ExportInvariant("WAV writer has no open file"))?;
        io.write_all(file, &pcm.as_slice()[..batch * 3])
            .map_err(|source| io_error(path, source))?;
        writer.frames_written += batch as u64;
        frame_offset += batch;

        if writer.frames_written == output.frame_count {
            finalize_writer(writer, outputs, paths, io)?;
            let next = writer.output_index + 1;
            if next < writer.output_end {
                open_writer(
                    writer,
                    next,
                    writer.output_end,
                    outputs,
                    paths,
                    sample_rate,
                    io,
                )?;
            }
        }
    }
    Ok(())
}

fn finalize_writer(
    writer: &mut WriterSlot,
    outputs: &[OutputFileSlot],
    paths: &[ReusablePath],
    io: &mut impl WavIo,
) -> Result<()> {
    let Some(mut file) = writer.file.take() else {
        return Ok(());
    };
    let path = paths[outputs[writer.output_index].staged_path as usize].as_path();
    io.flush(&mut file)
        .map_err(|source| io_error(path, source))?;
    io.sync_all(&file)
        .map_err(|source| io_error(path, source))?;
    drop(file);
    Ok(())
}

fn wav_header(frames: u64, sample_rate: u32) -> Result<[u8; WAV_HEADER_BYTES as usize]> {
    let data_bytes = frames
        .checked_mul(WAV_BYTES_PER_SAMPLE)
        .ok_or(LambError::ExportInvariant("WAV data byte count overflow"))?;
    if data_bytes > u64::from(u32::MAX - 36) {
        return Err(LambError::Export(
            "classic WAV data exceeds RIFF size limit".to_string(),
        ));
    }
    let byte_rate = sample_rate
        .checked_mul(3)
        .ok_or_else(|| LambError::Export("WAV sample-rate byte count overflow".to_string()))?;
    let mut header = [0_u8; WAV_HEADER_BYTES as usize];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + data_bytes as u32).to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16_u32.to_le_bytes());
    header[20..22].copy_from_slice(&1_u16.to_le_bytes());
    header[22..24].copy_from_slice(&1_u16.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&3_u16.to_le_bytes());
    header[34..36].copy_from_slice(&24_u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&(data_bytes as u32).to_le_bytes());
    Ok(header)
}

fn mix_address(hash: usize, address: usize, length: usize) -> usize {
    hash.rotate_left(7) ^ address.wrapping_mul(31) ^ length
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
    use crate::recovery::{ManifestEntrySlot, TransactionManifest, MANIFEST_VERSION};

    fn test_workspace() -> (SessionMemoryPlan, PersistenceWorkspace) {
        let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames: 100,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 10,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes: 1_073_741_824,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: 4,
            capture_slot_frames: 10,
            capture_worker_stack_bytes: 64 * 1024,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap();
        let config = PersistenceWorkspaceConfig {
            retention_frames: 100,
            channels: 1,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 10,
            sample_bytes: 4,
            split_when_over_bytes: 1_073_741_824,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
        };
        let workspace = PersistenceWorkspace::new(&plan, config).unwrap();
        (plan, workspace)
    }

    #[test]
    fn reserved_manifest_entry_arena_is_consumed_by_manifest_construction() {
        let (_plan, mut workspace) = test_workspace();
        let slots = workspace.manifest_entries.as_mut_slice();
        let path_bytes = workspace.manifest_paths.as_mut_slice();
        let mut manifest = TransactionManifest::new(slots, path_bytes);
        manifest.version = MANIFEST_VERSION;
        manifest.transaction_id = manifest.push_path("tx").unwrap();
        manifest.staging_root_path = manifest.push_path("/staging/tx").unwrap();
        manifest.output_root = manifest.push_path("/output").unwrap();
        manifest.set_entry_count(1).unwrap();
        for index in 0..1 {
            let staged = manifest
                .push_path(&format!("/staging/tx/channel-{index}.wav"))
                .unwrap();
            let partial = manifest
                .push_path(&format!("/output/.channel-{index}.wav.tx.partial"))
                .unwrap();
            let final_path = manifest
                .push_path(&format!("/output/channel-{index}.wav"))
                .unwrap();
            let slot = manifest.entry_mut(index);
            slot.staged_path = staged;
            slot.partial_path = partial;
            slot.final_path = final_path;
        }
        // The reserved arena now holds live entry slots rather than dead bytes.
        assert_eq!(manifest.entries().len(), 1);
        assert!(manifest.entry(0).staged_path.len > 0);
        assert_eq!(
            manifest.path(manifest.entry(0).final_path),
            std::path::Path::new("/output/channel-0.wav")
        );
        assert_ne!(*slots.first().unwrap(), ManifestEntrySlot::default());
    }

    #[test]
    fn absolute_split_range_ending_at_u64_max_preserves_half_open_boundaries() {
        assert_eq!(
            absolute_split_range((u64::MAX - 4)..u64::MAX, 0, 3).unwrap(),
            (u64::MAX - 4)..(u64::MAX - 1)
        );
        assert_eq!(
            absolute_split_range((u64::MAX - 4)..u64::MAX, 1, 3).unwrap(),
            (u64::MAX - 1)..u64::MAX
        );
    }
}
