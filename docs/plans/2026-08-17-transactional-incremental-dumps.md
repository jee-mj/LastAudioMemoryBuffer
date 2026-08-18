# Transactional Incremental Dumps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `recall` and `dump` consume one shared stream of source-frame ranges, retry failed ranges, suppress wholly digital-silent ranges, and publish complete WAV transactions without blocking capture during filesystem work.

**Architecture:** `SampleRing` exposes exact absolute frame ranges and materializes selected samples into an owned snapshot before export. A capture-session-scoped `DumpCoordinator` holds the shared monotonic cursor and serializes selection, classification, publication, and commit while capture continues independently. Two publishers share WAV encoding but preserve distinct layouts: flat configured-output recall publication through `/tmp/LAMB/staging`, and atomic timestamp-directory dump publication.

**Tech Stack:** Rust 2021, existing `SampleRing`, `std::sync::{Arc, Mutex}`, 24-bit WAV writer, Linux `renameat2(RENAME_NOREPLACE)`, Cargo integration tests.

## Global Constraints

- `recall` and `dump` share one consumption cursor for each active capture session.
- Select half-open absolute source-frame ranges `[start, end)`; capture `end` before silence scanning or filesystem work.
- The first operation starts at the oldest retained frame; later operations start at `committed_until.max(oldest)`.
- Exact-zero silence means every source `f32` sample compares equal to `0.0`; `-0.0` is silent and any nonzero sample preserves all channels.
- Silent and successfully written ranges advance the cursor; empty ranges return `NoNewAudio`; failures never advance it.
- Keep the transaction mutex from range selection through cursor commit; capture callbacks never acquire it.
- Materialize an owned snapshot and release ring chunk pins before silence scanning or filesystem work.
- Recall keeps existing flat timestamped filenames and configured `outputDir` exactly.
- Recall preparation uses `/tmp/LAMB/staging/<unique-id>/`, then copies to unique adjacent hidden partials, syncs, and atomically renames without overwrite.
- A partial recall publication failure removes only files created by that transaction plus its partial/staging artifacts, best-effort.
- Dump publishes a complete timestamped directory with one no-overwrite atomic rename.
- Do not add noise thresholds, trimming, per-channel suppression, VAD, backend changes, or NixOS configuration.
- Do not commit changes unless the user explicitly requests a commit.

---

## File Structure

- Create `src/dump.rs`: source range metadata, owned snapshots, serialized cursor transaction, outcomes, publisher callback boundary.
- Modify `src/sample_ring.rs`: absolute ring boundaries and exact-range pinned snapshot selection.
- Modify `src/export_wav.rs`: owned-snapshot WAV encoding and transactional recall/dump publishers.
- Modify `src/control.rs`: structured persistence outcomes and successful CLI rendering for all three outcomes.
- Modify `src/daemon.rs`: one coordinator per capture session, shared by recall/dump, plus mode-specific publishers.
- Modify `src/lib.rs`: export the focused `dump` module.
- Modify `tests/ring_snapshot.rs`: exact range, wraparound, and owned-snapshot tests.
- Modify `tests/export_wav.rs`: transactional publication, collision, cleanup, and silence-safe no-output tests.
- Create `tests/dump_coordinator.rs`: incremental ranges, concurrent serialization, silence, retry, capture-during-export, wraparound, and retention-loss tests.
- Modify `tests/daemon_fake.rs`: shared recall/dump consumption and structured response regression tests.

---

### Task 1: Exact absolute frame snapshots

**Files:**
- Modify: `src/sample_ring.rs`
- Test: `tests/ring_snapshot.rs`

**Interfaces:**
- Produces: `SampleRing::oldest_frame() -> u64`
- Produces: `SampleRing::write_head_frame() -> u64`
- Produces: `SampleRing::snapshot_range(Range<u64>) -> Result<Snapshot>`
- Produces: `Snapshot::start_frame()`, `Snapshot::end_frame()`, and contiguous exact-range validation.

- [ ] **Step 1: Write failing exact-range tests**

