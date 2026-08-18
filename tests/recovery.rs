use lamb::error::LambError;
use lamb::recovery::{
    capture_identity, recover_dump_parent, recover_recall_root, FileIdentity, ManifestEntrySlot,
    ManifestPhase, ManifestStore, PathRef, RecoveryOutcome, TransactionKind, TransactionManifest,
    MANIFEST_VERSION,
};
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::path::{Path, PathBuf};

const BUFFER_BYTES: usize = 64 * 1024;
const PATH_ARENA_BYTES: usize = 512 * 1024;
const ENTRY_CAPACITY: usize = 16;

fn identity(path: &Path) -> FileIdentity {
    let metadata = fs::symlink_metadata(path).unwrap();
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

fn fresh_manifest() -> (Vec<ManifestEntrySlot>, Vec<u8>) {
    (
        vec![ManifestEntrySlot::default(); ENTRY_CAPACITY],
        vec![0_u8; PATH_ARENA_BYTES],
    )
}

fn write_manifest(path: &Path, manifest: &TransactionManifest) {
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    ManifestStore::new(&mut buffer)
        .write(path, manifest)
        .unwrap();
}

fn recall_manifest<'a>(
    slots: &'a mut [ManifestEntrySlot],
    path_bytes: &'a mut [u8],
    transaction_root: &Path,
    output_root: &Path,
    final_count: usize,
) -> TransactionManifest<'a> {
    fs::create_dir_all(transaction_root).unwrap();
    fs::create_dir_all(output_root).unwrap();
    let mut manifest = TransactionManifest::new(slots, path_bytes);
    manifest.version = MANIFEST_VERSION;
    manifest.uid = unsafe { libc::geteuid() };
    manifest.kind = TransactionKind::Recall;
    manifest.phase = if final_count == 2 {
        ManifestPhase::Complete
    } else {
        ManifestPhase::Publishing { index: final_count }
    };
    let transaction_id = transaction_root
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    manifest.transaction_id = manifest.push_path(&transaction_id).unwrap();
    manifest.staging_root_path = manifest
        .push_path(transaction_root.to_str().unwrap())
        .unwrap();
    manifest.staging_root_identity = Some(identity(transaction_root));
    manifest.output_root = manifest.push_path(output_root.to_str().unwrap()).unwrap();
    manifest.set_entry_count(2).unwrap();
    for index in 0..2 {
        let staged_path = transaction_root.join(format!("channel-{index}.wav"));
        let partial_path = output_root.join(format!(".channel-{index}.wav.tx.partial"));
        let final_path = output_root.join(format!("channel-{index}.wav"));
        fs::write(&staged_path, b"staged").unwrap();
        let final_identity = if index < final_count {
            fs::write(&final_path, b"published").unwrap();
            Some(identity(&final_path))
        } else {
            None
        };
        let staged_ref = manifest.push_path(staged_path.to_str().unwrap()).unwrap();
        let partial_ref = manifest.push_path(partial_path.to_str().unwrap()).unwrap();
        let final_ref = manifest.push_path(final_path.to_str().unwrap()).unwrap();
        let slot = manifest.entry_mut(index);
        slot.staged_path = staged_ref;
        slot.staged_identity = Some(identity(&staged_path));
        slot.partial_path = partial_ref;
        slot.partial_identity = None;
        slot.final_path = final_ref;
        slot.final_identity = final_identity;
    }
    manifest
}

