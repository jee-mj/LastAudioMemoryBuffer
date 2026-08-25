use crate::dump::{PublishedOutput, SampleSnapshot};
use crate::error::{io_error, LambError, Result};
use crate::math::wav_parts_for_channel;
use crate::persistence_workspace::{
    IndeterminatePublication, ManifestScratch, OwnedTransactionArtifacts, PreparedPersistence,
};
use crate::recovery::{
    capture_identity, sync_directory, verify_manifest_matches, FileIdentity as ManifestIdentity,
    ManifestEntrySlot, ManifestPhase, ManifestStore, PathRef, TransactionKind, TransactionManifest,
    MANIFEST_VERSION, RECALL_MANIFEST_NAME,
};
use crate::sample_ring::Snapshot;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone)]
struct OwnedPath {
    path: PathBuf,
    identity: FileIdentity,
}

impl OwnedPath {
    fn capture(path: PathBuf) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self::from_metadata(path, &metadata))
    }

    fn from_file(path: PathBuf, file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self::from_metadata(path, &metadata))
    }

    fn from_metadata(path: PathBuf, metadata: &fs::Metadata) -> Self {
        Self {
            path,
            identity: FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        }
    }

    fn with_path(&self, path: PathBuf) -> Self {
        Self {
            path,
            identity: self.identity,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn still_names_owned_inode(&self, metadata: &fs::Metadata) -> bool {
        metadata.dev() == self.identity.device && metadata.ino() == self.identity.inode
    }
}

/// Linux has no atomic compare-and-unlink operation. Keep this identity check
/// adjacent to removal to narrow, but not eliminate, the remaining race.
fn best_effort_remove_owned_file(owned: &OwnedPath) {
    if let Ok(metadata) = fs::symlink_metadata(owned.path()) {
        if owned.still_names_owned_inode(&metadata) {
            let _ = fs::remove_file(owned.path());
        }
    }
}

/// As with file cleanup, this is best-effort identity protection. The initial
/// metadata lookup does not follow symlinks; `remove_dir_all` runs immediately
/// after the identity comparison.
fn best_effort_remove_owned_directory(owned: &OwnedPath) {
    if let Ok(metadata) = fs::symlink_metadata(owned.path()) {
        if metadata.file_type().is_dir() && owned.still_names_owned_inode(&metadata) {
            let _ = fs::remove_dir_all(owned.path());
        }
    }
}

pub struct RecallPublishRequest<'a> {
    pub snapshot: &'a SampleSnapshot,
    pub output_dir: &'a Path,
    pub staging_root: &'a Path,
    pub timestamp: &'a str,
    pub split_when_over_bytes: u64,
    pub channel_names: &'a [String],
}

pub struct DumpPublishRequest<'a> {
    pub snapshot: &'a SampleSnapshot,
    pub output_parent: &'a Path,
    pub timestamp: &'a str,
    pub split_when_over_bytes: u64,
    pub channel_names: &'a [String],
}

pub struct ExportRequest<'a> {
    pub snapshot: &'a Snapshot,
    pub output_dir: &'a Path,
    pub timestamp: &'a str,
    pub split_when_over_bytes: u64,
    pub channel_names: &'a [String],
    pub simple_names: bool,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub files: Vec<PathBuf>,
}

pub fn export_snapshot_wav(request: ExportRequest<'_>) -> Result<ExportResult> {
    fs::create_dir_all(request.output_dir)
        .map_err(|source| io_error(request.output_dir, source))?;
    let mut files = Vec::new();
    for channel in 0..request.snapshot.channels() {
        let samples = request.snapshot.read_channel_samples(channel)?;
        let parts = wav_parts_for_channel(samples.len() as u64, 3, request.split_when_over_bytes)?;
        for (part_index, part) in parts.iter().enumerate() {
            let channel_name = request
                .channel_names
                .get(channel as usize)
                .cloned()
                .unwrap_or_else(|| format!("ch{:02}", channel + 1));
            let final_path = if request.simple_names {
                if parts.len() > 1 {
                    request.output_dir.join(format!(
                        "{}-part{:03}.wav",
                        channel_name,
                        part_index + 1
                    ))
                } else {
                    request.output_dir.join(format!("{channel_name}.wav"))
                }
            } else {
                request.output_dir.join(format!(
                    "lamb-{}-{}-{}Hz-{:09}-{:09}-part{:03}.wav",
                    request.timestamp,
                    channel_name,
                    request.snapshot.sample_rate(),
                    part.start_frame,
                    part.start_frame + part.frame_count,
                    part_index + 1
                ))
            };
            let temp_path = final_path.with_extension("wav.partial");
            let start = part.start_frame as usize;
            let end = (part.start_frame + part.frame_count) as usize;
            let temp_owned = write_mono_wav(
                &temp_path,
                &samples[start..end],
                request.snapshot.sample_rate(),
            )?;
            if let Err(error) = rename_no_replace(&temp_path, &final_path) {
                best_effort_remove_owned_file(&temp_owned);
                return Err(error);
            }
            files.push(final_path);
        }
    }
    Ok(ExportResult { files })
}