Add tests that write known interleaved frames, request `2..5`, and assert each channel contains exactly those three frames. Add a wrap test that fills more than capacity, asserts `oldest_frame()`, snapshots `[oldest, head)`, and rejects a range beginning before `oldest`.

```rust
#[test]
fn snapshot_range_returns_exact_absolute_frames() {
    let ring = wide_ring();
    ring.write_interleaved(&interleaved_frames(0, 8), 2).unwrap();
    let snapshot = ring.snapshot_range(2..5).unwrap();
    assert_eq!((snapshot.start_frame(), snapshot.end_frame()), (2, 5));
    assert_eq!(snapshot.read_channel_samples(0).unwrap(), vec![2.0, 3.0, 4.0]);
    assert_eq!(snapshot.read_channel_samples(1).unwrap(), vec![102.0, 103.0, 104.0]);
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cargo test --test ring_snapshot snapshot_range_returns_exact_absolute_frames`

Expected: compilation fails because the absolute-range APIs do not exist.

- [ ] **Step 3: Implement absolute boundaries and exact selection**

Store `start_frame` and `end_frame` on `Snapshot`. Define `oldest_frame()` as the maximum of the retention boundary, `clear_after_frame`, and the oldest contiguous published frame. Implement `snapshot_range` by validating `start <= end`, `start >= oldest`, and `end <= write_head`; pin overlapping segments, sort by absolute start, and reject any gap or overlap that prevents exact coverage. Unlike `snapshot_last_frames`, never extend the selected end when capture advances.

```rust
pub fn write_head_frame(&self) -> u64 {
    self.global_write_frame.load(Ordering::Acquire)
}

pub fn oldest_frame(&self) -> u64 {
    let head = self.write_head_frame();
    let retained = head.saturating_sub(self.status().capacity_frames);
    retained.max(self.clear_after_frame.load(Ordering::Acquire))
}

pub fn snapshot_range(&self, range: Range<u64>) -> Result<Snapshot> {
    // Validate against one captured head, collect/pin only range, then prove
    // the sorted segment intervals cover exactly [range.start, range.end).
}
```

- [ ] **Step 4: Keep `snapshot_last_frames` backward-compatible**

Make it capture the current head and call `snapshot_range(head.saturating_sub(requested).max(oldest)..head)`. Preserve an empty snapshot when start equals end.

- [ ] **Step 5: Run ring tests and format**

Run: `cargo test --test ring_snapshot && cargo fmt --check`

Expected: all ring tests pass, including existing clear and concurrent-writer tests.

---

### Task 2: Owned source snapshot and exact digital-silence classification

**Files:**
- Create: `src/dump.rs`
- Modify: `src/lib.rs`
- Test: `tests/dump_coordinator.rs`

**Interfaces:**
- Consumes: exact `Snapshot` from Task 1.
- Produces: `FrameRange { pub start: u64, pub end: u64 }`
- Produces: `SampleSnapshot::from_ring_range(&SampleRing, FrameRange) -> Result<Self>`
- Produces: `SampleSnapshot::{range, frames, channels, sample_rate, channel_samples, is_digital_silence}`.

- [ ] **Step 1: Write failing owned-snapshot and silence tests**

Cover all `0.0`, all `-0.0`, one nonzero sample, one active channel with silent peers, and proof that a materialized snapshot remains unchanged after the ring wraps.

```rust
#[test]
fn negative_zero_is_digital_silence_but_one_nonzero_sample_is_not() {
    let silent = owned_snapshot(&[0.0, -0.0, 0.0, -0.0], 2);
    assert!(silent.is_digital_silence());
    let active = owned_snapshot(&[0.0, 0.0, 0.0, f32::MIN_POSITIVE], 2);
    assert!(!active.is_digital_silence());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run: `cargo test --test dump_coordinator digital_silence`

Expected: compilation fails because `lamb::dump` does not exist.

- [ ] **Step 3: Implement the owned snapshot**

Materialize every channel while the short-lived ring `Snapshot` pins chunks, validate every channel length equals `range.end - range.start`, then drop the ring snapshot before returning. The silence method must short-circuit through flattened channel samples.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange { pub start: u64, pub end: u64 }

pub struct SampleSnapshot {
    range: FrameRange,
    sample_rate: u32,
    channel_samples: Vec<Vec<f32>>,
}

pub fn is_digital_silence(&self) -> bool {
    self.channel_samples.iter().flatten().all(|sample| *sample == 0.0)
}
```