fn dump_manifest<'a>(
    slots: &'a mut [ManifestEntrySlot],
    path_bytes: &'a mut [u8],
    parent: &Path,
    complete: bool,
) -> (PathBuf, TransactionManifest<'a>) {
    fs::create_dir_all(parent).unwrap();
    let transaction_id = "dump-transaction";
    let hidden = parent.join(format!(".tmp-lamb-{transaction_id}"));
    let final_dir = parent.join("20260818T120000");
    fs::create_dir(&hidden).unwrap();
    let hidden_file = hidden.join("mic.wav");
    fs::write(&hidden_file, b"wav").unwrap();
    let directory_identity = identity(&hidden);
    let file_identity = identity(&hidden_file);
    if complete {
        fs::rename(&hidden, &final_dir).unwrap();
    }
    let mut manifest = TransactionManifest::new(slots, path_bytes);
    manifest.version = MANIFEST_VERSION;
    manifest.uid = unsafe { libc::geteuid() };
    manifest.kind = TransactionKind::Dump;
    manifest.phase = if complete {
        ManifestPhase::Complete
    } else {
        ManifestPhase::Prepared
    };
    manifest.transaction_id = manifest.push_path(transaction_id).unwrap();
    manifest.staging_root_path = manifest.push_path(hidden.to_str().unwrap()).unwrap();
    manifest.staging_root_identity = Some(directory_identity);
    manifest.output_root = manifest.push_path(parent.to_str().unwrap()).unwrap();
    manifest.final_directory_path = manifest.push_path(final_dir.to_str().unwrap()).unwrap();
    manifest.final_directory_identity = Some(directory_identity);
    manifest.set_entry_count(1).unwrap();
    let staged_ref = manifest.push_path(hidden_file.to_str().unwrap()).unwrap();
    let final_ref = manifest
        .push_path(final_dir.join("mic.wav").to_str().unwrap())
        .unwrap();
    let slot = manifest.entry_mut(0);
    slot.staged_path = staged_ref;
    slot.staged_identity = Some(file_identity);
    slot.partial_path = PathRef::default();
    slot.final_path = final_ref;
    slot.final_identity = Some(file_identity);
    (
        parent.join(format!(".{transaction_id}.manifest.json")),
        manifest,
    )
}

#[test]
fn manifest_entry_capacity_fits_exactly_and_one_beyond_is_rejected() {
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = TransactionManifest::new(&mut slots, &mut path_bytes);
    manifest.set_entry_count(ENTRY_CAPACITY).unwrap();
    assert_eq!(manifest.entries().len(), ENTRY_CAPACITY);

    let (mut slots2, mut path_bytes2) = fresh_manifest();
    let mut manifest2 = TransactionManifest::new(&mut slots2, &mut path_bytes2);
    let error = manifest2.set_entry_count(ENTRY_CAPACITY + 1).unwrap_err();
    assert!(error.to_string().contains("capacity"));
}

#[test]
fn maximum_sized_paths_fit_the_reserved_arena_and_overflow_is_rejected() {
    let max_path = 4096_usize;
    let arena = (ENTRY_CAPACITY * 3 + 5) * max_path;
    let mut path_bytes = vec![0_u8; arena];
    let mut slots = vec![ManifestEntrySlot::default(); ENTRY_CAPACITY];
    let mut manifest = TransactionManifest::new(&mut slots, &mut path_bytes);
    manifest.set_entry_count(ENTRY_CAPACITY).unwrap();
    for index in 0..ENTRY_CAPACITY {
        let staged = format!("/staged/{index:04}/{}", "x".repeat(max_path - 20));
        let partial = format!("/partial/{index:04}/{}", "x".repeat(max_path - 20));
        let final_path = format!("/final/{index:04}/{}", "x".repeat(max_path - 20));
        let staged_ref = manifest.push_path(&staged).unwrap();
        let partial_ref = manifest.push_path(&partial).unwrap();
        let final_ref = manifest.push_path(&final_path).unwrap();
        let slot = manifest.entry_mut(index);
        slot.staged_path = staged_ref;
        slot.partial_path = partial_ref;
        slot.final_path = final_ref;
    }
    let overflow = "x".repeat(arena + 1);
    assert!(manifest.push_path(&overflow).is_err());
}

