# Preallocated Publication and Response Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove every sparse/split-output-count-scaled allocation from the daemon's prepared publication and response path while preserving publication, recovery, cursor, wire, and CLI behavior.

**Architecture:** The canonical sparse `FilePlan` remains in `PersistenceWorkspace` from preparation through publication, in-process recovery, coordinator commit, and borrowed response serialization. Startup-owned publication scratch replaces output-sized vectors; the coordinator delivers a borrowed committed outcome under an unconditional completion guard; daemon and CLI persistence paths stream the unchanged JSON file sequence.

**Tech Stack:** Rust 2021, Linux descriptor-relative filesystem APIs, serde/serde_json streaming traits, Unix domain sockets, Nix development/build gates.

**Spec:** `docs/specs/2026-08-26-preallocated-publication-response-design.md`

## Global Constraints

- `docs/specs/2026-08-26-export-policy-activity-calibration-design.md` and the correction spec above are binding.
- No output-count-sized collection may be allocated from frozen selection through daemon response serialization.
- All split-capacity publication scratch must be checked, memory-plan-accounted, allocated, and page-touched before backend activation.
- Prepared file-set and atomic-directory publication remain manifest-backed, identity-checked, no-overwrite, symlink-safe, durable, and crash-recoverable.
- Frozen decisions, selected ranges, and common crop remain immutable across retry.
- Publication/cursor commit precedes response delivery; transport failure never makes durable output retryable.
- Workspace cleanup and frozen release occur exactly once after visitor success, error, or unwind.
- Existing JSON fields, values, ordering of reported files, CLI commands, and user-visible persistence messages remain compatible.
- Legacy allocating snapshot helper APIs remain isolated; current daemon/CaptureSession code must not call them.
- Tests use caller-owned hooks/channels/barriers, never sleeps, polling races, process-global fault switches, or mutable runner environment.
- Implementers do not stage, commit, reset, clean, discard, or delegate; the parent session performs history operations after review.

## File structure

- `src/memory_plan.rs`: exact startup accounting for publication scratch.
- `src/persistence_workspace.rs`: scratch ownership, disjoint file-plan views, compact recovery state, completed-output borrowing, and reset lifecycle.
- `src/export_policy.rs`: canonical pre-staging UTF-8 root/final-path validation.
- `src/export_wav.rs`: vector-free prepared publishers and descriptor-relative scratch consumers; legacy snapshot publishers remain separate.
- `src/recovery.rs`: manifest-backed fixed-slot recovery helpers used by prepared in-process recovery.
- `src/dump.rs`: committed borrowed outcome, delivery visitor, state-lock boundary, and exact-once completion guard.
- `src/control.rs`: borrowed response serialization and streaming persistence-client decode while retaining owned compatibility types.
- `src/daemon.rs`: persistence requests write through the borrowed visitor; non-persistence responses keep the existing writer.
- `tests/memory_plan.rs`: independent publication-scratch accounting and memory-limit proofs.
- `tests/app_config.rs`, `tests/export_policy.rs`: UTF-8 validation before staging.
- `tests/persistence_workspace.rs`: startup addresses and prepare/publish allocation proofs.
- `tests/export_wav.rs`, `tests/recovery.rs`: publication/recovery checkpoint and filesystem invariants.
- `tests/dump_coordinator.rs`: borrowed completion, recovery, delivery failure, cursor, cleanup, and release proofs.
- `tests/daemon_fake.rs`, `tests/daemon_idle.rs`, `tests/threshold_cli.rs`: live wire compatibility and routing regressions.

---

### Task 1: Startup-owned publication scratch and UTF-8 preflight

**Files:**
- Modify: `src/memory_plan.rs`
- Modify: `src/persistence_workspace.rs`
- Modify: `src/export_policy.rs`
- Test: `tests/memory_plan.rs`
- Test: `tests/export_policy.rs`
- Test: `tests/persistence_workspace.rs`

