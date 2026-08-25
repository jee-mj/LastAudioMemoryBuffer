use crate::error::{io_error, LambError, Result};
use serde::de::DeserializeSeed;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

// Transactional publication and recovery rely on Linux `renameat2(RENAME_NOREPLACE
// /RENAME_EXCHANGE)` and `O_DIRECTORY|O_NOFOLLOW` directory synchronization. Fail
// at compile time on unsupported targets rather than at first publication.
#[cfg(not(target_os = "linux"))]
compile_error!("LAMB transactional persistence requires Linux renameat2 and directory fsync");

/// Version 2 records publication strategy. Version 1 is read-only legacy.
pub const MANIFEST_VERSION: u32 = 2;
pub const LEGACY_MANIFEST_VERSION: u32 = 1;
pub const RECALL_MANIFEST_NAME: &str = "manifest.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    /// Version-one FileSet spelling; current publishers never write it.
    Recall,
    /// Version-one AtomicDirectory spelling; current publishers never write it.
    Dump,
    FileSet,
    AtomicDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManifestPhase {
    Prepared,
    Publishing { index: usize },
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Captures the device/inode identity of an existing regular file or
/// directory, refusing symlinks.
pub fn capture_identity(path: &Path) -> Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(LambError::Validation(format!(
            "refusing to capture symlink identity: {}",
            path.display()
        )));
    }
    Ok(identity_from_metadata(&metadata))
}

/// Offset + length reference into the fixed manifest path arena.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathRef {
    pub offset: u32,
    pub len: u32,
}

/// Fixed-size manifest entry slot. All path data is referenced by `PathRef`
/// offsets into the preallocated path arena; no heap allocation is performed
/// per entry after capture starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManifestEntrySlot {
    pub staged_path: PathRef,
    pub staged_identity: Option<FileIdentity>,
    pub partial_path: PathRef,
    pub partial_identity: Option<FileIdentity>,
    pub final_path: PathRef,
    pub final_identity: Option<FileIdentity>,
}

/// A FileSet parent which this transaction intended to create.  `Intent` is
/// written before mkdir; it deliberately has no identity and is never removed
/// by recovery.  `Owned` is written only after mkdir and identity capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestDirectoryState {
    Intent,
    Owned,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManifestDirectorySlot {
    pub entry_index: u32,
    pub prefix_len: u32,
    pub state: Option<ManifestDirectoryState>,
    pub identity: Option<FileIdentity>,
}

/// Preallocated byte arena backing manifest path strings.
pub struct PathArena<'a> {
    bytes: &'a mut [u8],
    used: usize,
}

impl<'a> PathArena<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, used: 0 }
    }

    pub fn push(&mut self, value: &str) -> Result<PathRef> {
        let value_bytes = value.as_bytes();
        let len = u32::try_from(value_bytes.len())
            .map_err(|_| LambError::Validation("manifest path is too long".to_string()))?;
        let end = self
            .used
            .checked_add(value_bytes.len())
            .ok_or_else(|| LambError::Validation("manifest path arena overflow".to_string()))?;
        if end > self.bytes.len() {
            return Err(LambError::Validation(
                "manifest path arena capacity exceeded".to_string(),
            ));
        }
        self.bytes[self.used..end].copy_from_slice(value_bytes);
        let path_ref = PathRef {
            offset: u32::try_from(self.used)
                .map_err(|_| LambError::Validation("manifest path arena overflow".to_string()))?,
            len,
        };
        self.used = end;
        Ok(path_ref)
    }

    pub fn slice(&self, path_ref: PathRef) -> &str {
        let start = path_ref.offset as usize;
        let end = start + path_ref.len as usize;
        // Paths are always written as valid UTF-8 by construction and by the
        // recovery parser (which only accepts UTF-8 strings).
        std::str::from_utf8(&self.bytes[start..end]).expect("manifest path bytes are UTF-8")
    }

    pub fn path(&self, path_ref: PathRef) -> &Path {
        Path::new(self.slice(path_ref))
    }
}

/// Fixed-capacity arena-backed transaction manifest. `slots` and `path_arena`
/// are borrowed from the preallocated persistence workspace, so building,
/// publishing, and recovering a manifest never allocates memory proportional
/// to the number of entries or output parts.
pub struct TransactionManifest<'a> {
    pub version: u32,
    pub uid: u32,
    pub kind: TransactionKind,
    pub phase: ManifestPhase,
    pub transaction_id: PathRef,
    pub staging_root_path: PathRef,
    pub staging_root_identity: Option<FileIdentity>,
    pub output_root: PathRef,
    pub final_directory_path: PathRef,
    pub final_directory_identity: Option<FileIdentity>,
    pub entry_count: usize,
    slots: &'a mut [ManifestEntrySlot],
    directory_slots: &'a mut [ManifestDirectorySlot],
    pub directory_count: usize,
    directory_journal_present: bool,
    path_arena: PathArena<'a>,
}

impl<'a> TransactionManifest<'a> {
    pub fn new(slots: &'a mut [ManifestEntrySlot], path_bytes: &'a mut [u8]) -> Self {
        Self::new_with_directories(slots, &mut [], path_bytes)
    }

    pub fn new_with_directories(
        slots: &'a mut [ManifestEntrySlot],
        directory_slots: &'a mut [ManifestDirectorySlot],
        path_bytes: &'a mut [u8],
    ) -> Self {
        for slot in slots.iter_mut() {
            *slot = ManifestEntrySlot::default();
        }
        for slot in directory_slots.iter_mut() {
            *slot = ManifestDirectorySlot::default();
        }
        Self {
            version: 0,
            uid: 0,
            kind: TransactionKind::FileSet,
            phase: ManifestPhase::Prepared,
            transaction_id: PathRef::default(),
            staging_root_path: PathRef::default(),
            staging_root_identity: None,
            output_root: PathRef::default(),
            final_directory_path: PathRef::default(),
            final_directory_identity: None,
            entry_count: 0,
            slots,
            directory_slots,
            directory_count: 0,
            directory_journal_present: true,
            path_arena: PathArena::new(path_bytes),
        }
    }

    pub fn path_ref(&self, path_ref: PathRef) -> &str {
        self.path_arena.slice(path_ref)
    }

    pub fn path(&self, path_ref: PathRef) -> &Path {
        self.path_arena.path(path_ref)
    }

    pub fn push_path(&mut self, value: &str) -> Result<PathRef> {
        self.path_arena.push(value)
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn entries(&self) -> &[ManifestEntrySlot] {
        &self.slots[..self.entry_count]
    }

    pub fn entry(&self, index: usize) -> &ManifestEntrySlot {
        &self.slots[index]
    }

    pub fn entry_mut(&mut self, index: usize) -> &mut ManifestEntrySlot {
        &mut self.slots[index]
    }

    pub fn set_entry_count(&mut self, count: usize) -> Result<()> {
        if count > self.slots.len() {
            return Err(LambError::Validation(
                "manifest exceeds the reserved entry capacity".to_string(),
            ));
        }
        self.entry_count = count;
        Ok(())
    }

    pub fn directory_capacity(&self) -> usize {
        self.directory_slots.len()
    }

    pub fn directories(&self) -> &[ManifestDirectorySlot] {
        &self.directory_slots[..self.directory_count]
    }

    pub fn directory_mut(&mut self, index: usize) -> Result<&mut ManifestDirectorySlot> {
        self.directory_slots.get_mut(index).ok_or_else(|| {
            LambError::Validation("manifest exceeds the reserved directory capacity".to_string())
        })
    }

    pub fn push_directory_intent(
        &mut self,
        entry_index: usize,
        prefix_len: usize,
    ) -> Result<usize> {
        if self.directory_count >= self.directory_slots.len() {
            return Err(LambError::Validation(
                "manifest exceeds the reserved directory capacity".to_string(),
            ));
        }
        let index = self.directory_count;
        self.directory_slots[index] = ManifestDirectorySlot {
            entry_index: u32::try_from(entry_index)
                .map_err(|_| LambError::Validation("directory entry index overflow".to_string()))?,
            prefix_len: u32::try_from(prefix_len).map_err(|_| {
                LambError::Validation("directory prefix length overflow".to_string())
            })?,
            state: Some(ManifestDirectoryState::Intent),
            identity: None,
        };
        self.directory_count += 1;
        Ok(index)
    }

    pub fn directory_path(&self, slot: &ManifestDirectorySlot) -> Result<&Path> {
        let entry = self
            .entries()
            .get(slot.entry_index as usize)
            .ok_or_else(|| {
                LambError::Validation("directory entry index exceeds manifest entries".to_string())
            })?;
        let final_path = self.path_ref(entry.final_path);
        let prefix = final_path.get(..slot.prefix_len as usize).ok_or_else(|| {
            LambError::Validation("directory prefix is not a UTF-8 boundary".to_string())
        })?;
        Ok(Path::new(prefix))
    }

    pub fn final_directory(&self) -> Option<(&Path, Option<FileIdentity>)> {
        if self.final_directory_path.len == 0 {
            None
        } else {
            Some((
                self.path(self.final_directory_path),
                self.final_directory_identity,
            ))
        }
    }

    pub fn transaction_id_str(&self) -> &str {
        self.path_ref(self.transaction_id)
    }

    /// Resolves a manifest entry's staged path and identity.
    pub fn staged(&self, entry: &ManifestEntrySlot) -> (&Path, Option<FileIdentity>) {
        (self.path(entry.staged_path), entry.staged_identity)
    }

    /// Resolves a manifest entry's optional adjacent partial path and identity.
    pub fn partial(&self, entry: &ManifestEntrySlot) -> Option<(&Path, Option<FileIdentity>)> {
        if entry.partial_path.len == 0 {
            None
        } else {
            Some((self.path(entry.partial_path), entry.partial_identity))
        }
    }

    /// Resolves a manifest entry's final path and identity.
    pub fn final_of(&self, entry: &ManifestEntrySlot) -> (&Path, Option<FileIdentity>) {
        (self.path(entry.final_path), entry.final_identity)
    }
}

impl std::fmt::Debug for TransactionManifest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionManifest")
            .field("version", &self.version)
            .field("uid", &self.uid)
            .field("kind", &self.kind)
            .field("phase", &self.phase)
            .field("transaction_id", &self.transaction_id_str())
            .field("entry_count", &self.entry_count)
            .finish_non_exhaustive()
    }
}