pub fn publish_recall(request: RecallPublishRequest<'_>) -> Result<PublishedOutput> {
    fs::create_dir_all(request.output_dir)
        .map_err(|source| io_error(request.output_dir, source))?;
    fs::create_dir_all(request.staging_root)
        .map_err(|source| io_error(request.staging_root, source))?;
    let (transaction_id, staging_dir) = create_unique_directory(request.staging_root, "")?;
    let mut partials = Vec::new();
    let mut created_finals = Vec::new();

    let publication = (|| {
        let staged_files = stage_snapshot_wavs(
            request.snapshot,
            staging_dir.path(),
            request.timestamp,
            request.split_when_over_bytes,
            request.channel_names,
            false,
        )?;

        for staged_path in staged_files {
            let file_name = staged_path.file_name().ok_or_else(|| {
                LambError::Export(format!(
                    "staged WAV has no filename: {}",
                    staged_path.display()
                ))
            })?;
            let final_path = request.output_dir.join(file_name);
            let partial_path = request.output_dir.join(format!(
                ".{}.{}.partial",
                file_name.to_string_lossy(),
                transaction_id
            ));
            let mut source =
                File::open(&staged_path).map_err(|source| io_error(&staged_path, source))?;
            let mut partial = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)
                .map_err(|source| io_error(&partial_path, source))?;
            let partial_owned = OwnedPath::from_file(partial_path.clone(), &partial)
                .map_err(|source| io_error(&partial_path, source))?;
            partials.push(partial_owned.clone());
            io::copy(&mut source, &mut partial)
                .map_err(|source| io_error(&partial_path, source))?;
            partial
                .flush()
                .map_err(|source| io_error(&partial_path, source))?;
            partial
                .sync_all()
                .map_err(|source| io_error(&partial_path, source))?;
            drop(partial);

            rename_no_replace(&partial_path, &final_path)?;
            created_finals.push(partial_owned.with_path(final_path));
        }
        Ok(())
    })();

    if let Err(error) = publication {
        for path in &created_finals {
            best_effort_remove_owned_file(path);
        }
        for path in &partials {
            best_effort_remove_owned_file(path);
        }
        best_effort_remove_owned_directory(&staging_dir);
        return Err(error);
    }

    best_effort_remove_owned_directory(&staging_dir);
    Ok(PublishedOutput {
        output_directory: request.output_dir.to_path_buf(),
        files: created_finals.into_iter().map(|owned| owned.path).collect(),
    })
}

pub fn publish_dump(request: DumpPublishRequest<'_>) -> Result<PublishedOutput> {
    validate_filename_component(request.timestamp, "dump timestamp")?;
    fs::create_dir_all(request.output_parent)
        .map_err(|source| io_error(request.output_parent, source))?;
    let (_, staging_dir) = create_unique_directory(request.output_parent, ".tmp-lamb-")?;
    let final_dir = request.output_parent.join(request.timestamp);

    let publication = (|| {
        let staged_files = stage_snapshot_wavs(
            request.snapshot,
            staging_dir.path(),
            request.timestamp,
            request.split_when_over_bytes,
            request.channel_names,
            true,
        )?;
        rename_no_replace(staging_dir.path(), &final_dir)?;
        Ok(staged_files)
    })();

    match publication {
        Ok(staged_files) => Ok(PublishedOutput {
            output_directory: final_dir.clone(),
            files: staged_files
                .into_iter()
                .map(|path| final_dir.join(path.file_name().expect("staged WAV has a filename")))
                .collect(),
        }),
        Err(error) => {
            best_effort_remove_owned_directory(&staging_dir);
            Err(error)
        }
    }
}