**Interfaces:**
- Consumes: `SessionMemoryPlan::{output_file_slots, manifest_directory_slots, maximum_path_bytes}` and existing `MaterializedBuffer`, `ExactArray`, and `ReusablePath` storage.
- Produces: `PublicationScratch`, `CurrentArtifactSlot`, `DirectorySyncSlot`, two reusable component buffers, fixed partial/manifest path slots, and `SessionMemoryPlan::publication_scratch_bytes()`.

- [ ] **Step 1: Write independent failing memory-plan tests**

Add a test which derives the three concrete publication allocations without calling the production formula:

```rust
#[test]
fn publication_scratch_is_startup_accounted_materialized_and_limit_checked() {
    let inputs = representative_inputs();
    let plan = SessionMemoryPlan::calculate(inputs).unwrap();
    let sync_payload = plan.manifest_directory_slots() * PUBLICATION_SYNC_SLOT_BYTES;
    let component_payload = inputs.maximum_path_bytes.checked_add(1).unwrap();
    let expected = allocation_budget_bytes(sync_payload).unwrap()
        + 2 * allocation_budget_bytes(component_payload).unwrap()
        + allocation_budget_bytes(PUBLICATION_ARTIFACT_SLOT_BYTES).unwrap();
    assert_eq!(plan.publication_scratch_bytes(), expected);
    assert!(plan.validate_max(Some(plan.required_with_headroom())).is_ok());
    assert!(plan
        .validate_max(Some(plan.required_with_headroom() - 1))
        .is_err());
}
```

Extend the workspace address test to require stable nonzero addresses/capacities for the sync slots, artifact slot, and both component buffers across reset/reuse.

- [ ] **Step 2: Write failing UTF-8 tests**

On Unix, construct an absolute non-UTF-8 root with `OsStringExt::from_vec` and require rejection from `ResolvedExportPolicy::new`. Also construct the same policy through a prepared request and require failure before staging exists:

```rust
#[test]
fn prepared_policy_rejects_non_utf8_root_before_staging() {
    let root = absolute_non_utf8_path();
    let error = ResolvedExportPolicy::new(root, ResolvedLayout::FlatDetailed, activity()).unwrap_err();
    assert!(error.to_string().contains("UTF-8"));
    assert!(!staging_root.exists());
}
```

- [ ] **Step 3: Run the RED tests**

Run:

```bash
nix develop -c cargo test --test memory_plan publication_scratch_is_startup_accounted_materialized_and_limit_checked -- --exact
nix develop -c cargo test --test export_policy non_utf8 -- --nocapture
nix develop -c cargo test --test persistence_workspace startup_addresses_and_operation_allocations_are_stable_across_sparse_maximum_outputs -- --exact
```

Expected: compilation fails because publication scratch fields/getters/constants are absent, and the policy currently accepts a programmatic non-UTF-8 root.

- [ ] **Step 4: Add exact memory-plan accounting**

Define conservative slot-size constants with compile-time size assertions and add a named `publication_scratch` component:

```rust
pub const PUBLICATION_SYNC_SLOT_BYTES: u64 = 16;
pub const PUBLICATION_ARTIFACT_SLOT_BYTES: u64 = 32;

let component_bytes = checked_add(
    "publication component buffer overflow",
    inputs.maximum_path_bytes,
    1,
)?;
let sync_payload = checked_mul(
    "publication sync slot storage overflow",
    manifest_directory_slots,
    PUBLICATION_SYNC_SLOT_BYTES,
)?;
let publication_scratch = checked_add(
    "publication scratch overflow",
    checked_add(
        "publication scratch overflow",
        allocation_budget_bytes(sync_payload)?,
        checked_mul(
            "publication component buffer storage overflow",
            2,
            allocation_budget_bytes(component_bytes)?,
        )?,
    )?,
    allocation_budget_bytes(PUBLICATION_ARTIFACT_SLOT_BYTES)?,
)?;
```

Store the exact value on `SessionMemoryPlan`, expose `publication_scratch_bytes()`, include it in `committed_bytes`, component reports, and geometry validation.

- [ ] **Step 5: Materialize publication scratch at workspace startup**

Add fixed-size metadata types and allocations:

