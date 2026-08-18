# Preallocated Persistence Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace pinned, recording-size persistence snapshots with a startup-reserved dual-ring runtime, keep control responsive during export, recover manifest-backed crash artifacts, and report every loss cause exactly once.

**Architecture:** Capture callbacks copy into a fixed page-touched SPSC ingress, and one prestarted capture worker exclusively owns two fully materialized retention rings. Persistence asks that worker to switch to standby between audio blocks and treats the former active ring as an immutable frozen epoch, then scans and encodes it with fixed reusable buffers. The existing control path hands mutating operations to one prestarted bounded operation lane; manifest-backed publication and cause-specific loss state complete the transaction before the shared cursor advances.

**Tech Stack:** Rust 2021, existing PipeWire/JACK/fake backends, atomics and fixed `std::sync` workers, Unix `renameat2`, `fsync`, serde JSON manifests, Cargo integration tests, Nix.

## Global Constraints

- `recall` and `dump` share exactly one monotonic `committed_until` cursor per capture session.
- Every absolute source frame admitted by capture is eventually accounted for exactly once as published, intentionally silent/cleared, retention-lost, or capture-dropped, without filesystem activity blocking the realtime producer.
- Recall followed immediately by dump, or dump followed immediately by recall, returns `NoNewAudio` when capture added no frames.
- The transaction mutex spans selection, stabilization, preparation, publication, and cursor commit; capture never acquires it.
- Capture callbacks only enqueue into fixed ingress slots; one capture worker writes the active epoch. The frozen epoch is immutable and independently owns the selected samples before filesystem work.
- No capture/ring lock is held during silence scanning, WAV encoding, copying, synchronization, or rename.
- All RAM proportional to retention, channels, chunk geometry, capture ingress, persistence workspace, both bounded command queues, and both worker stacks is calculated, allocated, and page-touched before backend activation.
- Persistence performs no heap allocation proportional to selected recording length.
- Recall keeps `/tmp/LAMB/staging/<unique-id>/`, existing detailed flat filenames, and configured `outputDir` layout.
- Recall copies closed staged WAVs to unique adjacent hidden partials, flushes/syncs, and publishes without overwrite.
- Flat multichannel publication is all-or-nothing for cursor commit; failure rolls back transaction-owned artifacts best-effort and keeps the range retryable.
- Dump keeps simple per-channel WAVs inside one atomically published timestamp directory.
- Exact-zero silence may create transaction-private temporary WAVs during scanning, but creates no final output and advances the cursor.
- `clear` waits for an active transaction; `stop`/`stop-capture` reject new persistence, wait for the active transaction, then stop.
- Only versioned manifest-backed artifacts are automatically recovered; unmarked legacy artifacts are untouched.
- Outcomes report `retention_lost_frames`, `cleared_frames`, and `capture_dropped_frames`; compatibility `lost_frames` is their saturating total.
- Successful `NoNewAudio` acknowledges pending loss so each loss is reported once; failures acknowledge neither cursor nor losses.
- Do not change recall naming, dump layout, NixOS options, sample format, or exact-zero policy.
- Do not commit, push, or stage unless the user explicitly requests it.

---

## File Structure

- Create `src/memory_plan.rs`: checked session-memory accounting, reusable page-materialized buffers, and Linux residency tests.
- Create `src/capture_arena.rs`: dual ring epochs, callback-safe active selection, absolute bases, freeze/retry/release lifecycle.
- Create `src/persistence_workspace.rs`: fixed scratch/path/file slots and streaming WAV preparation over a frozen epoch.
- Create `src/recovery.rs`: versioned transaction manifests, identity validation, startup recovery, and directory synchronization.
- Create `src/control_server.rs`: one fixed persistence worker, bounded operation queue, stream handoff, and graceful drain around the existing control path.
- Modify `src/sample_ring.rs`: reset/reuse, fixed-buffer range copying, capacity/materialization APIs, and clear-boundary metadata.
- Modify `src/dump.rs`: frozen transaction coordinator, pending retry, loss breakdown, and clear acknowledgement.
- Modify `src/export_wav.rs`: reuse streaming workspace publication and manifest hooks while preserving names/layout.
- Modify `src/capture_fake.rs`, `src/capture_jack.rs`, `src/capture_pipewire.rs`: write through `CaptureArena` rather than directly to one ring.
- Modify `src/daemon.rs`: construct `CaptureSession`, route lifecycle through operation lane, and run recovery.
- Modify `src/control.rs`: cause-specific structured loss fields and successful loss-bearing `NoNewAudio`.
- Modify `src/math.rs`, `src/config.rs`, `src/app_config.rs`, `src/profile.rs`: memory-plan inputs and validation messages without adding NixOS options.
- Modify `src/lib.rs`: export focused modules.
- Add/modify integration tests under `tests/` as named below.