- [ ] **Step 4: Verify owned snapshot behavior**

Run: `cargo test --test dump_coordinator digital_silence owned_snapshot`

Expected: all selected tests pass.

---

### Task 3: Serialized transactional frame cursor

**Files:**
- Modify: `src/dump.rs`
- Test: `tests/dump_coordinator.rs`

**Interfaces:**
- Produces: `DumpCoordinator::new() -> Self`
- Produces: `DumpCoordinator::dump<F>(&self, ring: &SampleRing, publisher: F) -> Result<DumpOutcome>` where `F: FnOnce(&SampleSnapshot) -> Result<PublishedOutput>`.
- Produces: `PublishedOutput { pub output_directory: PathBuf, pub files: Vec<PathBuf> }`.
- Produces: `DumpOutcome::{Written, SkippedSilent, NoNewAudio}` with frame range/count and `lost_frames` where applicable.

- [ ] **Step 1: Write incremental RED tests**

Test A then B then no-new; silence then sound; failed publisher then retry of the identical range; write B inside the first publisher and confirm it belongs only to the next call.

```rust
let first = coordinator.dump(&ring, |snapshot| {
    assert_eq!(snapshot.range(), FrameRange { start: 0, end: 3 });
    ring.write_interleaved(&segment_b, 2).unwrap();
    Ok(fake_publication("a"))
}).unwrap();
let second = coordinator.dump(&ring, |snapshot| {
    assert_eq!(snapshot.range(), FrameRange { start: 3, end: 5 });
    Ok(fake_publication("b"))
}).unwrap();
```

- [ ] **Step 2: Run incremental tests and verify RED**

Run: `cargo test --test dump_coordinator incremental failed_publish capture_during_publish`

Expected: compilation fails because `DumpCoordinator` is incomplete.

- [ ] **Step 3: Implement the transaction state machine**

Hold `Mutex<DumpState>` for the complete call. Capture `oldest` and `end` before creating the owned snapshot. Compute lost frames when an existing cursor is below `oldest`. Commit `end` only for `SkippedSilent` or a successful publisher result.

```rust
pub fn dump<F>(&self, ring: &SampleRing, publisher: F) -> Result<DumpOutcome>
where F: FnOnce(&SampleSnapshot) -> Result<PublishedOutput> {
    let mut state = self.state.lock().map_err(|_| LambError::Export("dump state lock poisoned".into()))?;
    let oldest = ring.oldest_frame();
    let end = ring.write_head_frame();
    let requested = state.committed_until.unwrap_or(oldest);
    let lost_frames = oldest.saturating_sub(requested);
    let start = requested.max(oldest);
    if start >= end { return Ok(DumpOutcome::NoNewAudio); }
    let range = FrameRange { start, end };
    let snapshot = SampleSnapshot::from_ring_range(ring, range)?;
    if snapshot.is_digital_silence() {
        state.committed_until = Some(end);
        return Ok(DumpOutcome::SkippedSilent { range, frames: end - start, lost_frames });
    }
    let published = publisher(&snapshot)?;
    state.committed_until = Some(end);
    Ok(DumpOutcome::Written { range, frames: end - start, lost_frames,
        output_directory: published.output_directory, files: published.files })
}
```

- [ ] **Step 4: Add concurrency and retention-loss tests**

Use `Arc<DumpCoordinator>`, two threads, and a barrier in the first publisher. Assert exactly one request writes the range and the other receives `NoNewAudio`. Wrap the ring after an initial successful call and assert the next range starts at `oldest_frame()` and reports the skipped frame count.

- [ ] **Step 5: Run all coordinator tests**

Run: `cargo test --test dump_coordinator`

Expected: incremental, silence, concurrency, retry, wrap, and retention-loss tests all pass.