```rust
#[derive(Clone, Copy, Default)]
pub(crate) struct DirectorySyncSlot {
    pub entry_index: u32,
    pub prefix_len: u32,
    pub active: bool,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct CurrentArtifactSlot {
    pub path: PathRef,
    pub identity: Option<FileIdentity>,
    pub final_name: bool,
}

pub(crate) struct PublicationScratch {
    sync_slots: ExactArray<DirectorySyncSlot>,
    current_artifact: ExactArray<CurrentArtifactSlot>,
    component_a: MaterializedBuffer<u8>,
    component_b: MaterializedBuffer<u8>,
    sync_count: usize,
}
```

Allocate exact plan capacities in `PersistenceWorkspace::allocate`, expose addresses in `WorkspaceAllocationAddresses`, clear contents without reallocating in `reset_slots`, and assign fixed path indexes 3 and 4 to `PARTIAL_SCRATCH_PATH` and `MANIFEST_SCRATCH_PATH`.

- [ ] **Step 6: Reject non-UTF-8 canonical paths before staging**

In `ResolvedExportPolicy::new`, reject `output_dir.to_str().is_none()` with a stable validation message. In canonical runtime rendering, validate `final_path.to_str()` before filesystem preflight; do not defer the check to manifest construction.

- [ ] **Step 7: Run Task 1 GREEN and regressions**

Run:

```bash
nix develop -c cargo test --test memory_plan --test export_policy --test app_config --test persistence_workspace
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Expected: all pass; no publication behavior changes yet.

- [ ] **Step 8: Independent review and parent commit**

Review exact formulas, concrete allocation/address correspondence, overflow handling, and pre-staging error order. Parent commits only reviewed files with:

```bash
git commit -m "fix: preallocate publication scratch"
```

---

### Task 2: Vector-free prepared file-set and atomic publishers

**Files:**
- Modify: `src/persistence_workspace.rs`
- Modify: `src/export_wav.rs`
- Modify: `src/recovery.rs`
- Modify: `src/dump.rs` only for the temporary completed-output adapter needed to keep commits buildable
- Test: `tests/persistence_workspace.rs`
- Test: `tests/export_wav.rs`
- Test: `tests/recovery.rs`
- Test: `tests/dump_coordinator.rs`

**Interfaces:**
- Consumes: Task 1 `PublicationScratch` and fixed path slots.
- Produces: slice-backed `FilePlan`, `PublicationViews`, marker-only `PreparedPublication::Published`, a fixed prepared `IndeterminatePublication` descriptor, vector-free prepared publishers, and a clearly named temporary `collect_completed_output_for_legacy_test_adapter` removed in Task 3.

- [ ] **Step 1: Write RED publication-allocation tests**

Add helpers which prepare equal-length path names for one output and the startup maximum (for example 64 outputs), then measure only prepared publication. Run once for `FileSet` and once for `AtomicDirectory`:

```rust
let (small_result, small_count, small_bytes) = allocation_count_during(|| {
    publish_prepared(small_prepared)
});
let (maximum_result, maximum_count, maximum_bytes) = allocation_count_during(|| {
    publish_prepared(maximum_prepared)
});
assert!(matches!(small_result, PreparedPublication::Published));
assert!(matches!(maximum_result, PreparedPublication::Published));
assert_eq!((maximum_count, maximum_bytes), (small_count, small_bytes));
```

Add checkpoint variants for one error immediately after partial creation, one after final rename, and one atomic post-rename error. Require no output-count growth and preserve the existing retry/indeterminate classification.

Add the fixed-descriptor structural RED in this task, before replacing the old state:

```rust
#[test]
fn prepared_indeterminate_descriptor_has_fixed_metadata_only() {
    assert!(std::mem::size_of::<IndeterminatePublication>() <= 32);
}
```

The old vector/durable-output representation must exceed the bound; the Task 2 descriptor must fit it and contain only workspace id, checked transaction generation, and strategy kind.

- [ ] **Step 2: Run RED**

Run:

```bash
nix develop -c cargo test --test persistence_workspace prepared_publication_allocations -- --nocapture
nix develop -c cargo test --test persistence_workspace prepared_indeterminate_descriptor_has_fixed_metadata_only -- --exact
nix develop -c cargo test --test dump_coordinator every_recall_checkpoint_rolls_back_or_completes_without_duplicate_publication -- --exact
nix develop -c cargo test --test dump_coordinator every_dump_checkpoint_rolls_back_or_completes_without_duplicate_directory -- --exact
```

Expected: allocation comparison fails because `planned`, rollback, parent, final-file, and output vectors grow with output count.

- [ ] **Step 3: Split the canonical file-plan borrow**

Refactor `FilePlan` to borrow only output/path slices:

```rust
#[derive(Clone, Copy)]
pub struct FilePlan<'a> {
    outputs: &'a [OutputFileSlot],
    paths: &'a [ReusablePath],
    len: usize,
}
```

Add `PublicationViews<'a>` containing `files`, `ManifestScratch`, and mutable `PublicationScratch`. Implement one `OwnedTransactionArtifacts::publication_views()` which splits disjoint workspace fields; no unsafe aliasing and no complete-workspace borrow in `FilePlan`.