---

### Task 1: Checked startup memory plan and committed buffers

**Files:**
- Create: `src/memory_plan.rs`
- Modify: `src/math.rs`
- Modify: `src/sample_ring.rs`
- Modify: `src/lib.rs`
- Create: `tests/memory_plan.rs`

**Interfaces:**
- Produces `SessionMemoryInputs`, `SessionMemoryPlan`, and `MemoryComponent`.
- Produces `MaterializedBuffer<T>::new_zeroed(len) -> Result<Self>` and reusable slice access.
- Produces `SampleRing::materialize_pages()` and exact `allocated_sample_bytes()`/`allocated_metadata_bytes()`.

- [ ] **Step 1: Write RED tests for complete accounting**

```rust
#[test]
fn plan_includes_two_rings_workspace_queues_stacks_and_headroom() {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 1_800 * 48_000,
        channels: 2,
        chunk_frames: 12_000,
        sample_bytes: 4,
        split_when_over_bytes: 1_073_741_824,
        capture_queue_slots: 8,
        capture_slot_frames: 12_000,
        capture_worker_stack_bytes: 524_288,
        control_queue_capacity: 16,
        worker_stack_bytes: 524_288,
        io_buffer_bytes_per_channel: 65_536,
        maximum_path_bytes: 4_096,
        headroom: 1.2,
    }).unwrap();
    assert_eq!(plan.ring_count(), 2);
    assert!(plan.component("ring_samples").unwrap().bytes > 0);
    assert!(plan.component("persistence_workspace").unwrap().bytes > 0);
    assert!(plan.required_with_headroom() >= plan.committed_bytes());
}
```

Add overflow tests, exact headroom-boundary tests above `2^53`, proportional
file/writer/manifest slot accounting tests, and a production-API limit test
asserting rejection occurs before an allocation callback is invoked.

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test memory_plan`

Expected: compilation fails because `memory_plan` does not exist.

- [ ] **Step 3: Implement checked component calculations**

Use `checked_mul`, `checked_add`, and finite-positive headroom validation. Include
the page-rounded capture-ingress sample slots, slot metadata, and capture-worker
stack as explicit components. Convert
the binary `f64` headroom to an upward-rounded fixed-point ratio, then perform
the final ceil multiplication with checked `u128` arithmetic so large byte
counts are never rounded down. Calculate maximum WAV parts arithmetically
without allocating a parts vector. Include explicit proportional budgets for
file/writer slots, path slot metadata, manifest entries, one operation-worker
stack, and the bounded operation queue. Preserve a component report suitable
for startup errors.

```rust
pub struct SessionMemoryPlan {
    components: Vec<MemoryComponent>,
    committed_bytes: u64,
    required_with_headroom: u64,
}