// Serialization mirrors the historical on-disk JSON exactly so that already
// written manifests remain readable, while producing zero per-entry heap
// allocation (paths are emitted via `collect_str` and entries are streamed
// from the fixed slots).

struct ManifestPathSer<'a> {
    path: &'a Path,
    identity: Option<FileIdentity>,
}

impl Serialize for ManifestPathSer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ManifestPath", 2)?;
        state.serialize_field("path", &PathDisplay(self.path))?;
        state.serialize_field("identity", &self.identity)?;
        state.end()
    }
}

struct PathDisplay<'a>(&'a Path);

impl Serialize for PathDisplay<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0.display())
    }
}

impl Serialize for TransactionManifest<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let fields = if self.version == MANIFEST_VERSION {
            10
        } else {
            9
        };
        let mut state = serializer.serialize_struct("TransactionManifest", fields)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("uid", &self.uid)?;
        state.serialize_field("transaction_id", self.transaction_id_str())?;
        state.serialize_field("kind", &self.kind)?;
        state.serialize_field("phase", &self.phase)?;
        state.serialize_field(
            "staging_root",
            &ManifestPathSer {
                path: self.path(self.staging_root_path),
                identity: self.staging_root_identity,
            },
        )?;
        state.serialize_field("output_root", &PathDisplay(self.path(self.output_root)))?;
        state.serialize_field("final_directory", &ManifestDirectorySer { manifest: self })?;
        state.serialize_field("entries", &ManifestEntriesSer { manifest: self })?;
        if self.version == MANIFEST_VERSION {
            state.serialize_field(
                "created_directories",
                &ManifestDirectoriesSer { manifest: self },
            )?;
        }
        state.end()
    }
}

struct ManifestDirectoriesSer<'a> {
    manifest: &'a TransactionManifest<'a>,
}

impl Serialize for ManifestDirectoriesSer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.manifest.directory_count))?;
        for slot in self.manifest.directories() {
            seq.serialize_element(&ManifestCreatedDirectorySer { slot })?;
        }
        seq.end()
    }
}

struct ManifestCreatedDirectorySer<'a> {
    slot: &'a ManifestDirectorySlot,
}

impl Serialize for ManifestCreatedDirectorySer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ManifestCreatedDirectory", 4)?;
        state.serialize_field("entry_index", &self.slot.entry_index)?;
        state.serialize_field("prefix_len", &self.slot.prefix_len)?;
        state.serialize_field("state", &self.slot.state)?;
        state.serialize_field("identity", &self.slot.identity)?;
        state.end()
    }
}

struct ManifestDirectorySer<'a> {
    manifest: &'a TransactionManifest<'a>,
}

impl Serialize for ManifestDirectorySer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match self.manifest.final_directory() {
            Some((path, identity)) => {
                serializer.serialize_some(&ManifestPathSer { path, identity })
            }
            None => serializer.serialize_none(),
        }
    }
}

struct ManifestEntriesSer<'a> {
    manifest: &'a TransactionManifest<'a>,
}

impl Serialize for ManifestEntriesSer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.manifest.entry_count))?;
        for slot in self.manifest.entries() {
            seq.serialize_element(&ManifestEntrySer {
                manifest: self.manifest,
                slot,
            })?;
        }
        seq.end()
    }
}

struct ManifestEntrySer<'a> {
    manifest: &'a TransactionManifest<'a>,
    slot: &'a ManifestEntrySlot,
}

impl Serialize for ManifestEntrySer<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let manifest = self.manifest;
        let slot = self.slot;
        let partial = if slot.partial_path.len == 0 {
            None
        } else {
            Some(ManifestPathSer {
                path: manifest.path(slot.partial_path),
                identity: slot.partial_identity,
            })
        };
        let mut state = serializer.serialize_struct("ManifestEntry", 3)?;
        state.serialize_field(
            "staged",
            &ManifestPathSer {
                path: manifest.path(slot.staged_path),
                identity: slot.staged_identity,
            },
        )?;
        state.serialize_field("partial", &partial)?;
        state.serialize_field(
            "final_path",
            &ManifestPathSer {
                path: manifest.path(slot.final_path),
                identity: slot.final_identity,
            },
        )?;
        state.end()
    }
}

// Recovery parsing. `ManifestParseSeed` fills the preallocated slots and path
// arena in place, streaming entries without a heap `Vec` or `PathBuf`.
pub struct ManifestParseSeed<'a> {
    slots: &'a mut [ManifestEntrySlot],
    directory_slots: &'a mut [ManifestDirectorySlot],
    path_arena: PathArena<'a>,
}

impl<'a> ManifestParseSeed<'a> {
    pub fn new(slots: &'a mut [ManifestEntrySlot], path_bytes: &'a mut [u8]) -> Self {
        Self::new_with_directories(slots, &mut [], path_bytes)
    }

    pub fn new_with_directories(
        slots: &'a mut [ManifestEntrySlot],
        directory_slots: &'a mut [ManifestDirectorySlot],
        path_bytes: &'a mut [u8],
    ) -> Self {
        for slot in slots.iter_mut() {
            *slot = ManifestEntrySlot::default();
        }
        for slot in directory_slots.iter_mut() {
            *slot = ManifestDirectorySlot::default();
        }
        Self {
            slots,
            directory_slots,
            path_arena: PathArena::new(path_bytes),
        }
    }
}

impl<'de, 'a> serde::de::DeserializeSeed<'de> for ManifestParseSeed<'a> {
    type Value = TransactionManifest<'a>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<'a> {
            slots: &'a mut [ManifestEntrySlot],
            directory_slots: &'a mut [ManifestDirectorySlot],
            path_arena: PathArena<'a>,
        }

        impl<'de, 'a> serde::de::Visitor<'de> for Visitor<'a> {
            type Value = TransactionManifest<'a>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a transaction manifest")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut manifest = TransactionManifest {
                    version: 0,
                    uid: 0,
                    kind: TransactionKind::Recall,
                    phase: ManifestPhase::Prepared,
                    transaction_id: PathRef::default(),
                    staging_root_path: PathRef::default(),
                    staging_root_identity: None,
                    output_root: PathRef::default(),
                    final_directory_path: PathRef::default(),
                    final_directory_identity: None,
                    entry_count: 0,
                    slots: self.slots,
                    directory_slots: self.directory_slots,
                    directory_count: 0,
                    directory_journal_present: false,
                    path_arena: self.path_arena,
                };