- [ ] **Step 4: Replace per-call C strings with reusable buffers**

Add component-buffer methods which reject NUL/capacity overflow, append one terminal NUL, and expose `&CStr`. Provide a two-buffer method for `renameat2`. Prepared descriptor helpers accept these buffers; legacy snapshot helpers retain their existing allocating `CString` path.

```rust
fn prepare_component<'a>(buffer: &'a mut [u8], name: &OsStr) -> Result<&'a CStr>;
fn prepare_component_pair<'a>(
    first: &'a mut [u8],
    second: &'a mut [u8],
    old: &OsStr,
    new: &OsStr,
) -> Result<(&'a CStr, &'a CStr)>;
```

- [ ] **Step 5: Rewrite file-set publication around manifest authority**

Delete prepared-path `planned`, `partials`, `created_finals`, `created_final_files`, and `parents` vectors, plus `RecallPublishError::{created_finals, partials}`. Build manifest entries directly from `FilePlan`. Build each adjacent partial in `PARTIAL_SCRATCH_PATH` without `parent.join(format!(...))`; track only the one current unjournaled identity in `CurrentArtifactSlot`; record each identity durably before clearing it. Reopen/sync finals one at a time.

Populate fixed `DirectorySyncSlot`s with `(entry_index, prefix_len)` using bounded linear duplicate detection during parent traversal, then resolve and sync each coordinate. Keep transaction-created rollback intents in `ManifestDirectorySlot`; do not mix the two states.

Prepared trusted-root traversal borrows the configured output path and stores the anchor as a UTF-8 prefix coordinate. It uses reusable path/component scratch inside every output/component loop; remove the current `anchor_path`, `output_relative`, `traversal_path`, `current`, and `output_path` `PathBuf` copies from that path. Fixed-count error values may allocate only when an error is actually returned.

- [ ] **Step 6: Rewrite atomic publication around the borrowed plan**

Delete prepared-path `planned`, `final_files`, all durable `PublishedOutput` clones, and `Box<(PathBuf, PathBuf, PublishedOutput)>`. Build the atomic manifest path in `MANIFEST_SCRATCH_PATH`, build the manifest directly, sync staged files one at a time, and retain the current rename/parent-sync/complete checkpoints. Return `PreparedPublication::Published` without an owned output.

Replace prepared `IndeterminatePublication.artifacts: Vec<PublicationArtifact>` and owned durable output/path state with Task 3's fixed descriptor shape now (`workspace_id`, checked transaction generation, and kind). Task 2 may keep the old recovery method signature temporarily, but its prepared state and every error/checkpoint value must contain no `Vec`, `Box`, `PublishedOutput`, or output-path `PathBuf`.

- [ ] **Step 7: Keep the intermediate commit buildable without hiding the defect**

Add a temporary `pub(crate)` adapter on `PersistenceWorkspace` which collects the completed borrowed plan only when old coordinator tests request an owned outcome. Name it `collect_completed_output_for_legacy_test_adapter`, document that Task 3 removes it, and ensure the new allocation tests call the publisher directly and cannot pass through the adapter.

