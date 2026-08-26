use crate::dump::{PublishedOutput, SampleSnapshot};
use crate::error::{io_error, LambError, Result};
use crate::math::wav_parts_for_channel;
use crate::persistence_workspace::{
    FilePlan, IndeterminatePublication, ManifestScratch, OwnedTransactionArtifacts,
    PreparedPersistence, PublicationPathScratch, PublicationViews,
};
use crate::recovery::{
    capture_identity, sync_directory, verify_manifest_matches, FileIdentity as ManifestIdentity,
    ManifestEntrySlot, ManifestPhase, ManifestStore, PathRef, TransactionKind, TransactionManifest,
    MANIFEST_VERSION,
};
use crate::sample_ring::Snapshot;
use std::collections::HashSet;
use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct TrustedFileSetRoot<'a> {
    output_path: &'a Path,
    anchor_prefix_len: usize,
    anchor_identity: FileIdentity,
    output: File,
    output_identity: FileIdentity,
}

impl<'a> TrustedFileSetRoot<'a> {
    fn open(
        output_path: &'a Path,
        component: &mut [u8],
        hook: &mut impl PreparedPublicationHook,
    ) -> Result<Self> {
        if !output_path.is_absolute() {
            return Err(LambError::Validation(
                "file-set output root must be absolute".to_string(),
            ));
        }
        let output_text = path_to_str(output_path, "file-set output root")?;
        let mut directory = open_absolute_root(output_path)?;
        let mut anchor_prefix_len = 1;
        let mut traversal_prefix_len = 1;
        let mut anchor_identity = None;
        let mut missing = false;
        for part in output_path.components() {
            match part {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    let next_prefix_len =
                        next_component_prefix_len(output_text, traversal_prefix_len, name)?;
                    let name = prepare_component(component, name)
                        .map_err(|source| io_error(output_path, source))?;
                    let (next, created) = if missing {
                        mkdir_at(&directory, name, output_path)?;
                        (
                            open_directory_at(&directory, name)
                                .map_err(|source| io_error(output_path, source))?,
                            true,
                        )
                    } else {
                        match open_directory_at(&directory, name) {
                            Ok(next) => (next, false),
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                                anchor_prefix_len = traversal_prefix_len;
                                let anchor_path = path_prefix(output_text, anchor_prefix_len)?;
                                let metadata = directory
                                    .metadata()
                                    .map_err(|source| io_error(anchor_path, source))?;
                                anchor_identity = Some(FileIdentity {
                                    device: metadata.dev(),
                                    inode: metadata.ino(),
                                });
                                missing = true;
                                mkdir_at(&directory, name, output_path)?;
                                (
                                    open_directory_at(&directory, name)
                                        .map_err(|source| io_error(output_path, source))?,
                                    true,
                                )
                            }
                            Err(source) => return Err(io_error(output_path, source)),
                        }
                    };
                    let parent_path = path_prefix(output_text, traversal_prefix_len)?;
                    directory
                        .sync_all()
                        .map_err(|source| io_error(parent_path, source))?;
                    if created {
                        hook.sync_directory(parent_path)?;
                    }
                    directory = next;
                    traversal_prefix_len = next_prefix_len;
                }
                _ => {
                    return Err(LambError::Validation(
                        "file-set output root is not a normalized absolute path".to_string(),
                    ))
                }
            }
        }
        if !missing {
            anchor_prefix_len = traversal_prefix_len;
        }
        let output_metadata = directory
            .metadata()
            .map_err(|source| io_error(output_path, source))?;
        Ok(Self {
            output_path,
            anchor_prefix_len,
            anchor_identity: anchor_identity.unwrap_or(FileIdentity {
                device: output_metadata.dev(),
                inode: output_metadata.ino(),
            }),
            output: directory,
            output_identity: FileIdentity {
                device: output_metadata.dev(),
                inode: output_metadata.ino(),
            },
        })
    }

    fn anchor_path(&self) -> Result<&Path> {
        path_prefix(
            path_to_str(self.output_path, "file-set output root")?,
            self.anchor_prefix_len,
        )
    }

    fn verify(&self, component: &mut [u8]) -> Result<()> {
        let anchor_path = self.anchor_path()?;
        let reopened = open_existing_absolute_directory(anchor_path, component)?;
        let metadata = reopened
            .metadata()
            .map_err(|source| io_error(anchor_path, source))?;
        if metadata.dev() != self.anchor_identity.device
            || metadata.ino() != self.anchor_identity.inode
        {
            return Err(LambError::ExportInvariant(
                "file-set output anchor identity changed during publication",
            ));
        }
        let output = open_existing_absolute_directory(self.output_path, component)?;
        let metadata = output
            .metadata()
            .map_err(|source| io_error(self.output_path, source))?;
        if metadata.dev() != self.output_identity.device
            || metadata.ino() != self.output_identity.inode
        {
            return Err(LambError::ExportInvariant(
                "configured file-set output root identity changed during publication",
            ));
        }
        Ok(())
    }

    fn output_root(&self, component: &mut [u8]) -> Result<File> {
        self.verify(component)?;
        self.output
            .try_clone()
            .map_err(|source| io_error(self.output_path, source))
    }

    fn open_relative_directory(&self, relative: &Path, component: &mut [u8]) -> Result<File> {
        let mut directory = self.output_root(component)?;
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(LambError::ExportInvariant(
                    "file-set relative directory contains a non-normal component",
                ));
            };
            let name = prepare_component(component, name)
                .map_err(|source| io_error(self.output_path, source))?;
            directory = open_directory_at(&directory, name)
                .map_err(|source| io_error(self.output_path, source))?;
        }
        Ok(directory)
    }

    fn ensure_final_absent(&self, final_path: &Path, component: &mut [u8]) -> Result<()> {
        let parent = final_path.parent().ok_or(LambError::ExportInvariant(
            "prepared file-set final path has no parent",
        ))?;
        let relative = parent.strip_prefix(self.output_path).map_err(|_| {
            LambError::Validation("file-set final path escapes output root".to_string())
        })?;
        let mut directory = self.output_root(component)?;
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(LambError::Validation(
                    "file-set parent is not lexically contained".to_string(),
                ));
            };
            let name = prepare_component(component, name)
                .map_err(|source| io_error(final_path, source))?;
            directory = match open_directory_at_if_exists(&directory, name) {
                Ok(Some(next)) => next,
                Ok(None) => return Ok(()),
                Err(source) => return Err(io_error(final_path, source)),
            };
        }
        let final_name = final_path.file_name().ok_or(LambError::ExportInvariant(
            "prepared file-set final path has no filename",
        ))?;
        let final_name = prepare_component(component, final_name)
            .map_err(|source| io_error(final_path, source))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                final_name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            Err(io_error(
                final_path,
                io::Error::new(io::ErrorKind::AlreadyExists, "final path already exists"),
            ))
        } else {
            let errno = last_errno();
            if errno == libc::ENOENT {
                Ok(())
            } else {
                Err(io_error(final_path, io::Error::from_raw_os_error(errno)))
            }
        }
    }

    fn materialize_directory_slot(
        &self,
        relative: &Path,
        component: &mut [u8],
    ) -> Result<(File, bool)> {
        let name = relative.file_name().ok_or(LambError::ExportInvariant(
            "file-set directory intent has no final component",
        ))?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent_directory = self.open_relative_directory(parent, component)?;
        let name = prepare_component(component, name)
            .map_err(|source| io_error(self.output_path, source))?;
        match open_directory_at(&parent_directory, name) {
            Ok(directory) => Ok((directory, false)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                mkdir_at(&parent_directory, name, self.output_path)?;
                let directory = open_directory_at(&parent_directory, name)
                    .map_err(|source| io_error(self.output_path, source))?;
                Ok((directory, true))
            }
            Err(source) => Err(io_error(self.output_path, source)),
        }
    }
}