                while let Some(key) = map.next_key::<&str>().map_err(serde::de::Error::custom)? {
                    match key {
                        "version" => {
                            manifest.version =
                                map.next_value().map_err(serde::de::Error::custom)?;
                        }
                        "uid" => {
                            manifest.uid = map.next_value().map_err(serde::de::Error::custom)?;
                        }
                        "transaction_id" => {
                            manifest.transaction_id = map.next_value_seed(StrSeed {
                                manifest: &mut manifest,
                            })?;
                        }
                        "kind" => {
                            manifest.kind = map.next_value().map_err(serde::de::Error::custom)?;
                        }
                        "phase" => {
                            manifest.phase = map.next_value().map_err(serde::de::Error::custom)?;
                        }
                        "staging_root" => {
                            let (path, identity) = map.next_value_seed(ManifestPathSeed {
                                manifest: &mut manifest,
                            })?;
                            manifest.staging_root_path = path;
                            manifest.staging_root_identity = identity;
                        }
                        "output_root" => {
                            manifest.output_root = map.next_value_seed(StrSeed {
                                manifest: &mut manifest,
                            })?;
                        }
                        "final_directory" => {
                            let value = map.next_value_seed(OptionManifestPathSeed {
                                manifest: &mut manifest,
                            })?;
                            match value {
                                Some((path, identity)) => {
                                    manifest.final_directory_path = path;
                                    manifest.final_directory_identity = identity;
                                }
                                None => {
                                    manifest.final_directory_path = PathRef::default();
                                    manifest.final_directory_identity = None;
                                }
                            }
                        }
                        "entries" => {
                            manifest.entry_count = map.next_value_seed(EntriesSeed {
                                manifest: &mut manifest,
                            })?;
                        }
                        "created_directories" => {
                            if manifest.directory_journal_present {
                                return Err(serde::de::Error::custom(
                                    "duplicate created_directories field",
                                ));
                            }
                            manifest.directory_journal_present = true;
                            manifest.directory_count = map.next_value_seed(DirectoriesSeed {
                                manifest: &mut manifest,
                            })?;
                        }
                        _ => {
                            return Err(serde::de::Error::custom(format!(
                                "unknown manifest field {key}"
                            )))
                        }
                    }
                }
                Ok(manifest)
            }
        }

        deserializer.deserialize_map(Visitor {
            slots: self.slots,
            directory_slots: self.directory_slots,
            path_arena: self.path_arena,
        })
    }
}

struct StrSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for StrSeed<'b, 'a> {
    type Value = PathRef;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<PathRef, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a>(&'b mut TransactionManifest<'a>);
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = PathRef;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a UTF-8 path string")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<PathRef, E> {
                self.0.push_path(value).map_err(E::custom)
            }
            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                value: &'de str,
            ) -> std::result::Result<PathRef, E> {
                self.0.push_path(value).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(V(self.manifest))
    }
}

struct ManifestPathSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for ManifestPathSeed<'b, 'a> {
    type Value = (PathRef, Option<FileIdentity>);

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a>(&'b mut TransactionManifest<'a>);
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = (PathRef, Option<FileIdentity>);
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a manifest path with identity")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut path = PathRef::default();
                let mut identity: Option<FileIdentity> = None;
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "path" => {
                            path = map.next_value_seed(StrSeed {
                                manifest: &mut *self.0,
                            })?;
                        }
                        "identity" => {
                            identity = map.next_value().map_err(serde::de::Error::custom)?;
                        }
                        _ => return Err(serde::de::Error::custom(format!("unknown field {key}"))),
                    }
                }
                Ok((path, identity))
            }
        }
        deserializer.deserialize_map(V(self.manifest))
    }
}

struct OptionManifestPathSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for OptionManifestPathSeed<'b, 'a> {
    type Value = Option<(PathRef, Option<FileIdentity>)>;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a>(&'b mut TransactionManifest<'a>);
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = Option<(PathRef, Option<FileIdentity>)>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an optional manifest path")
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> std::result::Result<Self::Value, D::Error> {
                let (path, identity) =
                    ManifestPathSeed { manifest: self.0 }.deserialize(deserializer)?;
                Ok(Some((path, identity)))
            }
        }
        deserializer.deserialize_option(V(self.manifest))
    }
}

struct EntriesSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for EntriesSeed<'b, 'a> {
    type Value = usize;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<usize, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a>(&'b mut TransactionManifest<'a>);
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = usize;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of manifest entries")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<usize, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut count = 0usize;
                while let Some(()) = seq.next_element_seed(EntrySlotSeed {
                    manifest: &mut *self.0,
                    index: count,
                })? {
                    count += 1;
                }
                Ok(count)
            }
        }
        deserializer.deserialize_seq(V(self.manifest))
    }
}

struct EntrySlotSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
    index: usize,
}

struct DirectoriesSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for DirectoriesSeed<'b, 'a> {
    type Value = usize;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<usize, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a>(&'b mut TransactionManifest<'a>);
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = usize;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of created directory records")
            }
            fn visit_seq<A>(self, mut seq: A) -> std::result::Result<usize, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut count = 0;
                while let Some(()) = seq.next_element_seed(DirectorySlotSeed {
                    manifest: &mut *self.0,
                    index: count,
                })? {
                    count += 1;
                }
                Ok(count)
            }
        }
        deserializer.deserialize_seq(V(self.manifest))
    }
}

struct DirectorySlotSeed<'b, 'a> {
    manifest: &'b mut TransactionManifest<'a>,
    index: usize,
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for DirectorySlotSeed<'b, 'a> {
    type Value = ();
    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a> {
            manifest: &'b mut TransactionManifest<'a>,
            index: usize,
        }
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a created directory record")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut entry_index = None;
                let mut prefix_len = None;
                let mut state = None;
                let mut identity = None;
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "entry_index" if entry_index.is_none() => {
                            entry_index = Some(map.next_value()?)
                        }
                        "prefix_len" if prefix_len.is_none() => {
                            prefix_len = Some(map.next_value()?)
                        }
                        "state" if state.is_none() => state = Some(map.next_value()?),
                        "identity" if identity.is_none() => identity = Some(map.next_value()?),
                        "entry_index" | "prefix_len" | "state" | "identity" => {
                            return Err(serde::de::Error::custom(
                                "duplicate created directory field",
                            ))
                        }
                        _ => return Err(serde::de::Error::custom(format!("unknown field {key}"))),
                    }
                }
                let slot = self
                    .manifest
                    .directory_slots
                    .get_mut(self.index)
                    .ok_or_else(|| {
                        serde::de::Error::custom("manifest exceeds directory capacity")
                    })?;
                slot.entry_index = entry_index.ok_or_else(|| {
                    serde::de::Error::custom("created directory entry index missing")
                })?;
                slot.prefix_len = prefix_len.ok_or_else(|| {
                    serde::de::Error::custom("created directory prefix length missing")
                })?;
                slot.state =
                    Some(state.ok_or_else(|| {
                        serde::de::Error::custom("created directory state missing")
                    })?);
                slot.identity = identity.ok_or_else(|| {
                    serde::de::Error::custom("created directory identity missing")
                })?;
                Ok(())
            }
        }
        deserializer.deserialize_map(V {
            manifest: self.manifest,
            index: self.index,
        })
    }
}

impl<'de, 'b, 'a> serde::de::DeserializeSeed<'de> for EntrySlotSeed<'b, 'a> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V<'b, 'a> {
            manifest: &'b mut TransactionManifest<'a>,
            index: usize,
        }
        impl<'de, 'b, 'a> serde::de::Visitor<'de> for V<'b, 'a> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a manifest entry")
            }
            fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let V { manifest, index } = self;
                let mut staged = (PathRef::default(), None);
                let mut partial: Option<(PathRef, Option<FileIdentity>)> = None;
                let mut final_path = (PathRef::default(), None);
                while let Some(key) = map.next_key::<&str>()? {
                    match key {
                        "staged" => {
                            staged = map.next_value_seed(ManifestPathSeed {
                                manifest: &mut *manifest,
                            })?;
                        }
                        "partial" => {
                            partial = map.next_value_seed(OptionManifestPathSeed {
                                manifest: &mut *manifest,
                            })?;
                        }
                        "final_path" => {
                            final_path = map.next_value_seed(ManifestPathSeed {
                                manifest: &mut *manifest,
                            })?;
                        }
                        _ => return Err(serde::de::Error::custom(format!("unknown field {key}"))),
                    }
                }
                let slot = manifest
                    .slots
                    .get_mut(index)
                    .ok_or_else(|| serde::de::Error::custom("manifest exceeds entry capacity"))?;
                slot.staged_path = staged.0;
                slot.staged_identity = staged.1;
                match partial {
                    Some((path, identity)) => {
                        slot.partial_path = path;
                        slot.partial_identity = identity;
                    }
                    None => {
                        slot.partial_path = PathRef::default();
                        slot.partial_identity = None;
                    }
                }
                slot.final_path = final_path.0;
                slot.final_identity = final_path.1;
                Ok(())
            }
        }
        deserializer.deserialize_map(V {
            manifest: self.manifest,
            index: self.index,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Complete,
    RolledBack,
    Pending,
}

pub struct ManifestStore<'a> {
    buffer: &'a mut [u8],
}