- [ ] **Step 8: Run Task 2 GREEN and checkpoint matrices**

Run:

```bash
nix develop -c cargo test --test persistence_workspace prepared_publication_allocations -- --nocapture
nix develop -c cargo test --test persistence_workspace prepared_indeterminate_descriptor_has_fixed_metadata_only -- --exact
nix develop -c cargo test --test export_wav --test recovery --test dump_coordinator
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Expected: publisher allocation count/bytes do not grow from one to maximum outputs; all crash/recovery tests pass.

- [ ] **Step 9: Independent review and parent commit**

Review mutation ordering, one-current-artifact proof, parent coordinate capacity/dedup, symlink trust boundaries, and absence of prepared-path output collections. Parent commits:

```bash
git commit -m "fix: publish from preallocated file plans"
```

---

### Task 3: Compact recovery, borrowed committed outcomes, and exact-once finalization

**Files:**
- Modify: `src/persistence_workspace.rs`
- Modify: `src/export_wav.rs`
- Modify: `src/dump.rs`
- Test: `tests/recovery.rs`
- Test: `tests/dump_coordinator.rs`
- Test: `tests/persistence_workspace.rs`

**Interfaces:**
- Consumes: Task 2 marker-only publisher and retained canonical plan.
- Produces: recovery behavior for Task 2's fixed `IndeterminatePublication`, `CompletedOutput<'a>`, `CommittedPersistenceRef<'a>`, `persist_policy_with_delivery`, and `CompletionGuard`; removes the temporary owned adapter from Task 2.

- [ ] **Step 1: Write RED compact-recovery behavior tests**

Inject a durable file-set and atomic failure using Task 2's fixed descriptor, recover `Complete`, and require an ordered borrowed path iterator equal to the original prepared plan while workspace allocation addresses remain unchanged. Recover `RolledBack` and require the same frozen decision/range to retry.

- [ ] **Step 2: Write RED delivery/finalization tests**

Add coordinator tests with a caller-owned delivery hook:

```rust
let delivered = coordinator.persist_policy_with_delivery(
    &arena,
    &mut workspace,
    request,
    DEADLINE,
    DEADLINE,
    |outcome| {
        assert_written_paths(outcome, expected.as_slice());
        Ok(())
    },
)?;
```

Add hooks which return an error and panic under `catch_unwind`. In both cases require the cursor committed, workspace reusable, frozen epoch released or recorded once as pending, and the next command never republishes the prior files.

- [ ] **Step 3: Run RED**

Run:

```bash
nix develop -c cargo test --test dump_coordinator borrowed_delivery -- --nocapture
nix develop -c cargo test --test recovery compact_indeterminate -- --nocapture
```

Expected: borrowed completion/recovery APIs are absent, and current recovery clears or returns owned output instead of retaining the canonical workspace plan for delivery.

- [ ] **Step 4: Integrate the fixed descriptor with workspace recovery**

Use the Task 2 descriptor and workspace identity fields exactly as established:

```rust
pub struct IndeterminatePublication {
    workspace_id: u64,
    transaction_generation: u64,
    kind: TransactionKind,
}
```

Recovery rejects a descriptor mismatch, resolves the one current artifact, and runs manifest recovery from fixed workspace paths. `Complete` leaves output/path slots intact and marks completed cleanup; `RolledBack` clears slots; `Pending` preserves everything. Startup scan behavior remains diagnostic-only.

- [ ] **Step 5: Add borrowed completed outcome types**

Define:

```rust
pub struct CompletedOutput<'a> {
    pub output_directory: &'a Path,
    pub files: FilePlan<'a>,
}