#[test]
fn recovery_of_a_maximum_manifest_uses_reserved_storage() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("staging").join("max-recall");
    let output = root.path().join("output");
    fs::create_dir_all(&transaction).unwrap();
    fs::create_dir_all(&output).unwrap();

    let (mut build_slots, mut build_paths) = fresh_manifest();
    let mut manifest = TransactionManifest::new(&mut build_slots, &mut build_paths);
    manifest.version = MANIFEST_VERSION;
    manifest.uid = unsafe { libc::geteuid() };
    manifest.kind = TransactionKind::Recall;
    manifest.phase = ManifestPhase::Complete;
    manifest.transaction_id = manifest.push_path("max-recall").unwrap();
    manifest.staging_root_path = manifest.push_path(transaction.to_str().unwrap()).unwrap();
    manifest.staging_root_identity = Some(identity(&transaction));
    manifest.output_root = manifest.push_path(output.to_str().unwrap()).unwrap();
    manifest.set_entry_count(ENTRY_CAPACITY).unwrap();
    for index in 0..ENTRY_CAPACITY {
        let staged_path = transaction.join(format!("channel-{index:02}.wav"));
        let partial_path = output.join(format!(".channel-{index:02}.wav.max.partial"));
        let final_path = output.join(format!("channel-{index:02}.wav"));
        fs::write(&staged_path, b"staged").unwrap();
        fs::write(&final_path, b"published").unwrap();
        let staged_ref = manifest.push_path(staged_path.to_str().unwrap()).unwrap();
        let partial_ref = manifest.push_path(partial_path.to_str().unwrap()).unwrap();
        let final_ref = manifest.push_path(final_path.to_str().unwrap()).unwrap();
        let slot = manifest.entry_mut(index);
        slot.staged_path = staged_ref;
        slot.staged_identity = Some(identity(&staged_path));
        slot.partial_path = partial_ref;
        slot.partial_identity = None;
        slot.final_path = final_ref;
        slot.final_identity = Some(identity(&final_path));
    }
    write_manifest(&transaction.join("manifest.json"), &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();
    assert_eq!(outcome, RecoveryOutcome::Complete);
    assert!(!transaction.exists());
    for index in 0..ENTRY_CAPACITY {
        assert!(output.join(format!("channel-{index:02}.wav")).exists());
    }
}

#[test]
fn recovery_preserves_complete_recall_and_removes_transaction_metadata() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("staging").join("recall-1");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 2);
    let expected: Vec<PathBuf> = (0..2)
        .map(|index| output.join(format!("channel-{index}.wav")))
        .collect();
    write_manifest(&transaction.join("manifest.json"), &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::Complete);
    assert!(expected.iter().all(|path| path.exists()));
    assert!(!transaction.exists());
}

#[test]
fn recovery_rolls_back_only_owned_paths_from_partial_recall() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("staging").join("recall-2");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let partial = output.join(".channel-1.wav.tx.partial");
    fs::write(&partial, b"partial").unwrap();
    manifest.entry_mut(1).partial_identity = Some(identity(&partial));
    let published = output.join("channel-0.wav");
    write_manifest(&transaction.join("manifest.json"), &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::RolledBack);
    assert!(!published.exists());
    assert!(!partial.exists());
    assert!(!transaction.exists());
}

#[test]
fn recovery_preserves_foreign_inode_replacements() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("staging").join("recall-3");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let published = output.join("channel-0.wav");
    let replacement = output.join("replacement");
    fs::write(&replacement, b"foreign").unwrap();
    fs::rename(&replacement, &published).unwrap();
    write_manifest(&transaction.join("manifest.json"), &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::RolledBack);
    assert_eq!(fs::read(&published).unwrap(), b"foreign");
}

#[test]
fn dump_recovery_preserves_complete_directory_and_removes_manifest() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let (manifest_path, manifest) = dump_manifest(&mut slots, &mut path_bytes, &parent, true);
    let final_dir = parent.join("20260818T120000");
    write_manifest(&manifest_path, &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_dump_parent(
        &parent,
        &manifest_path,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::Complete);
    assert_eq!(fs::read(final_dir.join("mic.wav")).unwrap(), b"wav");
    assert!(!manifest_path.exists());
}

#[test]
fn dump_recovery_removes_incomplete_owned_hidden_directory() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let (manifest_path, manifest) = dump_manifest(&mut slots, &mut path_bytes, &parent, false);
    let hidden = parent.join(".tmp-lamb-dump-transaction");
    write_manifest(&manifest_path, &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_dump_parent(
        &parent,
        &manifest_path,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::RolledBack);
    assert!(!hidden.exists());
    assert!(!manifest_path.exists());
}

#[test]
fn dump_recovery_removes_temp_manifest_orphan_and_preserves_foreign_sibling() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let (manifest_path, manifest) = dump_manifest(&mut slots, &mut path_bytes, &parent, true);
    write_manifest(&manifest_path, &manifest);
    let transaction_id = "dump-transaction";
    let temp_path = parent.join(format!(
        "..{transaction_id}.manifest.json.{transaction_id}.tmp"
    ));
    fs::write(&temp_path, b"orphaned temp manifest").unwrap();
    let foreign_path = parent.join("..foreign-transaction.manifest.json.foreign-transaction.tmp");
    fs::write(&foreign_path, b"foreign").unwrap();
    let final_dir = parent.join("20260818T120000");

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_dump_parent(
        &parent,
        &manifest_path,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::Complete);
    assert_eq!(fs::read(final_dir.join("mic.wav")).unwrap(), b"wav");
    assert!(!manifest_path.exists());
    assert!(!temp_path.exists());
    assert_eq!(fs::read(&foreign_path).unwrap(), b"foreign");
}