impl<'a> ManifestStore<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self { buffer }
    }

    pub fn write(&mut self, manifest_path: &Path, manifest: &TransactionManifest) -> Result<()> {
        self.write_with_directory_sync(manifest_path, manifest, sync_directory)
    }

    pub fn write_with_directory_sync(
        &mut self,
        manifest_path: &Path,
        manifest: &TransactionManifest,
        directory_sync: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        validate_manifest_basics(manifest)?;
        let serialized_len = serialize_bounded(self.buffer, manifest)?;
        let parent = manifest_path.parent().ok_or_else(|| {
            LambError::Validation("manifest path has no parent directory".to_string())
        })?;
        reject_symlink_path(parent)?;

        let existing = match read_manifest_identity(manifest_path) {
            Ok(identity) => {
                // Verify the existing manifest belongs to this transaction before
                // replacing it. A partial borrow-parse (no heap growth) extracts
                // only the identity fields into the fixed buffer.
                let previous = self.read_identity_fields(manifest_path)?;
                if previous.version != manifest.version
                    || previous.uid != manifest.uid
                    || previous.transaction_id != manifest.transaction_id_str()
                    || previous.kind != manifest.kind
                    || previous.staging_root() != manifest.path(manifest.staging_root_path)
                    || previous.output_root() != manifest.path(manifest.output_root)
                {
                    return Err(LambError::Validation(
                        "refusing to replace a foreign manifest".to_string(),
                    ));
                }
                Some(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(manifest_path, error)),
        };

        // read_identity_fields() reused the fixed buffer, so serialize the new
        // state again before writing.
        let serialized_len = if existing.is_some() {
            serialize_bounded(self.buffer, manifest)?
        } else {
            serialized_len
        };
        let temp_path = manifest_temp_path(manifest_path, manifest.transaction_id_str())?;
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&temp_path)
            .map_err(|source| io_error(&temp_path, source))?;
        let temp_identity = identity_from_metadata(
            &temp
                .metadata()
                .map_err(|source| io_error(&temp_path, source))?,
        );
        let write_result = (|| {
            temp.write_all(&self.buffer[..serialized_len])
                .map_err(|source| io_error(&temp_path, source))?;
            temp.flush()
                .map_err(|source| io_error(&temp_path, source))?;
            temp.sync_all()
                .map_err(|source| io_error(&temp_path, source))?;
            drop(temp);
            match existing {
                None => rename_no_replace(&temp_path, manifest_path)?,
                Some(expected) => {
                    rename_exchange(&temp_path, manifest_path)?;
                    let replaced = read_manifest_identity(&temp_path)
                        .map_err(|source| io_error(&temp_path, source))?;
                    if replaced != expected {
                        let restore = rename_exchange(&temp_path, manifest_path);
                        return match restore {
                            Ok(()) => Err(LambError::Validation(
                                "manifest identity changed during atomic replacement".to_string(),
                            )),
                            Err(error) => Err(LambError::IndeterminatePublication {
                                operation: Box::new(error),
                            }),
                        };
                    }
                    remove_if_identity(&temp_path, expected)?;
                }
            }
            directory_sync(parent)
        })();
        if write_result.is_err() {
            let _ = remove_if_identity(&temp_path, temp_identity);
        }
        write_result
    }

    fn read_identity_fields(&mut self, manifest_path: &Path) -> Result<ExistingManifestCheck<'_>> {
        let used = self.read_file_bounded(manifest_path)?;
        serde_json::from_slice(&self.buffer[..used]).map_err(|error| {
            LambError::Validation(format!("malformed transaction manifest: {error}"))
        })
    }

    fn read_file_bounded(&mut self, manifest_path: &Path) -> Result<usize> {
        if self.buffer.is_empty() {
            return Err(LambError::Validation(
                "manifest buffer must not be empty".to_string(),
            ));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(manifest_path)
            .map_err(|source| io_error(manifest_path, source))?;
        let metadata = file
            .metadata()
            .map_err(|source| io_error(manifest_path, source))?;
        if !metadata.file_type().is_file() {
            return Err(LambError::Validation(
                "manifest must be a regular file".to_string(),
            ));
        }
        let mut used = 0;
        while used < self.buffer.len() {
            match file.read(&mut self.buffer[used..]) {
                Ok(0) => break,
                Ok(count) => used += count,
                Err(source) => return Err(io_error(manifest_path, source)),
            }
        }
        if used == self.buffer.len() {
            let mut extra = [0_u8; 1];
            if file
                .read(&mut extra)
                .map_err(|source| io_error(manifest_path, source))?
                != 0
            {
                return Err(LambError::Validation(
                    "manifest exceeds the fixed recovery buffer".to_string(),
                ));
            }
        }
        Ok(used)
    }

    pub fn read<'b>(
        &mut self,
        manifest_path: &Path,
        slots: &'b mut [ManifestEntrySlot],
        path_bytes: &'b mut [u8],
    ) -> Result<TransactionManifest<'b>> {
        let (manifest, _) = self.read_with_identity(manifest_path, slots, path_bytes)?;
        Ok(manifest)
    }

    pub fn read_with_identity<'b>(
        &mut self,
        manifest_path: &Path,
        slots: &'b mut [ManifestEntrySlot],
        path_bytes: &'b mut [u8],
    ) -> Result<(TransactionManifest<'b>, FileIdentity)> {
        self.read_with_identity_directories(manifest_path, slots, &mut [], path_bytes)
    }

    pub fn read_with_identity_directories<'b>(
        &mut self,
        manifest_path: &Path,
        slots: &'b mut [ManifestEntrySlot],
        directory_slots: &'b mut [ManifestDirectorySlot],
        path_bytes: &'b mut [u8],
    ) -> Result<(TransactionManifest<'b>, FileIdentity)> {
        let used = self.read_file_bounded(manifest_path)?;
        let identity = read_manifest_identity(manifest_path).map_err(|source| {
            LambError::Validation(format!("failed to read manifest identity: {source}"))
        })?;
        let mut deserializer = serde_json::Deserializer::from_slice(&self.buffer[..used]);
        let manifest = ManifestParseSeed::new_with_directories(slots, directory_slots, path_bytes)
            .deserialize(&mut deserializer)
            .map_err(|error| {
                LambError::Validation(format!("malformed transaction manifest: {error}"))
            })?;
        Ok((manifest, identity))
    }

    pub fn verify_matches(
        &mut self,
        manifest_path: &Path,
        expected: &TransactionManifest,
    ) -> Result<()> {
        let previous = self.read_identity_fields(manifest_path)?;
        if previous.version != expected.version
            || previous.uid != expected.uid
            || previous.transaction_id != expected.transaction_id_str()
            || previous.kind != expected.kind
            || previous.staging_root() != expected.path(expected.staging_root_path)
            || previous.output_root() != expected.path(expected.output_root)
        {
            return Err(LambError::Validation(
                "transaction manifest changed before publication outcome".to_string(),
            ));
        }
        Ok(())
    }
}

/// Verifies the on-disk manifest still matches the given in-memory manifest's
/// identity fields, returning the manifest file identity on success.
pub fn verify_manifest_matches(
    serialization: &mut [u8],
    manifest_path: &Path,
    expected: &TransactionManifest,
) -> Result<FileIdentity> {
    let identity =
        read_manifest_identity(manifest_path).map_err(|source| io_error(manifest_path, source))?;
    ManifestStore::new(serialization).verify_matches(manifest_path, expected)?;
    Ok(identity)
}

/// Borrowed partial view of a manifest's identity fields, used only to verify a
/// pre-existing manifest belongs to the current transaction before replacement.
#[derive(serde::Deserialize)]
struct ExistingManifestCheck<'a> {
    version: u32,
    uid: u32,
    #[serde(borrow)]
    transaction_id: &'a str,
    kind: TransactionKind,
    #[serde(borrow)]
    staging_root: ExistingCheckPath<'a>,
    #[serde(borrow)]
    output_root: &'a str,
}

#[derive(serde::Deserialize)]
struct ExistingCheckPath<'a> {
    #[serde(borrow)]
    path: &'a str,
    #[serde(default)]
    #[allow(dead_code)]
    identity: Option<FileIdentity>,
}