pub enum CommittedPersistenceRef<'a> {
    Written {
        range: FrameRange,
        export_range: FrameRange,
        frames: u64,
        losses: LossBreakdown,
        output: CompletedOutput<'a>,
    },
    SkippedSilent { range: FrameRange, frames: u64, losses: LossBreakdown },
    SkippedByPolicy { range: FrameRange, frames: u64, losses: LossBreakdown },
    NoNewAudio { losses: LossBreakdown },
}
```

Expose final-path iteration without cloning and remove the temporary Task 2 collector.

- [ ] **Step 6: Implement coordinator commit-before-delivery**

Add the nonescaping API:

```rust
pub fn persist_policy_with_delivery<F>(
    &self,
    arena: &CaptureArena,
    workspace: &mut PersistenceWorkspace,
    request: PolicyPersistenceRequest<'_>,
    timeout: Duration,
    release_timeout: Duration,
    deliver: F,
) -> Result<()>
where
    F: for<'view> FnOnce(CommittedPersistenceRef<'view>) -> Result<()>;
```

Preserve selection/publication under the coordinator state lock, commit cursor/loss state, move the frozen transaction into `CompletionGuard`, then release the state lock before invoking delivery while the caller still holds `&mut PersistenceWorkspace`.

`CompletionGuard::deliver` takes the higher-ranked closure above, creates `CommittedPersistenceRef` from an immutable reborrow of the guard-owned workspace, invokes the closure, and ends that borrow before finalization. The view cannot be returned or stored because the closure result owns no view lifetime. `CompletionGuard::finalize` then runs completed-publication cleanup/reset and bounded frozen release exactly once. `Drop` invokes the same idempotent finalizer on early return/unwind and stores a failed release in existing coordinator pending-release state. A delivery error is returned only after finalization and never changes persistence classification.

- [ ] **Step 7: Convert prepared tests to borrowed assertions**

Keep legacy snapshot tests on owned `DumpOutcome`; convert every prepared-policy coordinator test to assert through `CommittedPersistenceRef`. Test-only helpers may collect after entering the hook, but allocation proofs use a noncollecting sink.

- [ ] **Step 8: Run Task 3 GREEN and full coordinator/recovery suites**

Run:

```bash
nix develop -c cargo test --test dump_coordinator --test recovery --test persistence_workspace
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Expected: all pass; no production prepared API returns an owned output list.

- [ ] **Step 9: Independent review and parent commit**

Review state/workspace lock ordering, visitor lifetime, guard unwind behavior, generation validation, exact retry, and cleanup/release idempotence. Parent commits:

```bash
git commit -m "fix: borrow committed persistence outcomes"
```

---

### Task 4: Borrowed daemon JSON delivery

**Files:**
- Modify: `src/control.rs`
- Modify: `src/daemon.rs`
- Test: `src/control.rs`
- Test: `tests/daemon_fake.rs`
- Test: `tests/daemon_idle.rs`
- Test: `tests/dump_coordinator.rs`

**Interfaces:**
- Consumes: Task 3 `CommittedPersistenceRef` and delivery visitor.
- Produces: `write_persistence_response`, borrowed serde wrappers over `FilePlan`, and persistence-aware operation-worker handlers which never create owned written responses.

- [ ] **Step 1: Write RED semantic JSON compatibility tests**

For written, skipped, policy-skipped, and no-new-audio outcomes, serialize the borrowed response to a byte sink and deserialize through existing `ControlResponse`. Compare every field to the existing owned fixture, including old default handling and ordered retained paths.

- [ ] **Step 2: Write RED end-to-end daemon allocation and delivery-error tests**

Use equal-length output paths and compare one versus maximum output counts across `prepare -> publish -> coordinator commit -> borrowed serde_json::to_writer(io::sink())` for FileSet and AtomicDirectory. Require equal allocation count/bytes. Add a deterministic writer which fails after the first file path; require committed cursor, cleanup/release, no duplicate output, and a transport diagnostic.

- [ ] **Step 3: Run RED**

Run:

```bash
nix develop -c cargo test --lib borrowed_persistence_json -- --nocapture
nix develop -c cargo test --test daemon_fake prepared_response_allocations -- --nocapture
nix develop -c cargo test --test dump_coordinator response_write_failure_commits_once -- --exact
```

Expected: borrowed writer APIs are absent and current owned response vectors/String scale with outputs.

- [ ] **Step 4: Add borrowed response serialization**