#[test]
fn incomplete_owned_final_dump_stays_pending_with_manifest() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let (manifest_path, manifest) = dump_manifest(&mut slots, &mut path_bytes, &parent, true);
    let final_dir = parent.join("20260818T120000");
    fs::remove_file(final_dir.join("mic.wav")).unwrap();
    write_manifest(&manifest_path, &manifest);

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let outcome = recover_dump_parent(
        &parent,
        &manifest_path,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::Pending);
    assert!(final_dir.exists());
    assert!(manifest_path.exists());
}

#[test]
fn unmarked_legacy_artifacts_are_never_removed() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("legacy-staging");
    let output = root.path().join("output");
    fs::create_dir_all(&transaction).unwrap();
    fs::create_dir_all(&output).unwrap();
    let legacy_staged = transaction.join("legacy.wav");
    let legacy_partial = output.join("legacy.wav.partial");
    fs::write(&legacy_staged, b"legacy").unwrap();
    fs::write(&legacy_partial, b"legacy").unwrap();

    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];
    let error = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(error.to_string().contains("manifest"));
    assert_eq!(fs::read(legacy_staged).unwrap(), b"legacy");
    assert_eq!(fs::read(legacy_partial).unwrap(), b"legacy");
}

#[test]
fn manifest_store_rejects_oversized_serialization_before_writing() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 0);
    let manifest_path = transaction.join("manifest.json");
    let mut buffer = [0_u8; 64];

    let error = ManifestStore::new(&mut buffer)
        .write(&manifest_path, &manifest)
        .unwrap_err();

    assert!(matches!(error, LambError::Validation(_)));
    assert!(!manifest_path.exists());
    assert_eq!(fs::read_dir(&transaction).unwrap().count(), 2);
}

#[test]
fn recovery_rejects_oversized_json_without_following_manifest_paths() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    fs::create_dir_all(&transaction).unwrap();
    fs::create_dir_all(&output).unwrap();
    let outside = root.path().join("outside");
    fs::write(&outside, b"foreign").unwrap();
    fs::write(
        transaction.join("manifest.json"),
        format!(
            "{{\"path\":\"{}\",\"padding\":\"{}\"}}",
            outside.display(),
            "x".repeat(512)
        ),
    )
    .unwrap();
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = [0_u8; 128];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert_eq!(fs::read(&outside).unwrap(), b"foreign");
}

#[test]
fn recovery_rejects_wrong_uid_version_duplicate_and_malformed_numeric_fields() {
    let cases = [
        ("wrong uid", "\"uid\":0", "\"uid\":4294967295"),
        ("wrong version", "\"version\":1", "\"version\":99"),
        ("malformed uid", "\"uid\":0", "\"uid\":-1"),
        ("malformed identity", "\"device\":0", "\"device\":1.5"),
    ];
    for (name, needle, replacement) in cases {
        let root = tempfile::tempdir().unwrap();
        let transaction = root.path().join("tx");
        let output = root.path().join("output");
        let (mut slots, mut path_bytes) = fresh_manifest();
        let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
        manifest.uid = 0;
        manifest.staging_root_identity.as_mut().unwrap().device = 0;
        let manifest_path = transaction.join("manifest.json");
        let json = serde_json::to_string(&manifest)
            .unwrap()
            .replace(needle, replacement);
        fs::write(&manifest_path, json).unwrap();
        let owned_final = output.join("channel-0.wav");
        let (mut rec_slots, mut rec_paths) = fresh_manifest();
        let mut buffer = vec![0_u8; BUFFER_BYTES];

        let error = recover_recall_root(
            &transaction,
            &output,
            &mut buffer,
            &mut rec_slots,
            &mut rec_paths,
        )
        .unwrap_err()
        .to_string();

        assert!(!error.is_empty(), "{name} unexpectedly succeeded");
        assert!(owned_final.exists(), "{name} followed an invalid manifest");
    }

    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let duplicate = manifest.entry(0).staged_identity;
    manifest.entry_mut(1).staged_identity = duplicate;
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let owned_final = output.join("channel-0.wav");
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(owned_final.exists());
}