fn open_absolute_root(path: &Path) -> Result<File> {
    let root = c"/";
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io_error(path, io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_existing_absolute_directory(path: &Path, component: &mut [u8]) -> Result<File> {
    let mut directory = open_absolute_root(path)?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory_at(
                    &directory,
                    prepare_component(component, name).map_err(|source| io_error(path, source))?,
                )
                .map_err(|source| io_error(path, source))?;
            }
            _ => {
                return Err(LambError::Validation(
                    "absolute directory path is not normalized".to_string(),
                ))
            }
        }
    }
    Ok(directory)
}

fn prepare_component<'a>(buffer: &'a mut [u8], name: &OsStr) -> io::Result<&'a CStr> {
    let bytes = name.as_bytes();
    if bytes.contains(&0) || bytes.len() >= buffer.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "component contains NUL or exceeds scratch capacity",
        ));
    }
    buffer[..bytes.len()].copy_from_slice(bytes);
    buffer[bytes.len()] = 0;
    CStr::from_bytes_with_nul(&buffer[..=bytes.len()])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

fn prepare_component_pair<'a>(
    first: &'a mut [u8],
    second: &'a mut [u8],
    old: &OsStr,
    new: &OsStr,
) -> io::Result<(&'a CStr, &'a CStr)> {
    Ok((
        prepare_component(first, old)?,
        prepare_component(second, new)?,
    ))
}

fn path_prefix(text: &str, prefix_len: usize) -> Result<&Path> {
    let prefix = text.get(..prefix_len).ok_or(LambError::ExportInvariant(
        "file-set path prefix is not a UTF-8 boundary",
    ))?;
    Ok(Path::new(prefix))
}

fn next_component_prefix_len(text: &str, prefix_len: usize, name: &OsStr) -> Result<usize> {
    let start = if prefix_len == 1 { 1 } else { prefix_len + 1 };
    let end = start
        .checked_add(name.as_bytes().len())
        .ok_or(LambError::ExportInvariant("file-set path prefix overflow"))?;
    if text.get(start..end)
        != Some(name.to_str().ok_or(LambError::Validation(
            "file-set path component is not valid UTF-8".to_string(),
        ))?)
    {
        return Err(LambError::ExportInvariant(
            "file-set path is not normalized",
        ));
    }
    Ok(end)
}