Implement `Serialize` wrappers whose file sequence iterates borrowed final paths and serializes validated `&str` values. Add:

```rust
pub fn write_persistence_response<W: Write>(
    writer: &mut W,
    ok: bool,
    message: &str,
    status: &DaemonStatus,
    sample_rate: u32,
    outcome: CommittedPersistenceRef<'_>,
) -> Result<()>;
```

Write with `serde_json::to_writer`, append one newline, and never create an intermediate body `String` or owned path. Preserve the current response schema and duration/range/loss validation.

- [ ] **Step 5: Route daemon persistence directly to the visitor**

In both legacy and app operation-worker closures, route `Recall`/`Dump` to a stream-owning persistence handler. That handler invokes `CaptureSession::persist_with_delivery` and writes the borrowed response inside the visitor. All other commands continue through `handle_request`/`handle_idle_request` plus `write_response`.

Construct response status only after coordinator state commit and lock release. On delivery failure, record/set the control error without attempting a second response and without changing the committed persistence outcome.

- [ ] **Step 6: Prove current daemon never calls allocating compatibility publishers**

Add compile-time/module-private routing tests which exercise both legacy and app recall/dump and assert the prepared publisher hook fires. Keep `export_snapshot_wav`, `publish_recall`, and `publish_dump` unreferenced from `daemon.rs` and `CaptureSession`.

- [ ] **Step 7: Run Task 4 GREEN and daemon regressions**

Run:

```bash
nix develop -c cargo test --lib control::tests
nix develop -c cargo test --lib daemon::tests
nix develop -c cargo test --test daemon_fake --test daemon_idle --test dump_coordinator --test threshold_cli
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Expected: semantic JSON and live socket behavior remain compatible; end-to-end daemon allocation does not grow with outputs.

- [ ] **Step 8: Independent review and parent commit**

Review response schema, post-commit transport classification, worker/lane behavior, status responsiveness, and absence of owned written response values. Parent commits:

```bash
git commit -m "fix: stream committed persistence responses"
```

---

### Task 5: Streaming persistence CLI consumption

**Files:**
- Modify: `src/control.rs`
- Modify: `src/main.rs` only if command dispatch needs a dedicated client entry point
- Test: `src/control.rs`
- Test: `tests/daemon_fake.rs`

**Interfaces:**
- Consumes: unchanged written-response JSON from Task 4.
- Produces: `send_persistence_request_streaming` and `PersistenceClientResponseSeed`; general `send_request` remains owned and compatible.

- [ ] **Step 1: Write RED streaming-client tests**

Feed one-file and maximum-file responses with equal maximum path length through the streaming client visitor and a fixed test writer. Require identical message text to `format_persistence_outcome`, ordered paths, equal allocation count/bytes after parser initialization, and no retained output vector. Include escaped JSON characters and malformed/oversized path rejection.

- [ ] **Step 2: Run RED**

Run:

```bash
nix develop -c cargo test --lib streaming_persistence_client -- --nocapture
nix develop -c cargo test --test daemon_fake fake_daemon_dump_exports_files_with_iso8601_timestamp_and_channel_names -- --exact
```

Expected: streaming seed/client entry point is absent; current `send_request` builds a response `String` and `Vec<PathBuf>`.

- [ ] **Step 3: Implement the reusable streaming deserializer**

Create `PersistenceClientResponseSeed<'a, W>` with fixed scalar state and one reusable unescape/path buffer capped at the control maximum. Implement manual serde map/sequence visitors. Require scalar written fields and `output_directory` before `files` for the specialized daemon/CLI stream, then format each `&str` directly to `W` as the sequence arrives. Reuse the same buffer for every escaped element; never construct a per-file `String` or `PathBuf`.

Retain existing `PersistenceOutcomeResponse` deserialization and `send_request` unchanged for public compatibility.

- [ ] **Step 4: Route only CLI recall/dump through streaming decode**

Change `client_recall` and `client_dump` to call `send_persistence_request_streaming`; status, stop, config, threshold, and external API tests continue using `send_request`. Preserve success/error exit behavior and exact user-visible formatting.