pub enum PreparedPublication {
    Published(PublishedOutput),
    RetryableFailure(LambError),
    Indeterminate {
        operation: LambError,
        cleanup: IndeterminatePublication,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationCheckpoint {
    RecallManifestPrepared,
    RecallParentCreatedBeforeOwnedManifest { index: usize },
    RecallParentOwnedManifestRecorded { index: usize },
    RecallPartialCreatedBeforeManifest { index: usize },
    RecallPartialSynced { index: usize },
    RecallBeforeFinalRename { index: usize },
    RecallRenamedBeforeManifest { index: usize },
    RecallAfterFinalRename { index: usize },
    RecallFilesSynced,
    RecallOutputSynced,
    RecallCompleteRecorded,
    DumpFilesSynced,
    DumpDirectorySynced,
    DumpManifestPrepared,
    DumpAfterRename,
    DumpParentSynced,
    DumpCompleteRecorded,
}

pub fn publish_prepared(prepared: PreparedPersistence<'_>) -> PreparedPublication {
    publish_prepared_with_hook(prepared, &mut NoopPreparedPublicationHook)
}

pub trait PreparedPublicationHook {
    fn before_rename(&mut self, _index: usize, _final_path: &Path) -> Result<()> {
        Ok(())
    }

    fn checkpoint(&mut self, _checkpoint: PublicationCheckpoint) -> Result<()> {
        Ok(())
    }

    fn sync_directory(&mut self, path: &Path) -> Result<()> {
        sync_directory(path)
    }
}

struct NoopPreparedPublicationHook;

impl PreparedPublicationHook for NoopPreparedPublicationHook {}

pub fn publish_prepared_with_hook(
    prepared: PreparedPersistence<'_>,
    hook: &mut impl PreparedPublicationHook,
) -> PreparedPublication {
    match prepared {
        PreparedPersistence::Silent
        | PreparedPersistence::SkippedSilent
        | PreparedPersistence::SkippedByPolicy => PreparedPublication::RetryableFailure(
            LambError::ExportInvariant("silent persistence has no prepared artifacts"),
        ),
        PreparedPersistence::Recall { staging } | PreparedPersistence::FileSet { staging } => {
            publish_prepared_recall(staging, hook)
        }
        PreparedPersistence::Dump { staging }
        | PreparedPersistence::AtomicDirectory { staging } => publish_prepared_dump(staging, hook),
    }
}

fn publish_prepared_recall(
    mut staging: OwnedTransactionArtifacts<'_>,
    hook: &mut impl PreparedPublicationHook,
) -> PreparedPublication {
    let files = staging.files();
    let planned = match (0..files.len())
        .map(|index| {
            files
                .get(index)
                .ok_or(LambError::ExportInvariant(
                    "prepared recall file plan changed during publication",
                ))
                .map(|file| {
                    (
                        file.staged_path().to_path_buf(),
                        file.final_path().to_path_buf(),
                    )
                })
        })
        .collect::<Result<Vec<_>>>()
    {
        Ok(planned) if !planned.is_empty() => planned,
        Ok(_) => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared recall has no files",
            ))
        }
        Err(error) => return PreparedPublication::RetryableFailure(error),
    };
    let output_directory = staging.final_root().to_path_buf();
    for (_, final_path) in &planned {
        match fs::symlink_metadata(final_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return PreparedPublication::RetryableFailure(io_error(final_path, error))
            }
            Ok(_) => {
                return PreparedPublication::RetryableFailure(io_error(
                    final_path,
                    io::Error::new(io::ErrorKind::AlreadyExists, "final path already exists"),
                ))
            }
        }
    }
    let staging_directory = match planned[0].0.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared recall staging path has no parent",
            ))
        }
    };
    // NOTE: the export layer's `planned`/`partials`/`created_finals` vectors are
    // non-realtime and bounded by the configured output-part count. They remain
    // intentionally outside the arena-backed persistence model so that
    // arena-specific APIs do not propagate through the export layer. Only the
    // manifest model and streaming WAV buffers are arena-backed.
    let transaction_id = match staging_directory.file_name().and_then(|name| name.to_str()) {
        Some(transaction_id) => transaction_id.to_string(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared recall transaction id is invalid",
            ))
        }
    };
    let staging_identity = staging.staging_identity();
    let ManifestScratch {
        slots,
        directories,
        path_bytes,
        serialization,
    } = staging.manifest_scratch();
    match publish_recall_inner(
        slots,
        directories,
        path_bytes,
        serialization,
        hook,
        &planned,
        &staging_directory,
        &transaction_id,
        &output_directory,
        staging_identity,
    ) {
        Ok(published) => {
            staging.defer_completed_cleanup(TransactionKind::FileSet);
            PreparedPublication::Published(published)
        }
        Err(error) => {
            if error.durable_failure {
                let output = PublishedOutput {
                    output_directory: output_directory.clone(),
                    files: planned
                        .iter()
                        .map(|(_, final_path)| final_path.clone())
                        .collect(),
                };
                let cleanup =
                    IndeterminatePublication::recall(staging_directory, output_directory, output);
                staging.defer_recovery();
                return PreparedPublication::Indeterminate {
                    operation: error.operation,
                    cleanup,
                };
            }
            let mut cleanup = IndeterminatePublication::new();
            for owned in error.created_finals.iter().chain(error.partials.iter()) {
                cleanup.track(
                    owned.path.clone(),
                    owned.identity.device,
                    owned.identity.inode,
                );
            }
            let _ = staging.recover_publication(&mut cleanup);
            if cleanup.is_empty() {
                return PreparedPublication::RetryableFailure(error.operation);
            }
            staging.defer_recovery();
            PreparedPublication::Indeterminate {
                operation: error.operation,
                cleanup,
            }
        }
    }
}

struct RecallPublishError {
    operation: LambError,
    durable_failure: bool,
    created_finals: Vec<OwnedPath>,
    partials: Vec<OwnedPath>,
}

impl RecallPublishError {
    fn retryable(operation: LambError) -> Self {
        Self {
            operation,
            durable_failure: false,
            created_finals: Vec::new(),
            partials: Vec::new(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_recall_inner(
    slots: &mut [ManifestEntrySlot],
    directories: &mut [crate::recovery::ManifestDirectorySlot],
    path_bytes: &mut [u8],
    serialization: &mut [u8],
    hook: &mut impl PreparedPublicationHook,
    planned: &[(PathBuf, PathBuf)],
    staging_directory: &Path,
    transaction_id: &str,
    output_directory: &Path,
    staging_identity: (u64, u64),
) -> std::result::Result<PublishedOutput, RecallPublishError> {
    let mut manifest = TransactionManifest::new_with_directories(slots, directories, path_bytes);
    manifest.version = MANIFEST_VERSION;
    manifest.uid = unsafe { libc::geteuid() };
    manifest.kind = TransactionKind::FileSet;
    manifest.phase = ManifestPhase::Prepared;
    manifest.transaction_id = manifest
        .push_path(transaction_id)
        .map_err(RecallPublishError::retryable)?;
    manifest.staging_root_path = manifest
        .push_path(
            path_to_str(staging_directory, "staging directory")
                .map_err(RecallPublishError::retryable)?,
        )
        .map_err(RecallPublishError::retryable)?;
    manifest.staging_root_identity = Some(ManifestIdentity {
        device: staging_identity.0,
        inode: staging_identity.1,
    });
    manifest.output_root = manifest
        .push_path(
            path_to_str(output_directory, "output directory")
                .map_err(RecallPublishError::retryable)?,
        )
        .map_err(RecallPublishError::retryable)?;
    manifest
        .set_entry_count(planned.len())
        .map_err(RecallPublishError::retryable)?;

    for (index, (staged_path, final_path)) in planned.iter().enumerate() {
        let file_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                RecallPublishError::retryable(LambError::ExportInvariant(
                    "prepared recall final filename is invalid",
                ))
            })?;
        let partial_parent = final_path.parent().ok_or_else(|| {
            RecallPublishError::retryable(LambError::ExportInvariant(
                "prepared file-set final path has no parent",
            ))
        })?;
        let partial_path = partial_parent.join(format!(".{file_name}.{transaction_id}.partial"));
        let staged_ref = manifest
            .push_path(
                path_to_str(staged_path, "staged path").map_err(RecallPublishError::retryable)?,
            )
            .map_err(RecallPublishError::retryable)?;
        let staged_identity =
            capture_identity(staged_path).map_err(RecallPublishError::retryable)?;
        let partial_ref = manifest
            .push_path(
                path_to_str(&partial_path, "partial path")
                    .map_err(RecallPublishError::retryable)?,
            )
            .map_err(RecallPublishError::retryable)?;
        let final_ref = manifest
            .push_path(
                path_to_str(final_path, "final path").map_err(RecallPublishError::retryable)?,
            )
            .map_err(RecallPublishError::retryable)?;
        let slot = manifest.entry_mut(index);
        slot.staged_path = staged_ref;
        slot.staged_identity = Some(staged_identity);
        slot.partial_path = partial_ref;
        slot.final_path = final_ref;
    }

    // Preflight all candidates before mutating final publication state.  The
    // manifest records every absent candidate as an identity-unknown intent;
    // a crash before identity capture is consequently conservative.
    for (entry_index, (_, final_path)) in planned.iter().enumerate() {
        register_file_set_parent_intents(&mut manifest, output_directory, entry_index, final_path)
            .map_err(RecallPublishError::retryable)?;
    }
    let manifest_path = staging_directory.join(RECALL_MANIFEST_NAME);
    persist_manifest(&mut *serialization, &manifest_path, &manifest, hook)
        .map_err(RecallPublishError::retryable)?;
    hook.checkpoint(PublicationCheckpoint::RecallManifestPrepared)
        .map_err(|operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        })?;

    if let Err(operation) = materialize_file_set_parents(
        &mut manifest,
        output_directory,
        &manifest_path,
        serialization,
        hook,
    ) {
        return Err(RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        });
    }