---

### Task 4: Transactional WAV publishers

**Files:**
- Modify: `src/export_wav.rs`
- Test: `tests/export_wav.rs`

**Interfaces:**
- Consumes: `SampleSnapshot` and `PublishedOutput` from `src/dump.rs`.
- Produces: `publish_recall(RecallPublishRequest<'_>) -> Result<PublishedOutput>`.
- Produces: `publish_dump(DumpPublishRequest<'_>) -> Result<PublishedOutput>`.
- Keeps: existing 24-bit conversion, split-on-frame-boundary behavior, and recall filename format.

- [ ] **Step 1: Convert low-level WAV tests to owned snapshots**

Retain RIFF/header and split tests while passing a `SampleSnapshot`. Assert all channel WAV frame counts remain aligned.

- [ ] **Step 2: Write transactional RED tests**

Add tests for recall flat filenames, dump timestamp directory, no-overwrite collisions, partial multichannel recall rollback, staging cleanup, and no visible dump directory after write failure.

```rust
#[test]
fn recall_collision_rolls_back_only_files_created_by_this_transaction() {
    // Pre-create the second channel's final path, publish two channels,
    // assert the call fails, first new final is removed, and pre-existing
    // second final retains its original bytes.
}
```

- [ ] **Step 3: Run exporter tests and verify RED**

Run: `cargo test --test export_wav`

Expected: compilation fails because transactional publisher APIs do not exist.

- [ ] **Step 4: Factor directory-local WAV encoding**

Write all WAVs into a caller-provided staging directory with final basenames. Make `write_mono_wav` flush and call `File::sync_all()` after `BufWriter::into_inner()`. Keep split naming unchanged for recall and simple channel naming for dump.

- [ ] **Step 5: Implement Linux no-overwrite rename**

Use `libc::renameat2(AT_FDCWD, old, AT_FDCWD, new, RENAME_NOREPLACE)` with `CString` paths. Map `EEXIST` and all other errors through `io_error`; never use `fs::rename` for final publication because it overwrites on Unix.

- [ ] **Step 6: Implement recall publication**

Create a unique `/tmp/LAMB/staging/<id>` directory (tests inject a temporary staging root). After all staged WAVs close successfully, copy each to a unique `.<final-name>.<id>.partial` beside its destination using `OpenOptions::create_new(true)`, `io::copy`, `flush`, and `sync_all`; then no-overwrite rename it to the flat final path. Track only finals created by this transaction. On any error, remove tracked finals, remaining partials, and staging directory best-effort, then return the original error.

- [ ] **Step 7: Implement dump publication**

Create an adjacent hidden directory such as `<dump-parent>/.tmp-lamb-<id>`, write and close all channel WAVs, sync files, then no-overwrite rename that directory to `<dump-parent>/<timestamp>`. On failure, remove only the temporary directory and leave no final directory.

- [ ] **Step 8: Run exporter and coordinator tests**

Run: `cargo test --test export_wav --test dump_coordinator`

Expected: all tests pass without filesystem artifacts outside their temporary roots.

---

### Task 5: Capture-session integration and shared recall/dump consumption

**Files:**
- Modify: `src/daemon.rs`
- Modify: `src/control.rs`
- Modify: `src/main.rs`
- Test: `tests/daemon_fake.rs`

**Interfaces:**
- Consumes: `DumpCoordinator`, `DumpOutcome`, `publish_recall`, and `publish_dump`.
- Produces: structured `PersistenceOutcomeResponse` attached optionally to `ControlResponse`.
- Produces: `client_recall` and `client_dump` that print Written, SkippedSilent, and NoNewAudio as successful outcomes.

- [ ] **Step 1: Add structured response RED tests**

Round-trip each tagged response through JSON. Include start/end, frame count, duration seconds, lost frames, output directory, and files for written responses. Ensure `SkippedSilent` and `NoNewAudio` retain `ok: true`.

- [ ] **Step 2: Add daemon shared-cursor RED test**