- [ ] **Step 5: Run Task 5 GREEN and protocol regressions**

Run:

```bash
nix develop -c cargo test --lib control::tests
nix develop -c cargo test --test daemon_fake --test threshold_cli
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo fmt --check
```

Expected: all pass; persistence CLI peak/live allocations do not grow with file count and owned protocol compatibility remains intact.

- [ ] **Step 6: Independent review and parent commit**

Review malformed-wire handling, buffer reuse, JSON escaping, field-order contract for the specialized path, and unchanged public deserialization. Parent commits:

```bash
git commit -m "fix: stream persistence client output"
```

---

### Task 6: Complete regression, documentation consistency, and closure review

**Files:**
- Modify: `README.md` only if implementation changes an already documented internal ownership statement
- Modify: task-touched tests/docs only for verified corrections

**Interfaces:**
- Consumes: all preceding correction interfaces.
- Produces: final verified implementation and review evidence; no new runtime interface.

- [ ] **Step 1: Run focused correction suites**

Run:

```bash
nix develop -c cargo test --test memory_plan \
  --test app_config \
  --test export_policy \
  --test persistence_workspace \
  --test export_wav \
  --test recovery \
  --test dump_coordinator \
  --test daemon_fake \
  --test daemon_idle \
  --test threshold_cli
```

Expected: all focused tests pass, including allocation and delivery-failure proofs.

- [ ] **Step 2: Run required Rust gates**

Run:

```bash
nix develop -c cargo fmt --check
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo test --all-targets
git diff --check
```

Expected: every command exits 0 with no warning/failure.

- [ ] **Step 3: Run Nix checks and dirty build**

Run:

```bash
nix flake check --print-build-logs
nix build "path:.#lamb" --no-link --print-build-logs
```

Expected: declared checks and dirty package build pass.

- [ ] **Step 4: Inspect scope and artifacts**

Run:

```bash
git status --short
git diff --stat
git diff --check
```

Expected: only reviewed source/tests/docs changes; no WAV, manifest, calibration, staging, state, `result`, or temporary review artifact is tracked.

- [ ] **Step 5: Independent final correction review**

Review the full correction range against both binding specs. Require an explicit matrix covering startup accounting, no output-count allocations, file-set/atomic ordering, fixed recovery ownership, guard finalization, cursor/retry semantics, borrowed JSON, streaming CLI, realtime isolation, and all accepted original-plan deferrals recorded during original Task 11.

- [ ] **Step 6: Parent final commit if review fixes remain**

If the review required changes, rerun the affected focused tests and commit the reviewed fixes with a scoped message. Otherwise retain the five task commits as the complete correction.

- [ ] **Step 7: Repeat whole-plan review**

Package the complete original feature range from planning base `af0696b` through correction HEAD with the original Tasks 1–11 reports, correction Tasks 1–6 reports, and correction gate evidence. Final verdict must be `APPROVED` with no Critical/Important findings before completion is claimed.

## Plan self-review

- **Spec coverage:** Task 1 covers startup accounting and pre-staging UTF-8; Task 2 covers all prepared publisher collections/syscall buffers/parent sync and establishes fixed recovery state; Task 3 integrates that state with borrowed output, commit ordering, and unconditional finalization; Task 4 covers daemon response ownership; Task 5 covers CLI streaming; Task 6 covers every Rust/Nix/review gate and creates a sixth commit only when review fixes are required.
- **Placeholder scan:** Every task names exact files, interfaces, RED/GREEN commands, failure signal, implementation boundary, and commit message. No deferred implementation placeholder remains.
- **Type consistency:** `PublicationScratch` feeds `PublicationViews`; marker-only `PreparedPublication` feeds `persist_policy_with_delivery`; `CompletedOutput` feeds `CommittedPersistenceRef`; the same borrowed outcome feeds `write_persistence_response`; CLI consumes the unchanged wire independently.
- **Safety:** Visible mutation still starts only after a durable prepared manifest; one fixed unjournaled artifact is sufficient for serial publication; transport failure is post-commit; guard finalization cannot reclassify output; legacy allocating APIs remain outside current daemon routing.
