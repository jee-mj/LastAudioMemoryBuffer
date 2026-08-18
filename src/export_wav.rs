use crate::dump::{PublishedOutput, SampleSnapshot};
use crate::error::{io_error, LambError, Result};
use crate::math::wav_parts_for_channel;
use crate::sample_ring::Snapshot;
use std::collections::HashSet;
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
            let channel_name = channel_names
                .get(channel)
                .cloned()
                .unwrap_or_else(|| format!("ch{:02}", channel + 1));
            let file_name = if simple_names {
                if parts.len() > 1 {
                    format!("{}-part{:03}.wav", channel_name, part_index + 1)
                } else {
                    format!("{channel_name}.wav")
                }
            } else {
                format!(
                    "lamb-{}-{}-{}Hz-{:09}-{:09}-part{:03}.wav",
                    timestamp,
                    channel_name,
                    snapshot.sample_rate(),
                    part.start_frame,
                    part.start_frame + part.frame_count,
                    part_index + 1
                )
            };
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

fn validate_filename_component(value: &str, description: &str) -> Result<()> {
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

fn f32_to_s24_bytes(sample: f32) -> [u8; 3] {
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