fn open_directory_at(parent: &File, name: &CStr) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_directory_at_if_exists(parent: &File, name: &CStr) -> io::Result<Option<File>> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd >= 0 {
        Ok(Some(unsafe { File::from_raw_fd(fd) }))
    } else {
        let errno = last_errno();
        if errno == libc::ENOENT {
            Ok(None)
        } else {
            Err(io::Error::from_raw_os_error(errno))
        }
    }
}

#[cfg(target_os = "linux")]
fn last_errno() -> i32 {
    unsafe { *libc::__errno_location() }
}

#[cfg(not(target_os = "linux"))]
fn last_errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn mkdir_at(parent: &File, name: &CStr, error_path: &Path) -> Result<()> {
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(error_path, io::Error::last_os_error()))
    }
}

fn create_new_file_at(parent: &File, name: &CStr, error_path: &Path) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666,
        )
    };
    if fd < 0 {
        Err(io_error(error_path, io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_regular_file_at(parent: &File, name: &CStr, error_path: &Path) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io_error(error_path, io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|source| io_error(error_path, source))?;
    if !metadata.file_type().is_file() {
        return Err(LambError::ExportInvariant(
            "prepared publication file is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(target_os = "linux")]
fn rename_no_replace_at(
    parent: &File,
    old_name: &CStr,
    new_name: &CStr,
    error_path: &Path,
) -> Result<()> {
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            old_name.as_ptr(),
            parent.as_raw_fd(),
            new_name.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(error_path, io::Error::last_os_error()))
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace_at(
    _parent: &File,
    _old_name: &std::ffi::OsStr,
    _new_name: &std::ffi::OsStr,
    _error_path: &Path,
) -> Result<()> {
    Err(LambError::Export(
        "atomic no-overwrite publication requires Linux renameat2".to_string(),
    ))
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
    Published,
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

    fn sync_preopened_directory(&mut self, directory: &File, path: &Path) -> Result<()> {
        directory
            .sync_all()
            .map_err(|source| io_error(path, source))
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
    let staging_identity = staging.staging_identity();
    if let Err(error) = staging.prepare_recall_manifest_path() {
        return PreparedPublication::RetryableFailure(error);
    }
    let views = staging.publication_views();
    let PublicationViews {
        files: planned,
        manifest:
            ManifestScratch {
                slots,
                directories,
                path_bytes,
                serialization,
            },
        paths:
            PublicationPathScratch {
                partial,
                manifest: manifest_path,
            },
        scratch,
    } = views;
    if planned.is_empty() {
        return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
            "prepared recall has no files",
        ));
    }
    let output_directory = planned.final_root();
    let first = planned.get(0).expect("file plan is nonempty");
    let staging_directory = match first.staged_path().parent() {
        Some(parent) => parent,
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared recall staging path has no parent",
            ))
        }
    };
    let transaction_id = match staging_directory.file_name().and_then(|name| name.to_str()) {
        Some(transaction_id) => transaction_id,
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared recall transaction id is invalid",
            ))
        }
    };
    let publication = publish_recall_inner(
        slots,
        directories,
        path_bytes,
        serialization,
        hook,
        planned,
        staging_directory,
        transaction_id,
        output_directory,
        staging_identity,
        partial,
        manifest_path.as_path(),
        scratch,
    );
    match publication {
        Ok(_published) => {
            staging.defer_completed_cleanup(TransactionKind::FileSet);
            PreparedPublication::Published
        }
        Err(error) => {
            if error.durable_failure {
                let cleanup = staging.indeterminate(TransactionKind::FileSet);
                staging.defer_recovery();
                return PreparedPublication::Indeterminate {
                    operation: error.operation,
                    cleanup,
                };
            }
            PreparedPublication::RetryableFailure(error.operation)
        }
    }
}

struct RecallPublishError {
    operation: LambError,
    durable_failure: bool,
}

impl RecallPublishError {
    fn retryable(operation: LambError) -> Self {
        Self {
            operation,
            durable_failure: false,
        }
    }