impl ExistingManifestCheck<'_> {
    fn staging_root(&self) -> &Path {
        Path::new(self.staging_root.path)
    }
    fn output_root(&self) -> &Path {
        Path::new(self.output_root)
    }
}

pub fn recover_recall_root(
    transaction_root: &Path,
    output_root: &Path,
    buffer: &mut [u8],
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
) -> Result<RecoveryOutcome> {
    recover_recall_root_with_directories(
        transaction_root,
        output_root,
        buffer,
        slots,
        &mut [],
        path_bytes,
    )
}

pub fn recover_recall_root_with_directories(
    transaction_root: &Path,
    output_root: &Path,
    buffer: &mut [u8],
    slots: &mut [ManifestEntrySlot],
    directory_slots: &mut [ManifestDirectorySlot],
    path_bytes: &mut [u8],
) -> Result<RecoveryOutcome> {
    let manifest_path = transaction_root.join(RECALL_MANIFEST_NAME);
    let (manifest, manifest_identity) = ManifestStore::new(buffer).read_with_identity_directories(
        &manifest_path,
        slots,
        directory_slots,
        path_bytes,
    )?;
    validate_manifest(
        &manifest,
        TransactionKind::FileSet,
        transaction_root,
        output_root,
        &manifest_path,
    )?;

    if complete_final_set(&manifest)? {
        for (index, entry) in manifest.entries().iter().enumerate() {
            let identity = inferred_final_identity(&manifest, index)?.ok_or_else(|| {
                LambError::Validation("complete recall identity disappeared".to_string())
            })?;
            let (final_path, _) = manifest.final_of(entry);
            sync_regular_file(final_path, identity)?;
        }
        sync_file_set_directories(&manifest, output_root)?;
        best_effort_remove_recall_metadata(&manifest, &manifest_path, manifest_identity);
        if let Some(parent) = manifest.path(manifest.staging_root_path).parent() {
            let _ = sync_directory(parent);
        }
        return Ok(RecoveryOutcome::Complete);
    }

    let mut pending = false;
    for (index, entry) in manifest.entries().iter().enumerate() {
        let (final_path, _) = manifest.final_of(entry);
        let final_identity = inferred_final_identity(&manifest, index)?;
        pending |= !cleanup_file(final_path, final_identity)?;
        if let Some((partial_path, partial_identity)) = manifest.partial(entry) {
            pending |= !cleanup_file(partial_path, partial_identity)?;
        }
        let (staged_path, staged_identity) = manifest.staged(entry);
        pending |= !cleanup_file(staged_path, staged_identity)?;
    }
    if pending {
        return Ok(RecoveryOutcome::Pending);
    }
    // Directory cleanup is best-effort: an owned directory made nonempty by a
    // foreign sibling must not strand an otherwise resolved transaction.
    for directory in manifest.directories().iter().rev() {
        if directory.state == Some(ManifestDirectoryState::Owned) {
            let _ = remove_directory_if_identity(
                manifest.directory_path(directory)?,
                directory.identity,
            )?;
        }
    }
    let staging_root = manifest.path(manifest.staging_root_path);
    if !directory_contains_only(staging_root, &manifest_path)? {
        return Ok(RecoveryOutcome::Pending);
    }
    remove_if_identity(&manifest_path, manifest_identity)?;
    if !remove_directory_if_identity(staging_root, manifest.staging_root_identity)? {
        return Ok(RecoveryOutcome::Pending);
    }
    Ok(RecoveryOutcome::RolledBack)
}

pub fn recover_dump_parent(
    output_parent: &Path,
    manifest_path: &Path,
    buffer: &mut [u8],
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
) -> Result<RecoveryOutcome> {
    let (manifest, manifest_identity) =
        ManifestStore::new(buffer).read_with_identity(manifest_path, slots, path_bytes)?;
    validate_manifest(
        &manifest,
        TransactionKind::AtomicDirectory,
        manifest.path(manifest.staging_root_path),
        output_parent,
        manifest_path,
    )?;

    if complete_final_set(&manifest)? {
        for entry in manifest.entries() {
            let (final_path, final_identity) = manifest.final_of(entry);
            let identity = final_identity.ok_or_else(|| {
                LambError::Validation("complete dump identity disappeared".to_string())
            })?;
            sync_regular_file(final_path, identity)?;
        }
        let (final_directory, final_directory_identity) =
            manifest.final_directory().ok_or_else(|| {
                LambError::Validation("dump manifest has no final directory".to_string())
            })?;
        let directory_identity = final_directory_identity.ok_or_else(|| {
            LambError::Validation("complete dump directory identity disappeared".to_string())
        })?;
        sync_owned_directory(final_directory, directory_identity)?;
        sync_directory(output_parent)?;
        remove_if_identity(manifest_path, manifest_identity)?;
        if !cleanup_dump_temp_manifest(manifest_path, manifest.transaction_id_str())? {
            return Ok(RecoveryOutcome::Pending);
        }
        sync_directory(output_parent)?;
        return Ok(RecoveryOutcome::Complete);
    }

    let (final_directory, final_directory_identity) = manifest
        .final_directory()
        .ok_or_else(|| LambError::Validation("dump manifest has no final directory".to_string()))?;
    if path_exists(final_directory)? {
        let Some(identity) = final_directory_identity else {
            return Ok(RecoveryOutcome::Pending);
        };
        if identity_matches(final_directory, identity, true)? {
            return Ok(RecoveryOutcome::Pending);
        }
    }
    for entry in manifest.entries() {
        let (staged_path, staged_identity) = manifest.staged(entry);
        if !cleanup_file(staged_path, staged_identity)? {
            return Ok(RecoveryOutcome::Pending);
        }
    }
    let staging_root = manifest.path(manifest.staging_root_path);
    if !remove_directory_if_identity(staging_root, manifest.staging_root_identity)? {
        return Ok(RecoveryOutcome::Pending);
    }
    remove_if_identity(manifest_path, manifest_identity)?;
    if !cleanup_dump_temp_manifest(manifest_path, manifest.transaction_id_str())? {
        return Ok(RecoveryOutcome::Pending);
    }
    Ok(RecoveryOutcome::RolledBack)
}

pub fn sync_directory(path: &Path) -> Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    directory
        .sync_all()
        .map_err(|source| io_error(path, source))
}

/// A single recovery problem surfaced to the operator: a transaction that
/// failed or could not be resolved (pending), with its identity and cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryIssue {
    pub path: PathBuf,
    pub error: String,
}

/// Best-effort summary of a startup recovery scan over marked transactions.
#[derive(Debug, Default)]
pub struct RecoveryScanSummary {
    pub discovered: usize,
    pub completed: usize,
    pub rolled_back: usize,
    pub pending: usize,
    pub failed: usize,
    pub issues: Vec<RecoveryIssue>,
}

impl RecoveryScanSummary {
    pub fn merge(&mut self, other: RecoveryScanSummary) {
        self.discovered += other.discovered;
        self.completed += other.completed;
        self.rolled_back += other.rolled_back;
        self.pending += other.pending;
        self.failed += other.failed;
        self.issues.extend(other.issues);
    }
}

/// Recovers every marked recall transaction under `staging_root` (each
/// immediate child directory containing a recall manifest). Recovery is
/// best-effort per transaction; unmarked legacy artifacts are never touched.
/// The caller provides the fixed parse arenas, reused across transactions.
pub fn recover_recall_staging_root(
    staging_root: &Path,
    output_root: &Path,
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
    buffer: &mut [u8],
) -> RecoveryScanSummary {
    recover_recall_staging_root_with_directories(
        staging_root,
        output_root,
        slots,
        &mut [],
        path_bytes,
        buffer,
    )
}