    let mut durable_failure = false;
    let mut partials = Vec::with_capacity(planned.len());
    let mut created_finals = Vec::with_capacity(planned.len());
    let publication = (|| {
        for (index, (staged_path, final_path)) in planned.iter().enumerate() {
            manifest.phase = ManifestPhase::Publishing { index };
            persist_manifest(&mut *serialization, &manifest_path, &manifest, hook)?;
            let partial_path = manifest
                .path(manifest.entry(index).partial_path)
                .to_path_buf();
            let mut source =
                File::open(staged_path).map_err(|source| io_error(staged_path, source))?;
            let mut partial = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial_path)
                .map_err(|source| io_error(&partial_path, source))?;
            let partial_owned = OwnedPath::from_file(partial_path.clone(), &partial)
                .map_err(|source| io_error(&partial_path, source))?;
            partials.push(partial_owned.clone());
            io::copy(&mut source, &mut partial)
                .map_err(|source| io_error(&partial_path, source))?;
            partial
                .flush()
                .map_err(|source| io_error(&partial_path, source))?;
            partial
                .sync_all()
                .map_err(|source| io_error(&partial_path, source))?;
            drop(partial);
            if let Err(error) =
                hook.checkpoint(PublicationCheckpoint::RecallPartialCreatedBeforeManifest { index })
            {
                durable_failure = true;
                return Err(error);
            }
            manifest.entry_mut(index).partial_identity = Some(ManifestIdentity {
                device: partial_owned.identity.device,
                inode: partial_owned.identity.inode,
            });
            persist_manifest(&mut *serialization, &manifest_path, &manifest, hook)?;
            if let Err(error) =
                hook.checkpoint(PublicationCheckpoint::RecallPartialSynced { index })
            {
                durable_failure = true;
                return Err(error);
            }
            if let Err(error) =
                hook.checkpoint(PublicationCheckpoint::RecallBeforeFinalRename { index })
            {
                durable_failure = true;
                return Err(error);
            }
            hook.before_rename(index, final_path)?;
            rename_no_replace(&partial_path, final_path)?;
            let final_owned = partial_owned.with_path(final_path.to_path_buf());
            created_finals.push(final_owned.clone());
            if let Err(error) =
                hook.checkpoint(PublicationCheckpoint::RecallRenamedBeforeManifest { index })
            {
                durable_failure = true;
                return Err(error);
            }
            manifest.entry_mut(index).final_identity = Some(ManifestIdentity {
                device: final_owned.identity.device,
                inode: final_owned.identity.inode,
            });
            manifest.entry_mut(index).partial_identity = None;
            if let Err(error) =
                persist_manifest(&mut *serialization, &manifest_path, &manifest, hook)
            {
                durable_failure = true;
                return Err(error);
            }
            if let Err(error) =
                hook.checkpoint(PublicationCheckpoint::RecallAfterFinalRename { index })
            {
                durable_failure = true;
                return Err(error);
            }
        }
        Ok(())
    })();

    if let Err(operation) = publication {
        return Err(RecallPublishError {
            operation,
            durable_failure,
            created_finals,
            partials,
        });
    }

    for owned in &created_finals {
        sync_published_file(&owned.path).map_err(|operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        })?;
    }
    hook.checkpoint(PublicationCheckpoint::RecallFilesSynced)
        .map_err(|operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        })?;
    let mut parents = vec![output_directory.to_path_buf()];
    for owned in &created_finals {
        let mut parent = owned.path.parent();
        while let Some(path) = parent {
            if !path.starts_with(output_directory) {
                break;
            }
            parents.push(path.to_path_buf());
            if path == output_directory {
                break;
            }
            parent = path.parent();
        }
    }
    parents.sort();
    parents.dedup();
    for parent in parents {
        hook.sync_directory(&parent)
            .map_err(|operation| RecallPublishError {
                operation,
                durable_failure: true,
                created_finals: Vec::new(),
                partials: Vec::new(),
            })?;
    }
    hook.checkpoint(PublicationCheckpoint::RecallOutputSynced)
        .map_err(|operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        })?;
    manifest.phase = ManifestPhase::Complete;
    persist_manifest(&mut *serialization, &manifest_path, &manifest, hook).map_err(
        |operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        },
    )?;
    hook.checkpoint(PublicationCheckpoint::RecallCompleteRecorded)
        .map_err(|operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        })?;
    let _ = verify_manifest_matches(&mut *serialization, &manifest_path, &manifest).map_err(
        |operation| RecallPublishError {
            operation,
            durable_failure: true,
            created_finals: Vec::new(),
            partials: Vec::new(),
        },
    )?;

    Ok(PublishedOutput {
        output_directory: output_directory.to_path_buf(),
        files: created_finals.into_iter().map(|owned| owned.path).collect(),
    })
}