impl SessionMemoryPlan {
    pub fn validate_max(&self, maximum: Option<u64>) -> Result<()> {
        if maximum.is_some_and(|limit| self.required_with_headroom > limit) {
            return Err(LambError::Validation(self.describe_limit_failure(limit)));
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Implement page-materialized reusable buffers**

Allocate one exact owned layout through a null-checking fallible raw allocation,
initialize every supported element, then use volatile writes of initialized
elements at each OS-page interval and at the final element. Restrict the arena
to explicitly supported materializable types such as `u8` and `f32`; do not
read arbitrary generic padding. On Linux tests, use `mincore` to verify every
page is resident immediately after construction. Do not use `mlock`.

```rust
pub struct MaterializedBuffer<T: Materializable> {
    pointer: NonNull<T>,
    length: usize,
    layout: Layout,
}
```

- [ ] **Step 5: Materialize existing ring pages explicitly**

Store ring samples in exact-layout materialized arenas so allocator-returned
`Vec` capacity cannot exceed the validated plan. Account metadata with explicit
conservative page-rounded per-allocation reserves rather than assumptions about
the standard library's private `Arc` header layout. Count chunk-index capacity
separately from actual `chunks.len()` object allocations. Ring reset introduced
in Task 2 must reuse these allocations.

- [ ] **Step 6: Run focused and regression checks**

Run: `nix develop -c cargo test --test memory_plan --test config_validation --test ring_snapshot && nix develop -c cargo clippy --all-targets -- -D warnings`

Expected: all selected tests pass and Clippy emits no warnings.

---

### Task 2: Dual-epoch capture arena

**Files:**
- Create: `src/capture_arena.rs`
- Modify: `src/sample_ring.rs`
- Modify: `src/lib.rs`
- Create: `tests/capture_arena.rs`

**Interfaces:**
- Consumes `SessionMemoryPlan` and materialized rings from Task 1.
- Produces allocation-free `CaptureIngress::try_push_interleaved`, one prestarted `CaptureWorker`, and `CaptureArena::{freeze_since,clear_active,release_frozen}` command methods plus cumulative status.
- Produces `FrozenCaptureEpoch` with absolute/local ranges and fixed-buffer reads.

- [ ] **Step 1: Write RED tests for epoch switching and absolute ranges**

```rust
#[test]
fn freeze_switches_capture_without_overlap_or_gap() {
    let arena = arena(8);
    arena.write_interleaved(&mono(0..5), 1).unwrap();
    let frozen = arena.freeze_since(None).unwrap().unwrap();
    arena.write_interleaved(&mono(5..9), 1).unwrap();
    assert_eq!(frozen.absolute_range(), 0..5);
    assert_eq!(arena.active_absolute_range(), 5..9);
    assert_eq!(frozen.read_all_for_test(), mono(0..5));
}
```

Add a producer/consumer barrier test proving every successfully enqueued frame is written to exactly one epoch across the switch, queue-full drop accounting, a failed-frozen retention test, and repeated alternating epochs.

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test capture_arena`

Expected: compilation fails because `CaptureArena` does not exist.

- [ ] **Step 3: Add allocation-reusing ring reset and fixed-buffer copy**

The resulting public signatures are:

```rust
pub fn reset(&self) -> Result<()>;
pub fn copy_interleaved_range_into(
    &self,
    range: Range<u64>,
    destination: &mut [f32],
) -> Result<u64>;
```

`reset` is legal only for the standby epoch and only on the capture worker. Tests must prove chunk allocation addresses remain unchanged.

- [ ] **Step 4: Implement fixed ingress and the sole-owner capture worker**

Preallocate and page-touch a bounded SPSC queue of fixed-size interleaved sample
slots. Producer callbacks split input across available slots and account any
unqueued whole frames as capture-dropped. The capture worker drains slots in
order, performs checked absolute-frame advancement, and alone mutates ring
epochs. A single preallocated command/result slot lets persistence request
freeze, clear, release, status, and shutdown between audio blocks.

```rust
pub fn try_push_interleaved(&self, samples: &[f32], channels: u32) -> PushResult;
```

- [ ] **Step 5: Implement frozen range streaming**

Translate absolute cursor boundaries into local ranges and copy at most the caller's fixed scratch capacity. Reject mutable epoch access. `FrozenCaptureEpoch` owns the old epoch identity until coordinator release.

- [ ] **Step 6: Verify capture continuity**

Run: `nix develop -c cargo test --test capture_arena --test ring_snapshot`

Expected: every switch/range/reset test passes with zero export-induced drops.

---

### Task 3: Fixed persistence workspace and streaming temporary WAVs

**Files:**
- Create: `src/persistence_workspace.rs`
- Modify: `src/export_wav.rs`
- Modify: `src/lib.rs`
- Modify: `tests/export_wav.rs`
- Create: `tests/persistence_workspace.rs`

**Interfaces:**
- Consumes `FrozenCaptureEpoch`, `SessionMemoryPlan`, and existing publication naming rules.
- Produces `PersistenceWorkspace::prepare(&FrozenCaptureEpoch, PrepareRequest) -> Result<PreparedPersistence>`.
- Produces `PreparedPersistence::{Silent, Recall, Dump}` and fixed reusable writer/path slots.

- [ ] **Step 1: Write RED tests for bounded streaming**

Add tests that stream multiple chunks and split parts, compare WAV samples/headers with existing output, classify exact-zero and `-0.0`, and assert the workspace buffer addresses/capacities are unchanged before and after maximum-range preparation.

```rust
#[test]
fn maximum_range_reuses_the_startup_workspace() {
    let (arena, mut workspace) = fixture_with_materialized_workspace();
    let addresses = workspace.allocation_addresses();
    let frozen = freeze_full_epoch(&arena);
    workspace.prepare(&frozen, recall_request()).unwrap();
    assert_eq!(workspace.allocation_addresses(), addresses);
}
```

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test persistence_workspace`

Expected: compilation fails because `PersistenceWorkspace` does not exist.

- [ ] **Step 3: Implement reusable WAV writers**

Replace recording-sized channel vectors and dynamically allocated `BufWriter` storage with fixed per-channel byte slices from the workspace. Open files per operation, but reuse all PCM buffers, path slots, split slots, and manifest serialization storage.

```rust
pub enum PreparedPersistence {
    Silent { staging: OwnedTransactionArtifacts },
    Recall { staging: OwnedTransactionArtifacts, files: FilePlan },
    Dump { staging: OwnedTransactionArtifacts, files: FilePlan },
}
```

- [ ] **Step 4: Stream and classify in one pass**

Read one block from the frozen epoch into fixed interleaved scratch, update `any_nonzero`, and feed every channel writer. Finalize/sync every temporary WAV. Silent preparation closes and deletes staging without final publication.

- [ ] **Step 5: Preserve split/name/publication regression behavior**

Run: `nix develop -c cargo test --test persistence_workspace --test export_wav`

Expected: exact existing recall names, dump names, 24-bit headers, split boundaries, collision behavior, and silence behavior pass.

- [ ] **Step 6: Add allocation instrumentation**

Under a serialized integration test, install a counting allocator and assert persistence makes no allocation whose size or count grows with selected frame count after the workspace has been constructed. Permit only documented bounded OS/path/control allocations until later tasks eliminate them.

---

### Task 4: Frozen transaction coordinator and cause-specific loss state

**Files:**
- Modify: `src/dump.rs`
- Modify: `src/control.rs`
- Modify: `src/sample_ring.rs`
- Modify: `tests/dump_coordinator.rs`

**Interfaces:**
- Consumes `CaptureArena`, `FrozenCaptureEpoch`, and `PersistenceWorkspace`.
- Produces `LossBreakdown`, loss-bearing `DumpOutcome`, `clear_in_order`, and pending frozen retry.

- [ ] **Step 1: Write RED loss and retry tests**

```rust
#[test]
fn no_new_audio_reports_and_acknowledges_each_loss_once() {
    let fixture = fixture();
    fixture.inject_capture_drops(7);
    fixture.clear_after_writing(5);
    let first = fixture.persist().unwrap();
    assert_eq!(first.losses().capture_dropped_frames, 7);
    assert_eq!(first.losses().cleared_frames, 5);
    assert_eq!(first.losses().lost_frames(), 12);
    assert_eq!(fixture.persist().unwrap().losses().lost_frames(), 0);
}
```

Add failed-publication non-acknowledgement, retention/clear separation, silent commit, shared recall/dump cursor, frozen retry while active capture wraps, and clear-waits ordering tests.

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test dump_coordinator`

Expected: compilation fails because `LossBreakdown` and frozen coordinator APIs do not exist.

- [ ] **Step 3: Refactor coordinator state**

```rust
struct DumpState {
    committed_until: Option<u64>,
    frozen: Option<FrozenTransaction>,
    acknowledged_dropped_frames: u64,
    pending_retention_lost_frames: u64,
    pending_cleared_frames: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct LossBreakdown {
    pub retention_lost_frames: u64,
    pub cleared_frames: u64,
    pub capture_dropped_frames: u64,
}
```

Use saturating addition for compatibility total. Capture cumulative dropped count at successful completion so drops during preparation are included; do not acknowledge on error.

- [ ] **Step 4: Implement frozen selection/retry/commit**

Retry `state.frozen` before freezing active audio. Success/silence releases the frozen epoch and commits its absolute end. Failure retains both frozen epoch and loss acknowledgement state.

- [ ] **Step 5: Implement ordered clear**

Under the transaction mutex, account retention already unavailable, count all recoverable uncommitted frozen/active frames as cleared, release/reset frozen and active epochs, set `committed_until` to the current absolute head, and retain pending loss for the next successful outcome.

- [ ] **Step 6: Update structured outcomes compatibly**

Add cause fields and compatibility total to every variant, including `NoNewAudio`. Use serde defaults for absent new fields when deserializing older response JSON.

- [ ] **Step 7: Verify coordinator and protocol**

Run: `nix develop -c cargo test --test dump_coordinator --test daemon_fake --lib`

Expected: loss is cause-specific, reported once, and cursor/frozen retry behavior remains exact.

---

### Task 5: Manifest-backed recovery and directory durability

**Files:**
- Create: `src/recovery.rs`
- Modify: `src/export_wav.rs`
- Modify: `src/persistence_workspace.rs`
- Modify: `src/lib.rs`
- Create: `tests/recovery.rs`

**Interfaces:**
- Produces versioned `TransactionManifest`, `ManifestStore`, `recover_recall_root`, `recover_dump_parent`, and `sync_directory`.
- Consumes identity-aware `OwnedPath` semantics from the current exporter.

- [ ] **Step 1: Write RED recovery tests**

Cover complete interrupted recall preservation, partial recall rollback, complete dump preservation, incomplete hidden dump removal, foreign inode replacement preservation, invalid/path-escaping manifest rejection, and untouched unmarked legacy artifacts.

```rust
#[test]
fn recovery_preserves_complete_recall_but_rolls_back_partial_recall() {
    let complete = interrupted_recall_fixture(2, 2);
    recover_recall_root(complete.staging_root()).unwrap();
    assert!(complete.final_file(0).exists());
    assert!(complete.final_file(1).exists());

    let partial = interrupted_recall_fixture(1, 2);
    recover_recall_root(partial.staging_root()).unwrap();
    assert!(!partial.final_file(0).exists());
}
```

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test recovery`

Expected: compilation fails because `recovery` does not exist.

- [ ] **Step 3: Implement atomic synchronized manifests**

Serialize into the preallocated manifest buffer, write/sync a transaction-owned temporary manifest, atomically replace only the transaction's manifest name, and sync the parent. Validate version, UID, roots, filename components, and dev/inode before cleanup.

- [ ] **Step 4: Integrate recall publication checkpoints**

Record expected finals before copying, then record each adjacent partial/final identity after successful creation/rename. Sync output directory after all final renames and before reporting publication success.

- [ ] **Step 5: Integrate dump publication checkpoints**

Sync every WAV and hidden directory, persist sibling manifest, rename directory no-replace, sync dump parent, then report publication success. Recovery infers complete publication only when every expected identity is present.

- [ ] **Step 6: Verify recovery and exporter regressions**

Run: `nix develop -c cargo test --test recovery --test export_wav --test persistence_workspace`

Expected: complete outputs survive, partial transaction-owned outputs roll back, foreign/unmarked paths remain untouched, and all directory sync hooks are observed.

---

### Task 6: Bounded persistence handoff on the existing control server

**Files:**
- Create: `src/control_server.rs`
- Modify: `src/daemon.rs`
- Modify: `src/lib.rs`
- Create: `tests/control_concurrency.rs`

**Interfaces:**
- Produces one bounded `OperationLane`, one prestarted operation worker, stream handoff, and drain/close semantics.
- Consumes the preplanned queue capacity and single worker-stack budget from Task 1.

- [ ] **Step 1: Write RED concurrency tests**

Use injected blocking persistence operations to prove status responds before release, clear runs afterward, stop rejects later persistence and waits, concurrent recall/dump are ordered, and queue saturation returns busy without creating threads.

```rust
#[test]
fn status_remains_responsive_while_persistence_is_blocked() {
    let server = server_with_blocking_persistence();
    let persistence = server.send_async(ControlRequest::Recall);
    server.wait_until_persistence_entered();
    assert!(server.send(ControlRequest::Status).ok);
    assert!(!persistence.is_finished());
    server.release_persistence();
}
```

- [ ] **Step 2: Verify RED**

Run: `nix develop -c cargo test --test control_concurrency`

Expected: current synchronous server blocks status and tests fail.

- [ ] **Step 3: Preserve the existing parser path and add bounded handoff**

Keep the existing single accept/parser path. Set a bounded socket read timeout,
prestart exactly one operation worker with an explicit stack size, touch its
configured stack pages during initialization, and use one preallocated bounded
operation queue. Transfer mutating requests and their streams to that worker;
reject queue overflow deterministically. Do not add per-connection or parser
worker threads.

- [ ] **Step 4: Implement one ordered mutation lane**

The parser transfers recall/dump/clear/lifecycle jobs and their sockets to the
bounded lane, then resumes accepting. Status is answered directly. Stop marks
admission closed, cancels queued unstarted persistence with shutting-down
responses, waits for the active job, and runs stop.

- [ ] **Step 5: Replace blocking accept loops**

Use a nonblocking Unix listener or poll timeout so the daemon notices stop without requiring another connection. Join owned workers before removing the socket.

- [ ] **Step 6: Verify fixed resource behavior**

Run: `nix develop -c cargo test --test control_concurrency --test daemon_fake --test daemon_idle`

Expected: status is responsive, mutating operations retain deterministic order,
and persistence adds exactly one fixed worker and one bounded queue.

---

### Task 7: Backend, daemon, memory-limit, and recovery integration

**Files:**
- Modify: `src/capture_fake.rs`
- Modify: `src/capture_jack.rs`
- Modify: `src/capture_pipewire.rs`
- Modify: `src/daemon.rs`
- Modify: `src/config.rs`
- Modify: `src/app_config.rs`
- Modify: `src/profile.rs`
- Modify: `src/control.rs`
- Modify: `src/main.rs`
- Modify: `tests/daemon_fake.rs`
- Modify: `tests/daemon_idle.rs`
- Modify: `tests/pipewire_backend.rs`

**Interfaces:**
- Consumes all preceding focused modules.
- Produces one `CaptureSession` lifecycle object owning backend, arena, coordinator, workspace, recovery roots, and its single operation worker.

- [ ] **Step 1: Write RED session-start tests**

Prove a tight `memory.max` fails before fake/backend startup with a component report, a valid plan creates and touches two rings/workspace, and replacement drains the old operation lane before activating the new session.

- [ ] **Step 2: Route all backends through `CaptureArena`**

Replace direct `Arc<SampleRing>` callback fields with `Arc<CaptureArena>`. Preserve callback allocation-freedom, negotiated sample rate/channel metadata, and existing fake/JACK/PipeWire public behavior.

- [ ] **Step 3: Construct one session from one validated plan**

Resolve final sample rate/channels first, calculate and validate memory,
allocate/touch every component, start the operation worker, then activate the
backend. On any failure, tear down already-created resources and leave the
daemon idle/faulted.

- [ ] **Step 4: Run recovery before admitting persistence**

Recover `/tmp/LAMB/staging` manifests and known configured dump parents. Scan every app-profile output root but act only on valid marked transactions. Leave unmarked legacy artifacts untouched.

- [ ] **Step 5: Route responses and CLI rendering**

Render separate loss causes plus compatibility total for Written, SkippedSilent, and NoNewAudio. Keep all three successful and retain older JSON compatibility.

- [ ] **Step 6: Add end-to-end tests**

Cover recall→dump and dump→recall, status during blocked export, clear ordering and loss-bearing NoNewAudio, stop drain, failed frozen retry while capture continues, no export-induced drops, recovery before first command, and early memory-limit failure.

- [ ] **Step 7: Run integrated suites**

Run: `nix develop -c cargo test --all-targets && nix develop -c cargo clippy --all-targets -- -D warnings`

Expected: all legacy and new tests pass with no warnings.

---

### Task 8: Documentation and final verification

**Files:**
- Modify: `README.md`
- Modify: `docs/specs/2026-08-18-preallocated-persistence-runtime-design.md` only if implementation names materially differ.

**Interfaces:**
- Verifies the complete patch; adds no runtime behavior.

- [ ] **Step 1: Document deterministic memory requirements**

Explain dual full-ring reservation, startup page commitment, higher `memory.max` requirements, bounded workspace, and early failure behavior.

- [ ] **Step 2: Document concurrency/recovery/loss behavior**

Explain responsive status, ordered clear/stop, manifest-only recovery, and the three loss causes while preserving shared-cursor semantics.

- [ ] **Step 3: Run fresh Rust gates**

Run: `nix develop -c bash -lc 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets'`

Expected: formatting and lint are clean; every test passes.

- [ ] **Step 4: Run package build**

Run: `nix build "path:.#lamb"`

Expected: the complete dirty working tree package builds. After files are committed, also run `nix build .#lamb`.

- [ ] **Step 5: Inspect scope and artifacts**

Run: `git status --short && git diff --check && git diff --stat && git log --oneline -5`

Expected: only planned source/tests/docs are changed; no WAV, manifest, staging, or generated transaction artifacts are tracked.

## Self-Review

- Coverage: dual-ring ownership, absolute epochs, deterministic RAM, page commitment, fixed streaming WAVs, shared cursor, failed frozen retries, concurrent control, ordered clear/stop, manifest recovery, directory sync, and all three loss causes map to explicit tasks and tests.
- Completeness: every production interface is introduced before its integration consumer; no behavior is deferred.
- Type consistency: `SessionMemoryPlan`, `CaptureArena`, `FrozenCaptureEpoch`, `PersistenceWorkspace`, `PreparedPersistence`, `LossBreakdown`, `TransactionManifest`, and `ControlServer` retain the same names across tasks.
- Scope: timestamp uniqueness, threshold silence, NixOS options, and backend format changes remain excluded.