pub fn recover_recall_staging_root_with_directories(
    staging_root: &Path,
    output_root: &Path,
    slots: &mut [ManifestEntrySlot],
    directory_slots: &mut [ManifestDirectorySlot],
    path_bytes: &mut [u8],
    buffer: &mut [u8],
) -> RecoveryScanSummary {
    let mut summary = RecoveryScanSummary::default();
    let Ok(entries) = fs::read_dir(staging_root) else {
        return summary;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join(RECALL_MANIFEST_NAME);
        if !manifest_path.is_file() {
            continue;
        }
        summary.discovered += 1;
        match recover_recall_root_with_directories(
            &path,
            output_root,
            buffer,
            slots,
            directory_slots,
            path_bytes,
        ) {
            Ok(RecoveryOutcome::Complete) => summary.completed += 1,
            Ok(RecoveryOutcome::RolledBack) => summary.rolled_back += 1,
            Ok(RecoveryOutcome::Pending) => {
                summary.pending += 1;
                summary.issues.push(RecoveryIssue {
                    path: path.clone(),
                    error: "recovery incomplete; transaction remains pending".to_string(),
                });
            }
            Err(error) => {
                summary.failed += 1;
                summary.issues.push(RecoveryIssue {
                    path: path.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
    summary
}

/// Recovers every marked dump transaction under `dump_parent` (each sibling
/// `.<id>.manifest.json`). Best-effort per transaction; foreign/unmarked files
/// are untouched.
pub fn recover_dump_root(
    dump_parent: &Path,
    slots: &mut [ManifestEntrySlot],
    path_bytes: &mut [u8],
    buffer: &mut [u8],
) -> RecoveryScanSummary {
    let mut summary = RecoveryScanSummary::default();
    let Ok(entries) = fs::read_dir(dump_parent) else {
        return summary;
    };
    for entry in entries.flatten() {
        let manifest_path = entry.path();
        let Some(name) = manifest_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with('.') || !name.ends_with(".manifest.json") {
            continue;
        }
        summary.discovered += 1;
        match recover_dump_parent(dump_parent, &manifest_path, buffer, slots, path_bytes) {
            Ok(RecoveryOutcome::Complete) => summary.completed += 1,
            Ok(RecoveryOutcome::RolledBack) => summary.rolled_back += 1,
            Ok(RecoveryOutcome::Pending) => {
                summary.pending += 1;
                summary.issues.push(RecoveryIssue {
                    path: manifest_path.clone(),
                    error: "recovery incomplete; transaction remains pending".to_string(),
                });
            }
            Err(error) => {
                summary.failed += 1;
                summary.issues.push(RecoveryIssue {
                    path: manifest_path.clone(),
                    error: error.to_string(),
                });
            }
        }
    }
    summary
}

fn sync_owned_directory(path: &Path, expected: FileIdentity) -> Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let metadata = directory
        .metadata()
        .map_err(|source| io_error(path, source))?;
    if identity_from_metadata(&metadata) != expected {
        return Err(LambError::Validation(
            "published directory identity changed before synchronization".to_string(),
        ));
    }
    directory
        .sync_all()
        .map_err(|source| io_error(path, source))?;
    if !identity_matches(path, expected, true)? {
        return Err(LambError::Validation(
            "published directory identity changed during synchronization".to_string(),
        ));
    }
    Ok(())
}

fn validate_manifest_basics(manifest: &TransactionManifest) -> Result<()> {
    if manifest.version != MANIFEST_VERSION && manifest.version != LEGACY_MANIFEST_VERSION {
        return Err(LambError::Validation(format!(
            "unsupported transaction manifest version {}",
            manifest.version
        )));
    }
    if !matches!(
        (manifest.version, manifest.kind),
        (
            LEGACY_MANIFEST_VERSION,
            TransactionKind::Recall | TransactionKind::Dump
        ) | (
            MANIFEST_VERSION,
            TransactionKind::FileSet | TransactionKind::AtomicDirectory
        )
    ) {
        return Err(LambError::Validation(
            "transaction manifest version and strategy do not match".to_string(),
        ));
    }
    if manifest.uid != unsafe { libc::geteuid() } {
        return Err(LambError::Validation(
            "transaction manifest belongs to a different uid".to_string(),
        ));
    }
    validate_component(manifest.transaction_id_str(), "transaction id")?;
    if manifest.entry_count == 0 {
        return Err(LambError::Validation(
            "transaction manifest has no file entries".to_string(),
        ));
    }
    if manifest.version == MANIFEST_VERSION && !manifest.directory_journal_present {
        return Err(LambError::Validation(
            "version-two manifest omits created directory journal".to_string(),
        ));
    }
    if manifest.version == LEGACY_MANIFEST_VERSION && manifest.directory_count != 0 {
        return Err(LambError::Validation(
            "version-one manifest may not contain created directory journal".to_string(),
        ));
    }
    Ok(())
}

fn validate_manifest(
    manifest: &TransactionManifest,
    expected_kind: TransactionKind,
    expected_staging_root: &Path,
    expected_output_root: &Path,
    manifest_path: &Path,
) -> Result<()> {
    validate_manifest_basics(manifest)?;
    if !matches!(
        (expected_kind, manifest.version, manifest.kind),
        (
            TransactionKind::FileSet,
            LEGACY_MANIFEST_VERSION,
            TransactionKind::Recall
        ) | (
            TransactionKind::AtomicDirectory,
            LEGACY_MANIFEST_VERSION,
            TransactionKind::Dump
        ) | (
            TransactionKind::FileSet,
            MANIFEST_VERSION,
            TransactionKind::FileSet
        ) | (
            TransactionKind::AtomicDirectory,
            MANIFEST_VERSION,
            TransactionKind::AtomicDirectory
        )
    ) {
        return Err(LambError::Validation(
            "transaction manifest kind does not match recovery API".to_string(),
        ));
    }
    validate_lexical_path(expected_staging_root)?;
    validate_lexical_path(expected_output_root)?;
    let staging_root_path = manifest.path(manifest.staging_root_path);
    let output_root_path = manifest.path(manifest.output_root);
    if staging_root_path != expected_staging_root || output_root_path != expected_output_root {
        return Err(LambError::Validation(
            "transaction manifest roots do not match approved recovery roots".to_string(),
        ));
    }
    if manifest.staging_root_identity.is_none() {
        return Err(LambError::Validation(
            "transaction staging root has no recorded identity".to_string(),
        ));
    }
    let staging_name = staging_root_path.file_name().and_then(|name| name.to_str());
    let transaction_id = manifest.transaction_id_str();
    let expected_staging_name = match expected_kind {
        TransactionKind::FileSet => transaction_id.to_string(),
        TransactionKind::AtomicDirectory => format!(".tmp-lamb-{transaction_id}"),
        TransactionKind::Recall | TransactionKind::Dump => unreachable!("strategy only"),
    };
    if staging_name != Some(expected_staging_name.as_str()) {
        return Err(LambError::Validation(
            "transaction id does not match its staging directory".to_string(),
        ));
    }
    if let ManifestPhase::Publishing { index } = manifest.phase {
        if index >= manifest.entry_count {
            return Err(LambError::Validation(
                "manifest publication index exceeds its file entries".to_string(),
            ));
        }
    }
    if expected_kind == TransactionKind::FileSet {
        if manifest_path != expected_staging_root.join(RECALL_MANIFEST_NAME) {
            return Err(LambError::Validation(
                "recall manifest is not inside its transaction directory".to_string(),
            ));
        }
    } else {
        let expected_name = format!(".{transaction_id}.manifest.json");
        if manifest_path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str())
        {
            return Err(LambError::Validation(
                "dump manifest name does not match its transaction id".to_string(),
            ));
        }
    }
    reject_symlink_path(expected_staging_root)?;
    reject_symlink_path(expected_output_root)?;
    reject_symlink_path(manifest_path)?;

    let mut paths = HashSet::new();
    insert_unique_path(&mut paths, staging_root_path)?;
    insert_unique_path(&mut paths, manifest_path)?;
    if expected_kind == TransactionKind::AtomicDirectory {
        let (final_directory_path, _) = manifest.final_directory().ok_or_else(|| {
            LambError::Validation("dump manifest has no final directory".to_string())
        })?;
        validate_direct_child(expected_output_root, staging_root_path)?;
        validate_direct_child(expected_output_root, final_directory_path)?;
        insert_unique_path(&mut paths, final_directory_path)?;
    } else if manifest.final_directory().is_some() {
        return Err(LambError::Validation(
            "recall manifest must not name a final directory".to_string(),
        ));
    }

    if expected_kind == TransactionKind::FileSet {
        for (index, directory) in manifest.directories().iter().enumerate() {
            let path = manifest.directory_path(directory)?;
            let entry = manifest
                .entries()
                .get(directory.entry_index as usize)
                .ok_or_else(|| {
                    LambError::Validation(
                        "directory entry index exceeds sparse entry count".to_string(),
                    )
                })?;
            let (final_path, _) = manifest.final_of(entry);
            validate_contained(expected_output_root, path)?;
            validate_contained(path, final_path)?;
            if path == expected_output_root {
                return Err(LambError::Validation(
                    "created directory journal may not own output root".to_string(),
                ));
            }
            for previous in &manifest.directories()[..index] {
                let previous = manifest.directory_path(previous)?;
                if previous == path {
                    return Err(LambError::Validation(
                        "created directory journal contains duplicate derived path".to_string(),
                    ));
                }
                if previous.starts_with(path) && previous != path {
                    return Err(LambError::Validation(
                        "created directory journal is not parent-first".to_string(),
                    ));
                }
            }
            match (directory.state, directory.identity) {
                (Some(ManifestDirectoryState::Intent), None)
                | (Some(ManifestDirectoryState::Owned), Some(_)) => {}
                _ => {
                    return Err(LambError::Validation(
                        "created directory journal state and identity disagree".to_string(),
                    ))
                }
            }
        }
    } else if manifest.directory_count != 0 {
        return Err(LambError::Validation(
            "atomic-directory manifest may not contain created directory journal".to_string(),
        ));
    }

    for entry in manifest.entries() {
        let (staged_path, _) = manifest.staged(entry);
        let (final_path, _) = manifest.final_of(entry);
        validate_contained(expected_staging_root, staged_path)?;
        validate_contained(expected_output_root, final_path)?;
        validate_direct_child(
            staged_path
                .parent()
                .ok_or_else(|| LambError::Validation("staged path has no parent".to_string()))?,
            staged_path,
        )?;
        if expected_kind == TransactionKind::FileSet {
            let (partial_path, _) = manifest.partial(entry).ok_or_else(|| {
                LambError::Validation("recall entry has no adjacent partial path".to_string())
            })?;
            let final_parent = final_path
                .parent()
                .ok_or_else(|| LambError::Validation("final path has no parent".to_string()))?;
            if partial_path.parent() != Some(final_parent) {
                return Err(LambError::Validation(
                    "file-set partial is not adjacent to its final".to_string(),
                ));
            }
            let partial_name = partial_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    LambError::Validation("partial path has an invalid filename".to_string())
                })?;
            validate_component(partial_name, "partial filename")?;
            if !partial_name.starts_with('.') || !partial_name.ends_with(".partial") {
                return Err(LambError::Validation(
                    "recall partial filename is not transaction-hidden".to_string(),
                ));
            }
            insert_unique_path(&mut paths, partial_path)?;
        } else {
            if manifest.partial(entry).is_some() {
                return Err(LambError::Validation(
                    "dump entry must not contain a partial file".to_string(),
                ));
            }
            let (final_directory_path, _) = manifest.final_directory().expect("validated above");
            validate_direct_child(staging_root_path, staged_path)?;
            validate_direct_child(final_directory_path, final_path)?;
        }
        insert_unique_path(&mut paths, staged_path)?;
        insert_unique_path(&mut paths, final_path)?;
        reject_symlink_path(staged_path)?;
        if let Some((partial_path, _)) = manifest.partial(entry) {
            reject_symlink_path(partial_path)?;
        }
        reject_symlink_path(final_path)?;
    }
    validate_phase_state(manifest)?;
    validate_identity_uniqueness(manifest)?;
    Ok(())
}

fn validate_phase_state(manifest: &TransactionManifest) -> Result<()> {
    if matches!(
        manifest.kind,
        TransactionKind::Dump | TransactionKind::AtomicDirectory
    ) {
        if matches!(manifest.phase, ManifestPhase::Publishing { .. }) {
            return Err(LambError::Validation(
                "dump manifest has an impossible publishing phase".to_string(),
            ));
        }
        let directory_identity = manifest
            .final_directory()
            .and_then(|(_, identity)| identity)
            .ok_or_else(|| {
                LambError::Validation("dump final directory identity is missing".to_string())
            })?;
        if manifest.staging_root_identity != Some(directory_identity)
            || manifest.entries().iter().any(|entry| {
                entry.staged_identity.is_none() || entry.final_identity != entry.staged_identity
            })
        {
            return Err(LambError::Validation(
                "dump manifest identities do not describe one directory rename".to_string(),
            ));
        }
        return Ok(());
    }

    match manifest.phase {
        ManifestPhase::Prepared => {
            if manifest
                .entries()
                .iter()
                .any(|entry| entry.final_identity.is_some() || entry.partial_identity.is_some())
            {
                return Err(LambError::Validation(
                    "prepared recall manifest contains publication identities".to_string(),
                ));
            }
        }
        ManifestPhase::Complete => {
            if manifest
                .entries()
                .iter()
                .any(|entry| entry.final_identity.is_none() || entry.partial_identity.is_some())
            {
                return Err(LambError::Validation(
                    "complete recall manifest has incomplete identities".to_string(),
                ));
            }
        }
        ManifestPhase::Publishing { index: active } => {
            for (index, entry) in manifest.entries().iter().enumerate() {
                let partial_identity = entry.partial_identity;
                let valid = if index < active {
                    entry.final_identity.is_some() && partial_identity.is_none()
                } else if index == active {
                    !(entry.final_identity.is_some() && partial_identity.is_some())
                } else {
                    entry.final_identity.is_none() && partial_identity.is_none()
                };
                if !valid {
                    return Err(LambError::Validation(
                        "recall manifest identities contradict publication phase".to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn complete_final_set(manifest: &TransactionManifest) -> Result<bool> {
    if matches!(
        manifest.kind,
        TransactionKind::Dump | TransactionKind::AtomicDirectory
    ) {
        let Some((final_directory_path, identity)) = manifest.final_directory() else {
            return Ok(false);
        };
        let Some(identity) = identity else {
            return Ok(false);
        };
        if !identity_matches(final_directory_path, identity, true)? {
            return Ok(false);
        }
    }
    for (index, entry) in manifest.entries().iter().enumerate() {
        let identity = inferred_final_identity(manifest, index)?;
        let Some(identity) = identity else {
            return Ok(false);
        };
        let (final_path, _) = manifest.final_of(entry);
        if !identity_matches(final_path, identity, false)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Synchronizes FileSet parents without an allocation-backed traversal. Entries
/// are already sparse and ordered deterministically; scanning prior entries
/// suppresses repeated ancestors while each path is visited root-first.
fn sync_file_set_directories(manifest: &TransactionManifest, output_root: &Path) -> Result<()> {
    sync_directory(output_root)?;
    for (index, entry) in manifest.entries().iter().enumerate() {
        let (final_path, _) = manifest.final_of(entry);
        let Some(parent) = final_path.parent() else {
            continue;
        };
        let root = output_root
            .to_str()
            .ok_or_else(|| LambError::Validation("output root is not valid UTF-8".to_string()))?;
        let parent = parent
            .to_str()
            .ok_or_else(|| LambError::Validation("final parent is not valid UTF-8".to_string()))?;
        let mut end = root.len();
        while end < parent.len() {
            end = parent[end + 1..]
                .find('/')
                .map(|offset| end + 1 + offset)
                .unwrap_or(parent.len());
            let candidate = Path::new(&parent[..end]);
            let seen = manifest.entries()[..index].iter().any(|prior| {
                let (prior_final, _) = manifest.final_of(prior);
                prior_final
                    .parent()
                    .is_some_and(|prior_parent| prior_parent.starts_with(candidate))
            });
            if !seen {
                sync_directory(candidate)?;
            }
            if end == parent.len() {
                break;
            }
        }
    }
    Ok(())
}

fn inferred_final_identity(
    manifest: &TransactionManifest,
    index: usize,
) -> Result<Option<FileIdentity>> {
    let entry = &manifest.entries()[index];
    if entry.final_identity.is_some() {
        return Ok(entry.final_identity);
    }
    if !matches!(manifest.phase, ManifestPhase::Publishing { index: active } if active == index) {
        return Ok(None);
    }
    let Some(partial_identity) = entry.partial_identity else {
        return Ok(None);
    };
    let (partial_path, _) = manifest
        .partial(entry)
        .expect("publishing recall entry has a partial path");
    if path_exists(partial_path)? {
        return Ok(None);
    }
    Ok(Some(partial_identity))
}

fn cleanup_file(path: &Path, identity: Option<FileIdentity>) -> Result<bool> {
    let Some(identity) = identity else {
        return Ok(!path_exists(path)?);
    };
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io_error(path, error)),
        Ok(metadata) if identity_from_metadata(&metadata) != identity => Ok(true),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Ok(false)
        }
        Ok(_) => match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(_) => Ok(false),
        },
    }
}

fn remove_directory_if_identity(path: &Path, identity: Option<FileIdentity>) -> Result<bool> {
    let Some(identity) = identity else {
        return Ok(!path_exists(path)?);
    };
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io_error(path, error)),
        Ok(metadata) if identity_from_metadata(&metadata) != identity => Ok(true),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() => {
            Ok(false)
        }
        Ok(_) => match fs::remove_dir(path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(_) => Ok(false),
        },
    }
}

fn best_effort_remove_recall_metadata(
    manifest: &TransactionManifest,
    manifest_path: &Path,
    manifest_identity: FileIdentity,
) {
    for entry in manifest.entries() {
        let (staged_path, staged_identity) = manifest.staged(entry);
        let _ = cleanup_file(staged_path, staged_identity);
        if let Some((partial_path, partial_identity)) = manifest.partial(entry) {
            let _ = cleanup_file(partial_path, partial_identity);
        }
    }
    let staging_root = manifest.path(manifest.staging_root_path);
    if directory_contains_only(staging_root, manifest_path).unwrap_or(false) {
        let _ = remove_if_identity(manifest_path, manifest_identity);
        let _ = remove_directory_if_identity(staging_root, manifest.staging_root_identity);
    }
}

fn directory_contains_only(directory: &Path, allowed: &Path) -> Result<bool> {
    let entries = fs::read_dir(directory).map_err(|source| io_error(directory, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| io_error(directory, source))?;
        if entry.path() != allowed {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_identity_uniqueness(manifest: &TransactionManifest) -> Result<()> {
    let mut identities = HashSet::new();
    if let Some(identity) = manifest.staging_root_identity {
        identities.insert((identity.device, identity.inode));
    }
    for entry in manifest.entries() {
        if let Some(identity) = entry.staged_identity {
            if !identities.insert((identity.device, identity.inode)) {
                return Err(LambError::Validation(
                    "transaction manifest contains duplicate owned identity".to_string(),
                ));
            }
        }
        if matches!(
            manifest.kind,
            TransactionKind::Recall | TransactionKind::FileSet
        ) {
            for identity in [entry.partial_identity, entry.final_identity]
                .into_iter()
                .flatten()
            {
                if !identities.insert((identity.device, identity.inode)) {
                    return Err(LambError::Validation(
                        "transaction manifest contains duplicate owned identity".to_string(),
                    ));
                }
            }
        }
    }
    for directory in manifest.directories() {
        if let Some(identity) = directory.identity {
            if !identities.insert((identity.device, identity.inode)) {
                return Err(LambError::Validation(
                    "transaction manifest contains duplicate owned identity".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn sync_regular_file(path: &Path, expected: FileIdentity) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error(path, source))?;
    let metadata = file.metadata().map_err(|source| io_error(path, source))?;
    if !metadata.file_type().is_file() {
        return Err(LambError::Validation(
            "published path is not a regular file".to_string(),
        ));
    }
    if identity_from_metadata(&metadata) != expected {
        return Err(LambError::Validation(
            "published file identity changed before synchronization".to_string(),
        ));
    }
    file.sync_all().map_err(|source| io_error(path, source))?;
    if !identity_matches(path, expected, false)? {
        return Err(LambError::Validation(
            "published file identity changed during synchronization".to_string(),
        ));
    }
    Ok(())
}

fn validate_component(component: &str, description: &str) -> Result<()> {
    let mut components = Path::new(component).components();
    if component.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(LambError::Validation(format!(
            "{description} is not a safe path component"
        )));
    }
    Ok(())
}

fn validate_lexical_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(LambError::Validation(format!(
            "path is not absolute and lexically canonical: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_contained(root: &Path, path: &Path) -> Result<()> {
    validate_lexical_path(path)?;
    let relative = path.strip_prefix(root).map_err(|_| {
        LambError::Validation(format!(
            "path escapes approved root {}: {}",
            root.display(),
            path.display()
        ))
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LambError::Validation(format!(
            "path is not canonically contained by {}: {}",
            root.display(),
            path.display()
        )));
    }
    Ok(())
}

fn validate_direct_child(parent: &Path, path: &Path) -> Result<()> {
    validate_contained(parent, path)?;
    let relative = path.strip_prefix(parent).expect("validated containment");
    if relative.components().count() != 1 {
        return Err(LambError::Validation(format!(
            "path is not an adjacent child of {}: {}",
            parent.display(),
            path.display()
        )));
    }
    let name = relative
        .to_str()
        .ok_or_else(|| LambError::Validation("manifest path is not valid UTF-8".to_string()))?;
    validate_component(name, "manifest filename")
}

fn insert_unique_path(paths: &mut HashSet<PathBuf>, path: &Path) -> Result<()> {
    if !paths.insert(path.to_path_buf()) {
        return Err(LambError::Validation(format!(
            "transaction manifest contains duplicate path {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_symlink_path(path: &Path) -> Result<()> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(LambError::Validation(format!(
                    "manifest path traverses a symlink: {}",
                    prefix.display()
                )))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(io_error(&prefix, error)),
        }
    }
    Ok(())
}

fn identity_matches(path: &Path, expected: FileIdentity, directory: bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
        Ok(metadata) => {
            let right_kind = if directory {
                metadata.file_type().is_dir()
            } else {
                metadata.file_type().is_file()
            };
            Ok(!metadata.file_type().is_symlink()
                && right_kind
                && identity_from_metadata(&metadata) == expected)
        }
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(path, error)),
    }
}

fn identity_from_metadata(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn read_manifest_identity(path: &Path) -> io::Result<FileIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest is not a regular file",
        ));
    }
    Ok(identity_from_metadata(&metadata))
}

fn remove_if_identity(path: &Path, expected: FileIdentity) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
        Ok(metadata) if identity_from_metadata(&metadata) != expected => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Ok(())
        }
        Ok(_) => fs::remove_file(path).map_err(|source| io_error(path, source)),
    }
}

fn manifest_temp_path(manifest_path: &Path, transaction_id: &str) -> Result<PathBuf> {
    let file_name = manifest_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LambError::Validation("manifest filename is invalid".to_string()))?;
    Ok(manifest_path.with_file_name(format!(".{file_name}.{transaction_id}.tmp")))
}

fn cleanup_dump_temp_manifest(manifest_path: &Path, transaction_id: &str) -> Result<bool> {
    let temp_path = manifest_temp_path(manifest_path, transaction_id)?;
    match fs::symlink_metadata(&temp_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io_error(&temp_path, error)),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.file_type().is_file() => {
            Ok(true)
        }
        Ok(_) => match fs::remove_file(&temp_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(_) => Ok(false),
        },
    }
}

struct SliceWriter<'a> {
    buffer: &'a mut [u8],
    used: usize,
    overflowed: bool,
}