Start the legacy fake daemon, wait for audio, and invoke recall followed by dump. Because the fake backend may capture between requests, accept either `NoNewAudio` or a second written range whose `start_frame` equals the first operation's `end_frame`; never accept overlap. Add the reverse ordering in a fresh daemon. This deterministically proves shared consumption while preserving the rule that a truly immediate operation with no intervening frames returns `NoNewAudio`; do not use file byte counts as source-frame truth.

- [ ] **Step 3: Run daemon tests and verify RED**

Run: `cargo test --test daemon_fake`

Expected: tests fail because daemon handlers still snapshot the complete retention window independently.

- [ ] **Step 4: Add one coordinator per capture session**

Add `DumpCoordinator` to legacy `DaemonContext`. Add `Option<Arc<DumpCoordinator>>` to `AppRuntimeState`, initialize it when capture starts, and clear it when capture stops or is replaced. Clone ring/coordinator/profile data while briefly holding runtime state, then release the runtime mutex before calling the coordinator.

- [ ] **Step 5: Route all four handlers through the shared coordinator**

Legacy/app recall call `publish_recall` with configured output directory, existing detailed names, and `/tmp/LAMB/staging`. Legacy dump publishes a timestamp directory under its configured output directory; app dump publishes under `~/.cache/lamb/out`. Both dump paths use simple channel filenames inside the committed directory. Remove direct daemon calls to `snapshot_last_frames` and `export_snapshot_wav`.

- [ ] **Step 6: Add structured control outcomes**

Define a tagged response enum and an optional field on `ControlResponse`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistenceOutcomeResponse {
    Written { start_frame: u64, end_frame: u64, frames: u64,
        duration_seconds: f64, lost_frames: u64,
        output_directory: PathBuf, files: Vec<PathBuf> },
    SkippedSilent { start_frame: u64, end_frame: u64, frames: u64,
        duration_seconds: f64, lost_frames: u64 },
    NoNewAudio,
}
```

Set the optional field to `None` for unrelated commands. Keep errors as `ok: false`; all three persistence outcomes use `ok: true`.

- [ ] **Step 7: Render CLI success text**

Use one client helper for both recall and dump. Print written paths, exact-zero skip text, no-new text, and a retention warning when `lost_frames > 0`. Change `main.rs` recall dispatch from the silent `client_send_simple` path to `client_recall`.

- [ ] **Step 8: Run daemon/control tests**

Run: `cargo test --test daemon_fake --test daemon_idle`

Expected: shared cursor and structured response tests pass; status/start/stop/reload regressions remain green.

---

### Task 6: Full regression and operational verification

**Files:**
- Modify: `README.md` only where command behavior needs explanation.

**Interfaces:**
- Verifies all preceding tasks; introduces no new runtime interface.

- [ ] **Step 1: Document incremental shared consumption**

Add a concise README section stating that recall/dump share a capture-session cursor, exact digital silence is consumed without files, recall remains flat in configured output, and dump uses timestamped directories.

- [ ] **Step 2: Run formatting and strict lint**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings`

Expected: both commands exit 0 with no warnings.

- [ ] **Step 3: Run the complete Rust test suite**

Run: `cargo test`

Expected: all unit and integration tests pass.

- [ ] **Step 4: Build the Nix package**

Run: `nix build .#lamb`

Expected: package builds successfully with existing service/module behavior unchanged.

- [ ] **Step 5: Inspect final changes**

Run: `git status --short && git diff --check && git diff`

Expected: only the planned Rust tests/modules, README, and this plan are modified; no staging/output artifacts are tracked.

## Self-Review

- Spec coverage: cursor sharing, half-open exact ranges, first-dump retention, retry, concurrency, capture boundary, wrap/loss warning, exact silence, owned snapshots, both filesystem publication policies, no-overwrite, rollback, structured outcomes, CLI success, and regression checks each map to a task.
- Completeness scan: every implementation step is concrete; excluded noise/VAD work is explicit.
- Type consistency: `FrameRange`, `SampleSnapshot`, `PublishedOutput`, `DumpOutcome`, publisher requests, and structured control response names are introduced before their consumers.