fn path_to_str<'a>(path: &'a Path, description: &str) -> std::result::Result<&'a str, LambError> {
    path.to_str().ok_or_else(|| {
        LambError::Validation(format!(
            "{description} is not valid UTF-8: {}",
            path.display()
        ))
    })
}

/// Creates a FileSet parent a component at a time, refusing symlinks at every
/// existing ancestor.  The prepared path renderer has already established
/// lexical containment; this closes the filesystem race/indirection boundary.
fn register_file_set_parent_intents(
    manifest: &mut TransactionManifest,
    output_root: &Path,
    entry_index: usize,
    final_path: &Path,
) -> Result<()> {
    let parent = final_path.parent().ok_or(LambError::ExportInvariant(
        "prepared file-set final path has no parent",
    ))?;
    let relative = parent.strip_prefix(output_root).map_err(|_| {
        LambError::Validation("file-set final path escapes output root".to_string())
    })?;
    let mut current = output_root.to_path_buf();
    match fs::symlink_metadata(&current) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(LambError::Validation(format!(
                "refusing symlink output root: {}",
                current.display()
            )))
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(LambError::Validation(format!(
                "file-set output root is not a directory: {}",
                current.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&current, source)),
    }
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(LambError::Validation(
                "file-set parent is not lexically contained".to_string(),
            ));
        }
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LambError::Validation(format!(
                    "refusing symlink ancestor: {}",
                    current.display()
                )))
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(LambError::Validation(format!(
                    "file-set parent is not a directory: {}",
                    current.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let prefix_len = current
                    .to_str()
                    .ok_or_else(|| {
                        LambError::Validation(
                            "created directory path is not valid UTF-8".to_string(),
                        )
                    })?
                    .len();
                if !manifest.directories().iter().any(|slot| {
                    manifest
                        .directory_path(slot)
                        .is_ok_and(|path| path == current)
                }) {
                    manifest.push_directory_intent(entry_index, prefix_len)?;
                }
            }
            Err(source) => return Err(io_error(&current, source)),
        }
    }
    Ok(())
}

fn materialize_file_set_parents(
    manifest: &mut TransactionManifest,
    output_root: &Path,
    manifest_path: &Path,
    serialization: &mut [u8],
    hook: &mut impl PreparedPublicationHook,
) -> Result<()> {
    // Output root belongs to configuration rather than the transaction journal.
    match fs::symlink_metadata(output_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(LambError::Validation(format!(
                "invalid file-set output root: {}",
                output_root.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(output_root).map_err(|source| io_error(output_root, source))?
        }
        Err(source) => return Err(io_error(output_root, source)),
    }
    for index in 0..manifest.directory_count {
        let directory = manifest.directories()[index];
        let path = manifest.directory_path(&directory)?;
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(LambError::Validation(format!(
                    "invalid file-set parent: {}",
                    path.display()
                )))
            }
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(path).map_err(|source| io_error(path, source))?;
                // If we crash before this capture/update, the durable Intent is
                // intentionally left identity-unknown and recovery will not rmdir it.
                hook.checkpoint(
                    PublicationCheckpoint::RecallParentCreatedBeforeOwnedManifest { index },
                )?;
                let identity = capture_identity(path)?;
                let slot = manifest.directory_mut(index)?;
                slot.state = Some(crate::recovery::ManifestDirectoryState::Owned);
                slot.identity = Some(identity);
                persist_manifest(serialization, manifest_path, manifest, hook)?;
                hook.checkpoint(PublicationCheckpoint::RecallParentOwnedManifestRecorded {
                    index,
                })?;
            }
            Err(source) => return Err(io_error(path, source)),
        }
    }
    Ok(())
}