impl Write for SliceWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(end) = self.used.checked_add(bytes.len()) else {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "manifest too large",
            ));
        };
        if end > self.buffer.len() {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "manifest too large",
            ));
        }
        self.buffer[self.used..end].copy_from_slice(bytes);
        self.used = end;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_bounded(buffer: &mut [u8], manifest: &TransactionManifest) -> Result<usize> {
    let mut writer = SliceWriter {
        buffer,
        used: 0,
        overflowed: false,
    };
    let result = serde_json::to_writer(&mut writer, manifest);
    if writer.overflowed {
        return Err(LambError::Validation(
            "manifest exceeds planned serialization capacity".to_string(),
        ));
    }
    result.map_err(|error| {
        LambError::Validation(format!("manifest serialization failed: {error}"))
    })?;
    Ok(writer.used)
}

#[cfg(target_os = "linux")]
fn rename_no_replace(old: &Path, new: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let old = std::ffi::CString::new(old.as_os_str().as_bytes())
        .map_err(|_| LambError::Validation("temporary manifest path contains NUL".to_string()))?;
    let new = std::ffi::CString::new(new.as_os_str().as_bytes())
        .map_err(|_| LambError::Validation("manifest path contains NUL".to_string()))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(
            PathBuf::from(new.to_string_lossy().into_owned()),
            io::Error::last_os_error(),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_old: &Path, _new: &Path) -> Result<()> {
    Err(LambError::Validation(
        "atomic no-overwrite manifest publication requires Linux renameat2".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn rename_exchange(old: &Path, new: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let old_c = std::ffi::CString::new(old.as_os_str().as_bytes())
        .map_err(|_| LambError::Validation("temporary manifest path contains NUL".to_string()))?;
    let new_c = std::ffi::CString::new(new.as_os_str().as_bytes())
        .map_err(|_| LambError::Validation("manifest path contains NUL".to_string()))?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            old_c.as_ptr(),
            libc::AT_FDCWD,
            new_c.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io_error(new, io::Error::last_os_error()))
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_exchange(_old: &Path, _new: &Path) -> Result<()> {
    Err(LambError::Validation(
        "atomic manifest replacement requires Linux renameat2".to_string(),
    ))
}