#[test]
fn recovery_rejects_traversal_and_symlink_components_before_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let outside = root.path().join("outside.wav");
    fs::write(&outside, b"foreign").unwrap();
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut traversal = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 0);
    let outside_path = output.join("..").join("outside.wav");
    traversal.entry_mut(0).final_path =
        traversal.push_path(outside_path.to_str().unwrap()).unwrap();
    write_manifest(&transaction.join("manifest.json"), &traversal);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert_eq!(fs::read(&outside).unwrap(), b"foreign");

    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let foreign = root.path().join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("partial"), b"foreign").unwrap();
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 0);
    let linked = output.join("linked");
    symlink(&foreign, &linked).unwrap();
    let linked_path = linked.join("partial");
    manifest.entry_mut(0).partial_path = manifest.push_path(linked_path.to_str().unwrap()).unwrap();
    manifest.entry_mut(0).partial_identity =
        Some(capture_identity(&foreign.join("partial")).unwrap());
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert_eq!(fs::read(foreign.join("partial")).unwrap(), b"foreign");
}

#[test]
fn recovery_rejects_mismatched_transaction_name_and_out_of_range_phase() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let owned_final = output.join("channel-0.wav");
    manifest.transaction_id = manifest.push_path("different-safe-id").unwrap();
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(owned_final.exists());

    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let owned_final = output.join("channel-0.wav");
    manifest.phase = ManifestPhase::Publishing { index: 99 };
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(owned_final.exists());
}

#[test]
fn recovery_rejects_identity_state_that_could_not_be_published() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let owned_final = output.join("channel-0.wav");
    manifest.phase = ManifestPhase::Prepared;
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(owned_final.exists());

    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("dumps");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let (manifest_path, mut manifest) = dump_manifest(&mut slots, &mut path_bytes, &parent, false);
    manifest.phase = ManifestPhase::Publishing { index: 0 };
    write_manifest(&manifest_path, &manifest);
    let hidden = parent.join(".tmp-lamb-dump-transaction");
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_dump_parent(
        &parent,
        &manifest_path,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(hidden.exists());
}

#[test]
fn recovery_rejects_duplicate_owned_identity_before_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let duplicate = manifest.entry(0).staged_identity;
    manifest.entry_mut(1).staged_identity = duplicate;
    let owned_final = output.join("channel-0.wav");
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap_err();

    assert!(owned_final.exists());
}

#[test]
fn foreign_staged_replacement_stays_pending_and_is_preserved() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 1);
    let staged = transaction.join("channel-0.wav");
    let replacement = transaction.join("replacement");
    fs::write(&replacement, b"foreign staged replacement").unwrap();
    fs::rename(&replacement, &staged).unwrap();
    write_manifest(&transaction.join("manifest.json"), &manifest);
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    let outcome = recover_recall_root(
        &transaction,
        &output,
        &mut buffer,
        &mut rec_slots,
        &mut rec_paths,
    )
    .unwrap();

    assert_eq!(outcome, RecoveryOutcome::Pending);
    assert_eq!(fs::read(&staged).unwrap(), b"foreign staged replacement");
    assert!(transaction.join("manifest.json").exists());
}

#[test]
fn manifest_update_never_overwrites_foreign_temp_file() {
    let root = tempfile::tempdir().unwrap();
    let transaction = root.path().join("tx");
    let output = root.path().join("output");
    let (mut slots, mut path_bytes) = fresh_manifest();
    let mut manifest = recall_manifest(&mut slots, &mut path_bytes, &transaction, &output, 0);
    let manifest_path = transaction.join("manifest.json");
    write_manifest(&manifest_path, &manifest);
    let original_phase = manifest.phase.clone();
    let temp_path = transaction.join(".manifest.json.tx.tmp");
    fs::write(&temp_path, b"foreign temp").unwrap();
    manifest.phase = ManifestPhase::Publishing { index: 0 };
    let mut buffer = vec![0_u8; BUFFER_BYTES];

    ManifestStore::new(&mut buffer)
        .write(&manifest_path, &manifest)
        .unwrap_err();

    assert_eq!(fs::read(&temp_path).unwrap(), b"foreign temp");
    let (mut rec_slots, mut rec_paths) = fresh_manifest();
    let mut read_buffer = vec![0_u8; BUFFER_BYTES];
    let stored = ManifestStore::new(&mut read_buffer)
        .read(&manifest_path, &mut rec_slots, &mut rec_paths)
        .unwrap();
    assert_eq!(stored.phase, original_phase);
}