fn publish_prepared_dump(
    mut staging: OwnedTransactionArtifacts<'_>,
    hook: &mut impl PreparedPublicationHook,
) -> PreparedPublication {
    let files = staging.files();
    let planned = match (0..files.len())
        .map(|index| {
            files
                .get(index)
                .ok_or(LambError::ExportInvariant(
                    "prepared dump file plan changed during publication",
                ))
                .map(|file| {
                    (
                        file.staged_path().to_path_buf(),
                        file.final_path().to_path_buf(),
                    )
                })
        })
        .collect::<Result<Vec<_>>>()
    {
        Ok(planned) if !planned.is_empty() => planned,
        Ok(_) => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump has no files",
            ))
        }
        Err(error) => return PreparedPublication::RetryableFailure(error),
    };
    let staging_directory = match planned[0].0.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump staging path has no parent",
            ))
        }
    };
    let output_directory = match planned[0].1.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump final path has no parent",
            ))
        }
    };
    let output_parent = match output_directory.parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump final directory has no parent",
            ))
        }
    };
    let transaction_id = match staging_directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".tmp-lamb-"))
        .filter(|id| !id.is_empty())
    {
        Some(transaction_id) => transaction_id.to_string(),
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump transaction id is invalid",
            ))
        }
    };
    let manifest_path = output_parent.join(format!(".{transaction_id}.manifest.json"));
    let staging_identity = staging.staging_identity();
    let ManifestScratch {
        slots,
        directories: _,
        path_bytes,
        serialization,
    } = staging.manifest_scratch();

    match publish_dump_inner(
        slots,
        path_bytes,
        serialization,
        hook,
        &planned,
        &staging_directory,
        &output_directory,
        &output_parent,
        &transaction_id,
        &manifest_path,
        staging_identity,
    ) {
        Ok(published) => {
            staging.defer_completed_cleanup(TransactionKind::AtomicDirectory);
            PreparedPublication::Published(published)
        }
        Err((error, Some(durable))) => {
            let (output_parent, manifest_path, published) = *durable;
            let cleanup = IndeterminatePublication::dump(output_parent, manifest_path, published);
            staging.defer_recovery();
            PreparedPublication::Indeterminate {
                operation: error,
                cleanup,
            }
        }
        Err((error, None)) => PreparedPublication::RetryableFailure(error),
    }
}

type DumpFailure = (LambError, Option<Box<(PathBuf, PathBuf, PublishedOutput)>>);

fn dump_durable(
    output_parent: &Path,
    manifest_path: &Path,
    published: &PublishedOutput,
) -> Option<Box<(PathBuf, PathBuf, PublishedOutput)>> {
    Some(Box::new((
        output_parent.to_path_buf(),
        manifest_path.to_path_buf(),
        published.clone(),
    )))
}

#[allow(clippy::too_many_arguments)]
fn publish_dump_inner(
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
    serialization: &mut [u8],
    hook: &mut impl PreparedPublicationHook,
    planned: &[(PathBuf, PathBuf)],
    staging_directory: &Path,
    output_directory: &Path,
    output_parent: &Path,
    transaction_id: &str,
    manifest_path: &Path,
    staging_identity: (u64, u64),
) -> std::result::Result<PublishedOutput, DumpFailure> {
    let final_files: Vec<PathBuf> = planned
        .iter()
        .map(|(_, final_path)| final_path.clone())
        .collect();
    match fs::symlink_metadata(output_directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err((io_error(output_directory, error), None)),
        Ok(_) => {
            return Err((
                io_error(
                    output_directory,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "final dump directory already exists",
                    ),
                ),
                None,
            ))
        }
    }

    let mut manifest = TransactionManifest::new(slots, path_bytes);
    manifest.version = MANIFEST_VERSION;
    manifest.uid = unsafe { libc::geteuid() };
    manifest.kind = TransactionKind::AtomicDirectory;
    manifest.phase = ManifestPhase::Prepared;
    manifest.transaction_id = manifest
        .push_path(transaction_id)
        .map_err(|error| (error, None))?;
    manifest.staging_root_path = manifest
        .push_path(
            path_to_str(staging_directory, "staging directory").map_err(|error| (error, None))?,
        )
        .map_err(|error| (error, None))?;
    manifest.staging_root_identity = Some(ManifestIdentity {
        device: staging_identity.0,
        inode: staging_identity.1,
    });
    manifest.output_root = manifest
        .push_path(path_to_str(output_parent, "dump parent").map_err(|error| (error, None))?)
        .map_err(|error| (error, None))?;
    manifest.final_directory_path = manifest
        .push_path(
            path_to_str(output_directory, "dump final directory").map_err(|error| (error, None))?,
        )
        .map_err(|error| (error, None))?;
    manifest.final_directory_identity = Some(ManifestIdentity {
        device: staging_identity.0,
        inode: staging_identity.1,
    });
    manifest
        .set_entry_count(planned.len())
        .map_err(|error| (error, None))?;

    for (index, (staged_path, final_path)) in planned.iter().enumerate() {
        let staged_identity = capture_identity(staged_path).map_err(|error| (error, None))?;
        let staged_ref = manifest
            .push_path(path_to_str(staged_path, "staged path").map_err(|error| (error, None))?)
            .map_err(|error| (error, None))?;
        let final_ref = manifest
            .push_path(path_to_str(final_path, "final path").map_err(|error| (error, None))?)
            .map_err(|error| (error, None))?;
        let slot = manifest.entry_mut(index);
        slot.staged_path = staged_ref;
        slot.staged_identity = Some(staged_identity);
        slot.partial_path = PathRef::default();
        slot.final_path = final_ref;
        slot.final_identity = Some(staged_identity);
    }

    let published = PublishedOutput {
        output_directory: output_directory.to_path_buf(),
        files: final_files,
    };

    for entry in manifest.entries() {
        sync_published_file(manifest.path(entry.staged_path)).map_err(|error| (error, None))?;
    }
    hook.checkpoint(PublicationCheckpoint::DumpFilesSynced)
        .map_err(|error| (error, None))?;
    hook.sync_directory(staging_directory)
        .map_err(|error| (error, None))?;
    hook.checkpoint(PublicationCheckpoint::DumpDirectorySynced)
        .map_err(|error| (error, None))?;
    if let Err(error) = persist_manifest(&mut *serialization, manifest_path, &manifest, hook) {
        let durable = fs::symlink_metadata(manifest_path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
            .map(|_| {
                Box::new((
                    output_parent.to_path_buf(),
                    manifest_path.to_path_buf(),
                    published.clone(),
                ))
            });
        return Err((error, durable));
    }
    hook.checkpoint(PublicationCheckpoint::DumpManifestPrepared)
        .map_err(|error| {
            (
                error,
                dump_durable(output_parent, manifest_path, &published),
            )
        })?;
    rename_no_replace(staging_directory, &published.output_directory).map_err(|error| {
        (
            error,
            dump_durable(output_parent, manifest_path, &published),
        )
    })?;
    hook.checkpoint(PublicationCheckpoint::DumpAfterRename)
        .map_err(|error| {
            (
                error,
                dump_durable(output_parent, manifest_path, &published),
            )
        })?;
    hook.sync_directory(output_parent).map_err(|error| {
        (
            error,
            dump_durable(output_parent, manifest_path, &published),
        )
    })?;
    hook.checkpoint(PublicationCheckpoint::DumpParentSynced)
        .map_err(|error| {
            (
                error,
                dump_durable(output_parent, manifest_path, &published),
            )
        })?;
    manifest.phase = ManifestPhase::Complete;
    persist_manifest(&mut *serialization, manifest_path, &manifest, hook).map_err(|error| {
        (
            error,
            dump_durable(output_parent, manifest_path, &published),
        )
    })?;
    hook.checkpoint(PublicationCheckpoint::DumpCompleteRecorded)
        .map_err(|error| {
            (
                error,
                dump_durable(output_parent, manifest_path, &published),
            )
        })?;
    let _ = verify_manifest_matches(&mut *serialization, manifest_path, &manifest).map_err(
        |error| {
            (
                error,
                dump_durable(output_parent, manifest_path, &published),
            )
        },
    )?;
    Ok(published)
}