    fn durable(operation: LambError) -> Self {
        Self {
            operation,
            durable_failure: true,
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
    planned: FilePlan<'_>,
    staging_directory: &Path,
    transaction_id: &str,
    output_directory: &Path,
    staging_identity: (u64, u64),
    partial_path: &mut crate::persistence_workspace::ReusablePath,
    manifest_path: &Path,
    scratch: &mut crate::persistence_workspace::PublicationScratch,
) -> std::result::Result<(), RecallPublishError> {
    let trusted_root = TrustedFileSetRoot::open(output_directory, scratch.component_a(), hook)
        .map_err(RecallPublishError::retryable)?;
    let manifest_parent = File::open(staging_directory)
        .map_err(|source| RecallPublishError::retryable(io_error(staging_directory, source)))?;
    let mut manifest_identity = None;
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

    for index in 0..planned.len() {
        let file = planned.get(index).expect("file plan length is stable");
        let staged_path = file.staged_path();
        let final_path = file.final_path();
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
        partial_path
            .set_path(partial_parent)
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_separator()
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_bytes(b".")
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_bytes(file_name.as_bytes())
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_bytes(b".")
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_bytes(transaction_id.as_bytes())
            .map_err(RecallPublishError::retryable)?;
        partial_path
            .push_bytes(b".partial")
            .map_err(RecallPublishError::retryable)?;
        let staged_ref = manifest
            .push_path(
                path_to_str(staged_path, "staged path").map_err(RecallPublishError::retryable)?,
            )
            .map_err(RecallPublishError::retryable)?;
        let staged_name = staged_path.file_name().ok_or_else(|| {
            RecallPublishError::retryable(LambError::ExportInvariant(
                "prepared staged path has no filename",
            ))
        })?;
        let staged_file = open_regular_file_at(
            &manifest_parent,
            prepare_component(scratch.component_a(), staged_name)
                .map_err(|e| RecallPublishError::retryable(io_error(staged_path, e)))?,
            staged_path,
        )
        .map_err(RecallPublishError::retryable)?;
        let staged_metadata = staged_file
            .metadata()
            .map_err(|e| RecallPublishError::retryable(io_error(staged_path, e)))?;
        let staged_identity = ManifestIdentity {
            device: staged_metadata.dev(),
            inode: staged_metadata.ino(),
        };
        let partial_ref = manifest
            .push_path(
                path_to_str(partial_path.as_path(), "partial path")
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

    for index in 0..planned.len() {
        let file = planned.get(index).expect("file plan length is stable");
        let final_path = file.final_path();
        trusted_root
            .ensure_final_absent(final_path, scratch.component_a())
            .map_err(RecallPublishError::retryable)?;
    }

    // Preflight all candidates before mutating final publication state.  The
    // manifest records every absent candidate as an identity-unknown intent;
    // a crash before identity capture is consequently conservative.
    {
        let (component, sync_slots) = scratch.component_a_and_sync_slots();
        add_directory_sync(
            sync_slots,
            planned,
            0,
            path_to_str(output_directory, "output directory")
                .map_err(RecallPublishError::retryable)?
                .len(),
        )
        .map_err(RecallPublishError::retryable)?;
        for entry_index in 0..planned.len() {
            let file = planned
                .get(entry_index)
                .expect("file plan length is stable");
            let final_path = file.final_path();
            register_file_set_parent_intents(
                &mut manifest,
                &trusted_root,
                component,
                sync_slots,
                planned,
                output_directory,
                entry_index,
                final_path,
            )
            .map_err(RecallPublishError::retryable)?;
        }
    }
    if let Err(operation) = persist_manifest_prepared_scratch(
        &mut *serialization,
        manifest_path,
        &manifest,
        &manifest_parent,
        transaction_id,
        &mut manifest_identity,
        scratch,
        hook,
    ) {
        return Err(if manifest_identity.is_some() {
            RecallPublishError::durable(operation)
        } else {
            RecallPublishError::retryable(operation)
        });
    }
    hook.checkpoint(PublicationCheckpoint::RecallManifestPrepared)
        .map_err(RecallPublishError::durable)?;
    trusted_root
        .verify(scratch.component_a())
        .map_err(RecallPublishError::durable)?;

    if let Err(operation) = materialize_file_set_parents(
        &mut manifest,
        &trusted_root,
        output_directory,
        &manifest_parent,
        transaction_id,
        &mut manifest_identity,
        scratch,
        manifest_path,
        serialization,
        hook,
    ) {
        return Err(RecallPublishError::durable(operation));
    }

    let publication = (|| {
        for index in 0..planned.len() {
            let file = planned.get(index).expect("file plan length is stable");
            let staged_path = file.staged_path();
            let final_path = file.final_path();
            manifest.phase = ManifestPhase::Publishing { index };
            persist_manifest_prepared_scratch(
                &mut *serialization,
                manifest_path,
                &manifest,
                &manifest_parent,
                transaction_id,
                &mut manifest_identity,
                scratch,
                hook,
            )?;
            partial_path.set_path(manifest.path(manifest.entry(index).partial_path))?;
            let final_parent = final_path.parent().ok_or(LambError::ExportInvariant(
                "prepared file-set final path has no parent",
            ))?;
            let relative_parent = final_parent.strip_prefix(output_directory).map_err(|_| {
                LambError::Validation("file-set final path escapes output root".to_string())
            })?;
            trusted_root.verify(scratch.component_a())?;
            let parent_directory =
                trusted_root.open_relative_directory(relative_parent, scratch.component_a())?;
            let partial_name =
                partial_path
                    .as_path()
                    .file_name()
                    .ok_or(LambError::ExportInvariant(
                        "prepared file-set partial path has no filename",
                    ))?;
            let final_name = final_path.file_name().ok_or(LambError::ExportInvariant(
                "prepared file-set final path has no filename",
            ))?;
            let staged_name = staged_path.file_name().ok_or(LambError::ExportInvariant(
                "prepared staged path has no filename",
            ))?;
            let mut source = open_regular_file_at(
                &manifest_parent,
                prepare_component(scratch.component_a(), staged_name)
                    .map_err(|e| io_error(staged_path, e))?,
                staged_path,
            )?;
            let partial_component = prepare_component(scratch.component_a(), partial_name)
                .map_err(|source| io_error(partial_path.as_path(), source))?;
            let mut partial =
                create_new_file_at(&parent_directory, partial_component, partial_path.as_path())?;
            let partial_metadata = partial
                .metadata()
                .map_err(|source| io_error(partial_path.as_path(), source))?;
            *scratch.current_artifact() = crate::persistence_workspace::CurrentArtifactSlot {
                path: manifest.entry(index).partial_path,
                identity: Some(ManifestIdentity {
                    device: partial_metadata.dev(),
                    inode: partial_metadata.ino(),
                }),
                quarantine_parent_identity: None,
                quarantine_identity: None,
                final_name: false,
                quarantine_created: false,
                quarantine_setup_complete: false,
                quarantine_handoff: false,
                quarantine_artifact_removed: false,
                quarantine_removed: false,
            };
            io::copy(&mut source, &mut partial)
                .map_err(|source| io_error(partial_path.as_path(), source))?;
            partial
                .flush()
                .map_err(|source| io_error(partial_path.as_path(), source))?;
            partial
                .sync_all()
                .map_err(|source| io_error(partial_path.as_path(), source))?;
            hook.checkpoint(PublicationCheckpoint::RecallPartialCreatedBeforeManifest { index })?;
            manifest.entry_mut(index).partial_identity = Some(ManifestIdentity {
                device: partial_metadata.dev(),
                inode: partial_metadata.ino(),
            });
            persist_manifest_prepared_scratch(
                &mut *serialization,
                manifest_path,
                &manifest,
                &manifest_parent,
                transaction_id,
                &mut manifest_identity,
                scratch,
                hook,
            )?;
            *scratch.current_artifact() =
                crate::persistence_workspace::CurrentArtifactSlot::default();
            hook.checkpoint(PublicationCheckpoint::RecallPartialSynced { index })?;
            hook.checkpoint(PublicationCheckpoint::RecallBeforeFinalRename { index })?;
            hook.before_rename(index, final_path)?;
            trusted_root.verify(scratch.component_a())?;
            let (old_name, new_name) = {
                let (first, second) = scratch.component_pair();
                prepare_component_pair(first, second, partial_name, final_name)
                    .map_err(|source| io_error(final_path, source))?
            };
            rename_no_replace_at(&parent_directory, old_name, new_name, final_path)?;
            *scratch.current_artifact() = crate::persistence_workspace::CurrentArtifactSlot {
                path: manifest.entry(index).final_path,
                identity: Some(ManifestIdentity {
                    device: partial_metadata.dev(),
                    inode: partial_metadata.ino(),
                }),
                quarantine_parent_identity: None,
                quarantine_identity: None,
                final_name: true,
                quarantine_created: false,
                quarantine_setup_complete: false,
                quarantine_handoff: false,
                quarantine_artifact_removed: false,
                quarantine_removed: false,
            };
            hook.checkpoint(PublicationCheckpoint::RecallRenamedBeforeManifest { index })?;
            manifest.entry_mut(index).final_identity = Some(ManifestIdentity {
                device: partial_metadata.dev(),
                inode: partial_metadata.ino(),
            });
            manifest.entry_mut(index).partial_identity = None;
            persist_manifest_prepared_scratch(
                &mut *serialization,
                manifest_path,
                &manifest,
                &manifest_parent,
                transaction_id,
                &mut manifest_identity,
                scratch,
                hook,
            )?;
            *scratch.current_artifact() =
                crate::persistence_workspace::CurrentArtifactSlot::default();
            hook.checkpoint(PublicationCheckpoint::RecallAfterFinalRename { index })?;
        }
        Ok(())
    })();

    if let Err(operation) = publication {
        return Err(RecallPublishError::durable(operation));
    }

    for index in 0..planned.len() {
        let file = planned.get(index).expect("file plan length is stable");
        let final_path = file.final_path();
        let parent = final_path.parent().ok_or_else(|| {
            RecallPublishError::durable(LambError::ExportInvariant("prepared final has no parent"))
        })?;
        let relative = parent.strip_prefix(output_directory).map_err(|_| {
            RecallPublishError::durable(LambError::Validation(
                "file-set final path escapes output root".to_string(),
            ))
        })?;
        let directory = trusted_root
            .open_relative_directory(relative, scratch.component_a())
            .map_err(RecallPublishError::durable)?;
        let name = final_path.file_name().ok_or_else(|| {
            RecallPublishError::durable(LambError::ExportInvariant(
                "prepared final has no filename",
            ))
        })?;
        open_regular_file_at(
            &directory,
            prepare_component(scratch.component_a(), name)
                .map_err(|e| RecallPublishError::durable(io_error(final_path, e)))?,
            final_path,
        )
        .and_then(|file| file.sync_all().map_err(|e| io_error(final_path, e)))
        .map_err(RecallPublishError::durable)?;
    }
    hook.checkpoint(PublicationCheckpoint::RecallFilesSynced)
        .map_err(RecallPublishError::durable)?;
    for slot_index in 0..scratch.sync_slots().len() {
        let slot = scratch.sync_slots()[slot_index];
        if !slot.active {
            continue;
        }
        let file = planned
            .get(slot.entry_index as usize)
            .expect("directory sync entry is valid");
        let final_text = path_to_str(file.final_path(), "prepared final path")
            .map_err(RecallPublishError::durable)?;
        let parent = path_prefix(final_text, slot.prefix_len as usize)
            .map_err(RecallPublishError::durable)?;
        trusted_root
            .verify(scratch.component_a())
            .map_err(RecallPublishError::durable)?;
        let relative = parent.strip_prefix(output_directory).map_err(|_| {
            RecallPublishError::durable(LambError::Validation(
                "file-set sync directory escapes output root".to_string(),
            ))
        })?;
        let directory = trusted_root
            .open_relative_directory(relative, scratch.component_a())
            .map_err(RecallPublishError::durable)?;
        directory
            .sync_all()
            .map_err(|source| RecallPublishError::durable(io_error(parent, source)))?;
        hook.sync_directory(parent)
            .map_err(RecallPublishError::durable)?;
        trusted_root
            .verify(scratch.component_a())
            .map_err(RecallPublishError::durable)?;
    }
    hook.checkpoint(PublicationCheckpoint::RecallOutputSynced)
        .map_err(RecallPublishError::durable)?;
    manifest.phase = ManifestPhase::Complete;
    persist_manifest_prepared_scratch(
        &mut *serialization,
        manifest_path,
        &manifest,
        &manifest_parent,
        transaction_id,
        &mut manifest_identity,
        scratch,
        hook,
    )
    .map_err(RecallPublishError::durable)?;
    hook.checkpoint(PublicationCheckpoint::RecallCompleteRecorded)
        .map_err(RecallPublishError::durable)?;
    let _ = verify_manifest_matches(&mut *serialization, manifest_path, &manifest)
        .map_err(RecallPublishError::durable)?;
    trusted_root
        .verify(scratch.component_a())
        .map_err(RecallPublishError::durable)?;

    Ok(())
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
#[allow(clippy::too_many_arguments)]
fn register_file_set_parent_intents(
    manifest: &mut TransactionManifest,
    trusted_root: &TrustedFileSetRoot,
    component: &mut [u8],
    sync_slots: &mut [crate::persistence_workspace::DirectorySyncSlot],
    planned: FilePlan<'_>,
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
    trusted_root.verify(component)?;
    let mut directory = Some(trusted_root.output_root(component)?);
    let mut prefix_len = path_to_str(output_root, "file-set output root")?.len();
    let final_text = path_to_str(final_path, "file-set final path")?;
    for part in relative.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(LambError::Validation(
                "file-set parent is not lexically contained".to_string(),
            ));
        }
        let name = part.as_os_str();
        prefix_len = next_component_prefix_len(final_text, prefix_len, name)?;
        add_directory_sync(sync_slots, planned, entry_index, prefix_len)?;
        let next = match directory.as_ref() {
            Some(parent) => match open_directory_at(
                parent,
                prepare_component(component, name)
                    .map_err(|source| io_error(final_path, source))?,
            ) {
                Ok(next) => Some(next),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(source) => return Err(io_error(final_path, source)),
            },
            None => None,
        };
        if next.is_none() {
            let path = path_prefix(final_text, prefix_len)?;
            if !manifest.directories().iter().any(|slot| {
                manifest
                    .directory_path(slot)
                    .is_ok_and(|candidate| candidate == path)
            }) {
                manifest.push_directory_intent(entry_index, prefix_len)?;
            }
        }
        directory = next;
    }
    Ok(())
}

fn add_directory_sync(
    slots: &mut [crate::persistence_workspace::DirectorySyncSlot],
    planned: FilePlan<'_>,
    entry_index: usize,
    prefix_len: usize,
) -> Result<()> {
    let candidate_file = planned.get(entry_index).ok_or(LambError::ExportInvariant(
        "directory sync entry is invalid",
    ))?;
    let candidate = path_prefix(
        path_to_str(candidate_file.final_path(), "prepared final path")?,
        prefix_len,
    )?;
    for slot in slots.iter().filter(|slot| slot.active) {
        let existing = planned
            .get(slot.entry_index as usize)
            .ok_or(LambError::ExportInvariant(
                "directory sync entry is invalid",
            ))?;
        if path_prefix(
            path_to_str(existing.final_path(), "prepared final path")?,
            slot.prefix_len as usize,
        )? == candidate
        {
            return Ok(());
        }
    }
    let slot = slots
        .iter_mut()
        .find(|slot| !slot.active)
        .ok_or(LambError::ExportInvariant(
            "prepared directory sync capacity exhausted; retry transaction",
        ))?;
    *slot = crate::persistence_workspace::DirectorySyncSlot {
        entry_index: entry_index as u32,
        prefix_len: prefix_len as u32,
        active: true,
    };
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_file_set_parents(
    manifest: &mut TransactionManifest,
    trusted_root: &TrustedFileSetRoot,
    output_root: &Path,
    manifest_parent: &File,
    transaction_id: &str,
    current_manifest_identity: &mut Option<ManifestIdentity>,
    scratch: &mut crate::persistence_workspace::PublicationScratch,
    manifest_path: &Path,
    serialization: &mut [u8],
    hook: &mut impl PreparedPublicationHook,
) -> Result<()> {
    trusted_root.verify(scratch.component_a())?;
    let _output_directory = trusted_root.output_root(scratch.component_a())?;
    for index in 0..manifest.directory_count {
        let directory = manifest.directories()[index];
        let path = manifest.directory_path(&directory)?;
        let relative = path.strip_prefix(output_root).map_err(|_| {
            LambError::Validation("file-set parent escapes output root".to_string())
        })?;
        let (directory_file, created) =
            trusted_root.materialize_directory_slot(relative, scratch.component_a())?;
        if created {
            // If we crash before this capture/update, the durable Intent is
            // intentionally left identity-unknown and recovery will not rmdir it.
            hook.checkpoint(
                PublicationCheckpoint::RecallParentCreatedBeforeOwnedManifest { index },
            )?;
            let metadata = directory_file
                .metadata()
                .map_err(|source| io_error(path, source))?;
            let identity = ManifestIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            let slot = manifest.directory_mut(index)?;
            slot.state = Some(crate::recovery::ManifestDirectoryState::Owned);
            slot.identity = Some(identity);
            persist_manifest_prepared_scratch(
                serialization,
                manifest_path,
                manifest,
                manifest_parent,
                transaction_id,
                current_manifest_identity,
                scratch,
                hook,
            )?;
            hook.checkpoint(PublicationCheckpoint::RecallParentOwnedManifestRecorded { index })?;
        }
    }
    Ok(())
}

fn publish_prepared_dump(
    mut staging: OwnedTransactionArtifacts<'_>,
    hook: &mut impl PreparedPublicationHook,
) -> PreparedPublication {
    let staging_identity = staging.staging_identity();
    if let Err(error) = staging.prepare_atomic_manifest_path() {
        return PreparedPublication::RetryableFailure(error);
    }
    let views = staging.publication_views();
    let PublicationViews {
        files: planned,
        manifest:
            ManifestScratch {
                slots,
                directories: _,
                path_bytes,
                serialization,
            },
        paths:
            PublicationPathScratch {
                partial: _,
                manifest: manifest_path,
            },
        scratch,
    } = views;
    if planned.is_empty() {
        return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
            "prepared dump has no files",
        ));
    }
    let first = planned.get(0).expect("file plan is nonempty");
    let staging_directory = match first.staged_path().parent() {
        Some(parent) => parent,
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump staging path has no parent",
            ))
        }
    };
    let output_directory = match first.final_path().parent() {
        Some(parent) => parent,
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump final path has no parent",
            ))
        }
    };
    let output_parent = match output_directory.parent() {
        Some(parent) => parent,
        None => {
            return PreparedPublication::RetryableFailure(LambError::ExportInvariant(
                "prepared dump final directory has no parent",
            ))
        }
    };
    let publication = publish_dump_inner(
        slots,
        path_bytes,
        serialization,
        hook,
        planned,
        staging_directory,
        output_directory,
        output_parent,
        manifest_path.as_path(),
        staging_identity,
        scratch,
    );
    match publication {
        Ok(()) => {
            staging.defer_completed_cleanup(TransactionKind::AtomicDirectory);
            PreparedPublication::Published
        }
        Err((error, Some(_durable))) => {
            let cleanup = staging.indeterminate(TransactionKind::AtomicDirectory);
            staging.defer_recovery();
            PreparedPublication::Indeterminate {
                operation: error,
                cleanup,
            }
        }
        Err((error, None)) => PreparedPublication::RetryableFailure(error),
    }
}