fn persist_manifest(
    serialization: &mut [u8],
    manifest_path: &Path,
    manifest: &TransactionManifest,
    hook: &mut impl PreparedPublicationHook,
) -> Result<()> {
    ManifestStore::new(serialization).write_with_directory_sync(manifest_path, manifest, |parent| {
        hook.sync_directory(parent)
    })
}

fn sync_published_file(path: &Path) -> Result<()> {
    let file = File::open(path).map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))
}

fn stage_snapshot_wavs(
    snapshot: &SampleSnapshot,
    output_dir: &Path,
    timestamp: &str,
    split_when_over_bytes: u64,
    channel_names: &[String],
    simple_names: bool,
) -> Result<Vec<PathBuf>> {
    let mut basenames = HashSet::new();
    let mut planned_files = Vec::new();
    for (channel, samples) in snapshot.channel_samples().iter().enumerate() {
        let parts = wav_parts_for_channel(samples.len() as u64, 3, split_when_over_bytes)?;
        for (part_index, part) in parts.iter().enumerate() {
            let mut file_name = String::new();
            write_wav_basename(
                &mut file_name,
                WavBasename {
                    simple_names,
                    timestamp,
                    channel_name: channel_names.get(channel).map(String::as_str),
                    channel_index: channel,
                    sample_rate: snapshot.sample_rate(),
                    part_count: parts.len() as u64,
                    part_index: part_index as u64,
                    start_frame: part.start_frame,
                    frame_count: part.frame_count,
                },
            )
            .expect("writing a WAV basename to String cannot fail");
            validate_filename_component(&file_name, "WAV basename")?;
            if !basenames.insert(file_name.clone()) {
                return Err(LambError::Export(format!(
                    "duplicate WAV filename: {file_name}"
                )));
            }
            let start = part.start_frame as usize;
            let end = (part.start_frame + part.frame_count) as usize;
            planned_files.push((channel, start, end, file_name));
        }
    }

    let mut files = Vec::with_capacity(planned_files.len());
    for (channel, start, end, file_name) in planned_files {
        let path = output_dir.join(file_name);
        let _ = write_mono_wav(
            &path,
            &snapshot.channel_samples()[channel][start..end],
            snapshot.sample_rate(),
        )?;
        files.push(path);
    }
    Ok(files)
}

pub(crate) fn validate_filename_component(value: &str, description: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if value.contains('\0')
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(LambError::Export(format!(
            "{description} is not a safe filename component: {value:?}"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct WavBasename<'a> {
    pub simple_names: bool,
    pub timestamp: &'a str,
    pub channel_name: Option<&'a str>,
    pub channel_index: usize,
    pub sample_rate: u32,
    pub part_count: u64,
    pub part_index: u64,
    pub start_frame: u64,
    pub frame_count: u64,
}

pub(crate) fn write_wav_basename(
    output: &mut impl fmt::Write,
    name: WavBasename<'_>,
) -> fmt::Result {
    let channel_name = name
        .channel_name
        .map(ChannelName::Configured)
        .unwrap_or(ChannelName::Default(name.channel_index + 1));
    if name.simple_names {
        if name.part_count > 1 {
            write!(output, "{channel_name}-part{:03}.wav", name.part_index + 1)
        } else {
            write!(output, "{channel_name}.wav")
        }
    } else {
        write!(
            output,
            "lamb-{}-{channel_name}-{}Hz-{:09}-{:09}-part{:03}.wav",
            name.timestamp,
            name.sample_rate,
            name.start_frame,
            name.start_frame + name.frame_count,
            name.part_index + 1
        )
    }
}

enum ChannelName<'a> {
    Configured(&'a str),
    Default(usize),
}

impl fmt::Display for ChannelName<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configured(name) => formatter.write_str(name),
            Self::Default(index) => write!(formatter, "ch{index:02}"),
        }
    }
}

fn create_unique_directory(parent: &Path, prefix: &str) -> Result<(String, OwnedPath)> {
    loop {
        let id = transaction_id();
        let path = parent.join(format!("{prefix}{id}"));
        match fs::create_dir(&path) {
            Ok(()) => {
                let owned =
                    OwnedPath::capture(path.clone()).map_err(|source| io_error(&path, source))?;
                return Ok((id, owned));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&path, error)),
        }
    }
}