type DumpFailure = (LambError, Option<()>);

fn dump_durable() -> Option<()> {
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn publish_dump_inner(
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
    serialization: &mut [u8],
    hook: &mut impl PreparedPublicationHook,
    planned: FilePlan<'_>,
    staging_directory: &Path,
    output_directory: &Path,
    output_parent: &Path,
    manifest_path: &Path,
    staging_identity: (u64, u64),
    scratch: &mut crate::persistence_workspace::PublicationScratch,
) -> std::result::Result<(), DumpFailure> {
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
    let transaction_id = staging_directory
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(".tmp-lamb-"))
        .filter(|id| !id.is_empty())
        .ok_or((
            LambError::ExportInvariant("prepared dump transaction id is invalid"),
            None,
        ))?;
    let manifest_parent =
        File::open(output_parent).map_err(|source| (io_error(output_parent, source), None))?;
    let mut manifest_identity = None;
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

    for index in 0..planned.len() {
        let file = planned.get(index).expect("file plan length is stable");
        let staged_path = file.staged_path();
        let final_path = file.final_path();
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

    for entry in manifest.entries() {
        sync_published_file(manifest.path(entry.staged_path)).map_err(|error| (error, None))?;
    }
    hook.checkpoint(PublicationCheckpoint::DumpFilesSynced)
        .map_err(|error| (error, None))?;
    hook.sync_directory(staging_directory)
        .map_err(|error| (error, None))?;
    hook.checkpoint(PublicationCheckpoint::DumpDirectorySynced)
        .map_err(|error| (error, None))?;
    if let Err(error) = persist_manifest_prepared_scratch(
        &mut *serialization,
        manifest_path,
        &manifest,
        &manifest_parent,
        transaction_id,
        &mut manifest_identity,
        scratch,
        hook,
    ) {
        let durable = fs::symlink_metadata(manifest_path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
            .map(|_| ());
        return Err((error, durable));
    }
    hook.checkpoint(PublicationCheckpoint::DumpManifestPrepared)
        .map_err(|error| (error, dump_durable()))?;
    let staging_name = staging_directory.file_name().ok_or((
        LambError::ExportInvariant("prepared dump staging directory has no filename"),
        dump_durable(),
    ))?;
    let output_name = output_directory.file_name().ok_or((
        LambError::ExportInvariant("prepared dump output directory has no filename"),
        dump_durable(),
    ))?;
    let (staging_name, output_name) = {
        let (first, second) = scratch.component_pair();
        prepare_component_pair(first, second, staging_name, output_name)
            .map_err(|source| (io_error(output_directory, source), dump_durable()))?
    };
    rename_no_replace_at(
        &manifest_parent,
        staging_name,
        output_name,
        output_directory,
    )
    .map_err(|error| (error, dump_durable()))?;
    hook.checkpoint(PublicationCheckpoint::DumpAfterRename)
        .map_err(|error| (error, dump_durable()))?;
    hook.sync_directory(output_parent)
        .map_err(|error| (error, dump_durable()))?;
    hook.checkpoint(PublicationCheckpoint::DumpParentSynced)
        .map_err(|error| (error, dump_durable()))?;
    manifest.phase = ManifestPhase::Complete;
    persist_manifest_prepared_scratch(
        &mut *serialization,
        manifest_path,
        &manifest,
        &manifest_parent,
        transaction_id,
        &mut manifest_identity,
        scratch,
        hook,
    )
    .map_err(|error| (error, dump_durable()))?;
    hook.checkpoint(PublicationCheckpoint::DumpCompleteRecorded)
        .map_err(|error| (error, dump_durable()))?;
    let _ = verify_manifest_matches(&mut *serialization, manifest_path, &manifest)
        .map_err(|error| (error, dump_durable()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_manifest_prepared(
    serialization: &mut [u8],
    manifest_path: &Path,
    manifest: &TransactionManifest,
    manifest_parent: &File,
    transaction_id: &str,
    component_a: &mut [u8],
    component_b: &mut [u8],
    current_manifest_identity: &mut Option<ManifestIdentity>,
    hook: &mut impl PreparedPublicationHook,
) -> Result<()> {
    let manifest_name = manifest_path.file_name().ok_or(LambError::ExportInvariant(
        "prepared manifest has no filename",
    ))?;
    let manifest_name =
        prepare_component(component_a, manifest_name).map_err(|e| io_error(manifest_path, e))?;
    let name = manifest_name.to_bytes();
    let required = 1usize
        .checked_add(name.len())
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_add(transaction_id.len()))
        .and_then(|n| n.checked_add(4))
        .ok_or(LambError::ExportInvariant(
            "prepared manifest temporary name overflow",
        ))?;
    if required >= component_b.len() {
        return Err(LambError::ExportInvariant(
            "prepared manifest temporary name exceeds component scratch",
        ));
    }
    let mut cursor = 0;
    component_b[cursor] = b'.';
    cursor += 1;
    component_b[cursor..cursor + name.len()].copy_from_slice(name);
    cursor += name.len();
    component_b[cursor] = b'.';
    cursor += 1;
    component_b[cursor..cursor + transaction_id.len()].copy_from_slice(transaction_id.as_bytes());
    cursor += transaction_id.len();
    component_b[cursor..cursor + 4].copy_from_slice(b".tmp");
    cursor += 4;
    component_b[cursor] = 0;
    let temp_name = CStr::from_bytes_with_nul(&component_b[..=cursor]).map_err(|e| {
        io_error(
            manifest_path,
            io::Error::new(io::ErrorKind::InvalidInput, e),
        )
    })?;
    ManifestStore::new(serialization).write_prepared_at(
        manifest_parent,
        manifest_name,
        temp_name,
        manifest_path,
        manifest,
        current_manifest_identity,
        |directory, path| hook.sync_preopened_directory(directory, path),
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_manifest_prepared_scratch(
    serialization: &mut [u8],
    manifest_path: &Path,
    manifest: &TransactionManifest,
    manifest_parent: &File,
    transaction_id: &str,
    current_manifest_identity: &mut Option<ManifestIdentity>,
    scratch: &mut crate::persistence_workspace::PublicationScratch,
    hook: &mut impl PreparedPublicationHook,
) -> Result<()> {
    let (first, second) = scratch.component_pair();
    persist_manifest_prepared(
        serialization,
        manifest_path,
        manifest,
        manifest_parent,
        transaction_id,
        first,
        second,
        current_manifest_identity,
        hook,
    )
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
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn prepared_component_helpers_reject_nul_and_capacity_overflow() {
        let mut first = [0_u8; 4];
        let mut second = [0_u8; 4];

        assert_eq!(
            prepare_component(&mut first, OsStr::from_bytes(b"abc"))
                .unwrap()
                .to_bytes(),
            b"abc"
        );
        assert!(prepare_component(&mut first, OsStr::from_bytes(b"a\0b")).is_err());
        assert!(prepare_component(&mut first, OsStr::from_bytes(b"abcd")).is_err());
        assert!(prepare_component_pair(
            &mut first,
            &mut second,
            OsStr::from_bytes(b"ok"),
            OsStr::from_bytes(b"toolong"),
        )
        .is_err());
    }

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