fn transaction_id() -> String {
    let sequence = NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("{:x}-{nanos:x}-{sequence:x}", std::process::id())
}

#[cfg(target_os = "linux")]
fn rename_no_replace(old: &Path, new: &Path) -> Result<()> {
    let old_c = CString::new(old.as_os_str().as_bytes())
        .map_err(|error| io_error(old, io::Error::new(io::ErrorKind::InvalidInput, error)))?;
    let new_c = CString::new(new.as_os_str().as_bytes())
        .map_err(|error| io_error(new, io::Error::new(io::ErrorKind::InvalidInput, error)))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old_c.as_ptr(),
            libc::AT_FDCWD,
            new_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(new, io::Error::last_os_error()))
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_old: &Path, _new: &Path) -> Result<()> {
    Err(LambError::Export(
        "atomic no-overwrite publication requires Linux renameat2".to_string(),
    ))
}

fn write_u16le(writer: &mut impl Write, value: u16) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32le(writer: &mut impl Write, value: u32) -> std::io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

pub(crate) fn f32_to_s24_bytes(sample: f32) -> [u8; 3] {
    let clamped = sample.clamp(-1.0, 1.0);
    let scaled = if clamped >= 0.0 {
        (clamped * 8_388_607.0).round() as i32
    } else {
        (clamped * 8_388_608.0).round() as i32
    };
    let bounded = scaled.clamp(-8_388_608, 8_388_607);
    let bytes = bounded.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

fn write_mono_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<OwnedPath> {
    let data_bytes = (samples.len() as u64)
        .checked_mul(3)
        .ok_or_else(|| LambError::Export("WAV data size overflow".to_string()))?;
    if data_bytes > u64::from(u32::MAX - 36) {
        return Err(LambError::Export(
            "classic WAV data exceeds RIFF size limit".to_string(),
        ));
    }

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let owned =
        OwnedPath::from_file(path.to_path_buf(), &file).map_err(|source| io_error(path, source))?;
    let mut writer = BufWriter::new(file);

    let write_result = (|| {
        writer
            .write_all(b"RIFF")
            .map_err(|source| io_error(path, source))?;
        write_u32le(&mut writer, 36 + data_bytes as u32)
            .map_err(|source| io_error(path, source))?;
        writer
            .write_all(b"WAVE")
            .map_err(|source| io_error(path, source))?;

        writer
            .write_all(b"fmt ")
            .map_err(|source| io_error(path, source))?;
        write_u32le(&mut writer, 16).map_err(|source| io_error(path, source))?;
        write_u16le(&mut writer, 1).map_err(|source| io_error(path, source))?;
        write_u16le(&mut writer, 1).map_err(|source| io_error(path, source))?;
        write_u32le(&mut writer, sample_rate).map_err(|source| io_error(path, source))?;
        write_u32le(&mut writer, sample_rate * 3).map_err(|source| io_error(path, source))?;
        write_u16le(&mut writer, 3).map_err(|source| io_error(path, source))?;
        write_u16le(&mut writer, 24).map_err(|source| io_error(path, source))?;

        writer
            .write_all(b"data")
            .map_err(|source| io_error(path, source))?;
        write_u32le(&mut writer, data_bytes as u32).map_err(|source| io_error(path, source))?;

        for sample in samples {
            writer
                .write_all(&f32_to_s24_bytes(*sample))
                .map_err(|source| io_error(path, source))?;
        }
        writer.flush().map_err(|source| io_error(path, source))?;
        let file = writer
            .into_inner()
            .map_err(|error| io_error(path, error.into_error()))?;
        file.sync_all().map_err(|source| io_error(path, source))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        best_effort_remove_owned_file(&owned);
        return Err(error);
    }
    Ok(owned)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn cleanup_removes_transaction_owned_file_and_directory() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("owned.partial");
        fs::write(&file_path, b"owned").unwrap();
        let owned_file = OwnedPath::capture(file_path.clone()).unwrap();
        best_effort_remove_owned_file(&owned_file);
        assert!(!file_path.exists());

        let directory_path = root.path().join("owned-staging");
        fs::create_dir(&directory_path).unwrap();
        fs::write(directory_path.join("audio.wav"), b"owned").unwrap();
        let owned_directory = OwnedPath::capture(directory_path.clone()).unwrap();
        best_effort_remove_owned_directory(&owned_directory);
        assert!(!directory_path.exists());
    }

    #[test]
    fn cleanup_preserves_file_and_directory_replaced_with_different_inodes() {
        let root = tempfile::tempdir().unwrap();
        let file_path = root.path().join("replaced.partial");
        let original_file = File::create(&file_path).unwrap();
        let owned_file = OwnedPath::capture(file_path.clone()).unwrap();
        fs::remove_file(&file_path).unwrap();
        fs::write(&file_path, b"replacement").unwrap();
        best_effort_remove_owned_file(&owned_file);
        assert_eq!(fs::read(&file_path).unwrap(), b"replacement");
        drop(original_file);

        let directory_path = root.path().join("replaced-staging");
        fs::create_dir(&directory_path).unwrap();
        let original_directory = File::open(&directory_path).unwrap();
        let owned_directory = OwnedPath::capture(directory_path.clone()).unwrap();
        fs::remove_dir(&directory_path).unwrap();
        fs::create_dir(&directory_path).unwrap();
        fs::write(directory_path.join("replacement"), b"preserve").unwrap();
        best_effort_remove_owned_directory(&owned_directory);
        assert_eq!(
            fs::read(directory_path.join("replacement")).unwrap(),
            b"preserve"
        );
        drop(original_directory);
    }
}
