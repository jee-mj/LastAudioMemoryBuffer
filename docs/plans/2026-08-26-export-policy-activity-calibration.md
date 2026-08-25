# Export Policy, Activity Detection, and Calibration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace command-owned export naming and transaction-wide channel publication with typed layout/activity policy, deterministic sparse-channel preparation, common leading crop, and daemon-owned threshold calibration.

**Architecture:** New `activity`, `export_policy`, and `calibration` modules own detector policy, canonical path rendering, and calibration state. The coordinator retains one startup-allocated frozen export decision across retries; the persistence workspace classifies before it renders or opens sparse writers, then dispatches a manifest-backed file-set or separately atomic timestamp-directory publisher. A dedicated capture-side calibration slot records bounded future mono audio while the daemon operation lane serializes threshold commands and durable config/state publication.

**Tech Stack:** Rust 2021, serde/TOML/JSON, clap, existing exact/page-materialized arenas, Linux `renameat2`, directory `fsync`, IEEE-float WAV, SHA-256 stable input identifiers, Cargo integration tests, Nix.

**Spec:** `docs/specs/2026-08-26-export-policy-activity-calibration-design.md`

## Global Constraints

- Never change capture topology or perform detector/path/filesystem work in the realtime callback.
- `Always | Auto | Never`, detector result, and whole-export compatibility gate are separate typed decisions.
- `Ambiguous` always retains a channel; non-finite export samples are ambiguous.
- Modern profile omission resolves to `Auto + WindowedRmsPeak`; legacy config keeps historical whole-export exact-zero and command layouts.
- A successful operation consumes the full selected range even when WAVs begin later or channels are omitted.
- Every retained WAV uses one common export start; v1 preserves exactly two seconds of preroll and never trims the tail.
- A frozen retry never reclassifies channel disposition, first evidence, crop, or sparse file set.
- Custom paths use manifest-backed file-set publication; timestamp-directory remains a separate atomic-directory strategy.
- Render and collision-check every actual channel/part path before final publication, preserve no-overwrite, and enforce `maximum_path_bytes`.
- Persistence and detector storage are startup-planned; no allocation scales with selected export duration.
- Calibration observes only future admitted frames for 1–30 seconds, defaults to 5 seconds, uses preallocated capture-side storage, and never freezes/consumes the export cursor.
- Threshold/config/state mutation is daemon-owned, serialized, durable before live installation, and unsupported in legacy mode.
- Do not weaken identity-aware recovery or delete foreign/unmarked paths.

---

## File structure

### New focused modules

- `src/activity.rs`: typed channel/detector/threshold model, exact-zero and windowed detector v1, fixed detector workspace, frozen export decision, crop calculation.
- `src/export_policy.rs`: typed layouts, pattern parser, canonical renderer, preview API, capacity/collision validation, publication strategy selection.
- `src/calibration.rs`: stable input identity, calibration metadata, F32LE sample writer/reader helpers, generation store, staleness, atomic config/state transaction support.
- `tests/activity.rs`: detector, disposition, evidence, crop, and retry-decision unit/integration tests.
- `tests/export_policy.rs`: presets, custom rendering/preview, token/path/collision/capacity tests.
- `tests/calibration.rs`: bounded sample capture metadata, state generations, staleness, atomic failure, and WAV tests.
- `tests/threshold_cli.rs`: clap/control protocol and daemon threshold command tests.

### Existing modules with scoped changes

- `src/app_config.rs`: typed profile fields, channel activity table, serde names.
- `src/profile.rs`: policy/identity resolution and validation.
- `src/config.rs`: explicit legacy resolved export policy.
- `src/memory_plan.rs`: decision, detector, and calibration components.
- `src/capture_runtime.rs`: calibration capacity inputs and allocation wiring.
- `src/capture_arena.rs`: independent calibration request/result slot and future-frame accumulator.
- `src/persistence_workspace.rs`: two-pass classification/sparse writer planning/common crop.
- `src/dump.rs`: frozen decision ownership and new skip/export-range outcomes.
- `src/export_wav.rs`: strategy-based prepared publication and nested file-set paths.
- `src/recovery.rs`: sparse/nested strategy manifests while reading version 1.
- `src/control.rs`: new outcome fields, `SkippedByPolicy`, threshold requests/reports/clients.
- `src/control_server.rs`: threshold request admission classification.
- `src/daemon.rs`: one resolved policy per session, command-independent persistence requests, threshold handlers, app output-root correction.
- `src/main.rs`: `threshold` clap command family and default socket resolution.
- `src/lib.rs`: export the three focused modules.
- Existing tests under `tests/`: update fixtures and preserve recovery/memory/daemon regressions.
- `README.md`: policy, layouts, crop/accounting, detector, calibration, and examples.
- `Cargo.toml` / `Cargo.lock`: add only the pure-Rust SHA-256 dependency used for stable state directory ids.

---

### Task 1: Typed profile and resolved export policy

**Files:**
- Create: `src/activity.rs`
- Create: `src/export_policy.rs`
- Modify: `src/app_config.rs`
- Modify: `src/profile.rs`
- Modify: `src/config.rs`
- Modify: `src/lib.rs`
- Test: `tests/app_config.rs`
- Test: `tests/config_validation.rs`
- Test: `tests/activity.rs`

**Interfaces:**
- Produces: `ChannelExportMode`, `ActivityDetectorKind`, `ActivityResult`, `ThresholdSource`, `SilencePolicyPreset`.
- Produces: `ChannelActivityPolicy`, `ResolvedActivityPolicy`, `ResolvedExportPolicy`, `ResolvedLayout`, and `ExportCommand`.
- Produces: `profile::validate_profile(...) -> Result<ResolvedProfile>` with `ResolvedProfile.export_policy`.
- Consumes later: detector, renderer, daemon, persistence, and calibration tasks use these exact resolved types.

- [ ] **Step 1: Add RED serde/resolution tests**

Add tests that parse kebab-case enum values, a per-port `exportMode`, channel
threshold records, and modern omission. Assert:

```rust
let resolved = resolve_active_profile(&cfg).unwrap().unwrap();
assert_eq!(resolved.export_policy.activity.detector, ActivityDetectorKind::WindowedRmsPeak);
assert_eq!(resolved.export_policy.activity.channels[0].mode, ChannelExportMode::Auto);
```

Add explicit tests that `silencePolicy` conflicts with profile-wide
`defaultChannelMode`/`activityDetector`, per-port overrides remain legal,
reserved detectors parse but validation rejects them, threshold dBFS must be
finite in `[-120.0, 0.0]`, and channel activity keys must resolve exactly.

- [ ] **Step 2: Run RED tests**

Run: `cargo test --test app_config --test config_validation --test activity`

Expected: compilation fails because typed activity/export policy APIs and config fields do not exist.

- [ ] **Step 3: Implement typed config declarations**

Add serde enums with explicit kebab-case names and profile structures equivalent to:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelExportMode { Always, Auto, Never }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityDetectorKind {
    ExactZero,
    WindowedRmsPeak,
    FixedLevel,
    CalibratedNoiseFloor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityResult { Active, Inactive, Ambiguous }
```

Add `CapturePort.export_mode`, `ProfileExportConfig` layout/activity fields,
and `ProfileConfig.channels: BTreeMap<String, ProfileChannelConfig>`. Keep raw
TOML structs serializable so daemon-owned threshold mutation can write them.

- [ ] **Step 4: Implement deterministic profile resolution**

Resolve the matrix from the spec, including:

```text
modern omission              -> Auto + WindowedRmsPeak + trim
per-channel-exact-zero       -> Auto + ExactZero + trim
all-channels-exact-zero      -> global gate + Always + ExactZero + no trim
legacy configVersion=1       -> global gate + Always + legacy command layout + no trim
```

Validate reserved detectors with exact errors and replace
`ResolvedProfile.export_output_dir/export_mode/export_format` strings with one
typed `export_policy` while retaining `mode = per-channel` and `format = wav`
input compatibility.

- [ ] **Step 5: Run focused tests and lint the new modules**

Run: `cargo test --test app_config --test config_validation --test activity && cargo clippy --lib -- -D warnings`

Expected: all typed config/default/conflict tests pass and Clippy emits no warnings.

- [ ] **Step 6: Commit**

```bash
git add src/activity.rs src/export_policy.rs src/app_config.rs src/profile.rs src/config.rs src/lib.rs tests/app_config.rs tests/config_validation.rs tests/activity.rs
git commit -m "feat: add typed export activity policy"
```

---

### Task 2: Canonical layout parser, renderer, and preview

**Files:**
- Modify: `src/export_policy.rs`
- Create: `tests/export_policy.rs`
- Modify: `tests/app_config.rs`

**Interfaces:**
- Consumes: `ResolvedExportPolicy`, `ResolvedLayout`, `ExportCommand` from Task 1.
- Produces: `ValidatedPattern::parse`, `RenderContext`, `RenderedOutput`, `render_output_into`, and `preview_export_paths`.
- Produces: `PublicationStrategy::{FileSet, AtomicDirectory}` selected solely by resolved layout/legacy command default.

- [ ] **Step 1: Write RED preset and token tests**

Cover flat-detailed, timestamp-directory, custom nested patterns, profile token,
absolute part boundaries, one-based part, and suffix behavior:

```rust
assert_eq!(unsplit.part_suffix, "");
assert_eq!(split[0].part, 1);
assert_eq!(split[0].part_suffix, "-part001");
assert_eq!(split[0].start_frame, 1_104_000);
```

Assert timestamp-directory resolves `AtomicDirectory`; custom `{timestamp}`
still resolves `FileSet`.

- [ ] **Step 2: Write RED rejection tests**

Test unmatched/nested/unknown tokens, absolute directory output, `.`/`..`, NUL,
leading/trailing/double separators, filename `/`, empty filenames, injected
unsafe channel/profile values, maximum-path overflow, duplicate final paths,
file/parent conflicts, and cross-channel/part collisions.

- [ ] **Step 3: Run RED tests**

Run: `cargo test --test export_policy`

Expected: compilation fails because pattern/render/preview APIs are absent.

- [ ] **Step 4: Implement one parsed pattern language**

Parse into literal/token segments once. Accept exactly the eight tokens from
the spec. Use the same token formatter for preview and reusable persistence
paths. Do not add escaping or fallback token behavior.

Expose a typed preview:

```rust
pub fn preview_export_paths(
    policy: &ResolvedExportPolicy,
    context: &RenderContext<'_>,
    retained_channels: &[usize],
) -> Result<Vec<RenderedOutput>>;
```

Each entry includes original channel index/name, part, part count, absolute
half-open range, rendered relative directory, filename, and final path.

- [ ] **Step 5: Implement path/capacity/collision validation**

Use checked arithmetic for worst-case token widths and actual rendering. Check
all final paths against each other, not only matching part indexes. Require
`outputDir` absolute and all rendered directories lexical relative normal
components. Keep filesystem existence/symlink checks for the publication
preflight task.

- [ ] **Step 6: Verify renderer tests**

Run: `cargo test --test export_policy --test app_config && cargo clippy --lib -- -D warnings`

Expected: every preset/preview/rejection test passes.

- [ ] **Step 7: Commit**

```bash
git add src/export_policy.rs tests/export_policy.rs tests/app_config.rs
git commit -m "feat: add canonical export layout renderer"
```

---

### Task 3: Detector v1 and frozen channel decision

**Files:**
- Modify: `src/activity.rs`
- Modify: `src/memory_plan.rs`
- Modify: `tests/activity.rs`
- Modify: `tests/memory_plan.rs`

**Interfaces:**
- Consumes: typed activity policies from Task 1 and `FrozenCaptureEpoch` fixed reads.
- Produces: `ActivityDetector` trait, `DetectorWorkspace`, `FrozenChannelDecision`, `FrozenExportDecision`.
- Produces: `classify_frozen_epoch(frozen, policy, workspace, decision) -> Result<DecisionOutcome>`.
- Produces: startup plan components `frozen_export_decisions` and `activity_detector_workspace`.

- [ ] **Step 1: Write RED exact-zero and disposition tests**

Use four interleaved channels: active mic, one small guitar sample, exact-zero,
and negative-zero. Assert only mic/guitar are kept under auto exact-zero. Cover
one active channel, all inactive, all never, never+inactive auto, always silent,
and NaN/±Inf -> ambiguous/keep.

- [ ] **Step 2: Write RED windowed detector tests**

At a small deterministic test sample rate, generate windows proving 20 ms / 10
ms overlap, close threshold at −6 dB, hysteresis, 100 ms sustained-open measured
from first window start to current end, +12 dB transient bypass, weaker RMS
evidence -> ambiguous, and earliest evidence uses window start.

- [ ] **Step 3: Write RED crop tests**

Assert earliest evidence across retained channels gives one common two-second
preroll, clamps to transaction start, never changes end, and always-with-no-
evidence uses the original start. Assert compatibility policy disables crop.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test activity --test memory_plan`

Expected: tests fail because detector/workspace/decision plan is incomplete.

- [ ] **Step 5: Implement fixed detector workspace**

Allocate exact arrays for two staggered accumulators, gate state, result,
non-finite flag, and first-evidence frame per channel. Implement block traversal
without per-duration allocations. RMS is `sqrt(sum_squares / count)` converted
to dBFS with zero at −120 dBFS. Peak is used only for transient bypass.

- [ ] **Step 6: Implement immutable decision finalization**

Populate mode/result/disposition/evidence, apply all-never precedence and the
optional global exact-zero gate, then calculate the common crop. Set
`decision.valid = true` only after every field validates. Refuse to mutate a
valid decision for the same frozen transaction.

- [ ] **Step 7: Add exact startup accounting**

Add checked memory components and getters. Allocate/page-materialize decision
and detector arrays from the plan. Extend memory tests to check exact component
presence, overflow, and `memory.max` inclusion.

- [ ] **Step 8: Verify detector and memory tests**

Run: `cargo test --test activity --test memory_plan && cargo clippy --all-targets -- -D warnings`

Expected: detector, crop, and memory accounting tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/activity.rs src/memory_plan.rs tests/activity.rs tests/memory_plan.rs
git commit -m "feat: add bounded activity detector"
```

---

### Task 4: Coordinator-owned frozen decision and outcome ranges

**Files:**
- Modify: `src/dump.rs`
- Modify: `src/control.rs`
- Modify: `tests/dump_coordinator.rs`
- Modify: `src/daemon.rs` test fixtures only where construction changes

**Interfaces:**
- Consumes: `FrozenExportDecision` and plan allocation from Task 3.
- Produces: coordinator construction with reusable decision storage.
- Produces: `DumpOutcome::SkippedByPolicy` and `DumpOutcome::Written { export_start_frame, export_frames, ... }`.
- Preserves: current frozen epoch, loss accounting, indeterminate recovery, and release semantics.

- [ ] **Step 1: Add RED retry-decision tests**

Inject failure after classification and mutate the activity policy before
retry. Assert selected range, keep bitmap, first evidence, common crop, and
sparse candidate channels remain byte-for-byte equal. Add success recycling
tests proving the next new range receives a reset decision.

- [ ] **Step 2: Add RED outcome/protocol tests**

Round-trip `SkippedByPolicy` and written export fields through JSON. Assert:

```rust
assert!(export_start_frame >= start_frame);
assert_eq!(export_start_frame + export_frames, end_frame);
```

Keep old response deserialization compatible through serde defaults where
possible.

- [ ] **Step 3: Run RED tests**

Run: `cargo test --test dump_coordinator --lib control::tests`

Expected: compilation/assertion failures for decision ownership and new fields.

- [ ] **Step 4: Move decision storage into frozen transaction state**

Allocate one reusable decision at coordinator/session construction. Move it
logically into `FrozenTransaction` when a new epoch freezes; return it only
after successful commit/release or clear. Do not clear it on retryable or
indeterminate failure.

- [ ] **Step 5: Extend outcomes without changing consumed accounting**

Commit the original `range.end`; derive `frames` from the consumed range and
export fields from the frozen decision. Treat both skip variants as successful
cursor/loss commits. Keep `NoNewAudio` unchanged.

- [ ] **Step 6: Verify coordinator/control tests**

Run: `cargo test --test dump_coordinator --lib && cargo clippy --all-targets -- -D warnings`

Expected: retry, loss, clear, recovery, and protocol tests all pass.

- [ ] **Step 7: Commit**

```bash
git add src/dump.rs src/control.rs src/daemon.rs tests/dump_coordinator.rs
git commit -m "feat: freeze sparse export decisions"
```

---

### Task 5: Sparse two-pass persistence workspace

**Files:**
- Modify: `src/persistence_workspace.rs`
- Modify: `src/capture_runtime.rs`
- Modify: `tests/persistence_workspace.rs`
- Modify: `tests/export_wav.rs`

**Interfaces:**
- Consumes: valid or empty `FrozenExportDecision`, `ResolvedExportPolicy`, renderer APIs.
- Replaces: command/path-shaped `PrepareRequest::{Recall, Dump}` with a request containing command, policy, timestamp, and mutable frozen decision.
- Produces: `PreparedPersistence::{SkippedSilent, SkippedByPolicy, FileSet, AtomicDirectory}` and dense sparse `FilePlan`.

- [ ] **Step 1: Add RED sparse workspace tests**

Add the required four-channel, one-active, all-zero, non-finite, and sparse
split tests. Assert inactive/never channel writers are never opened by an
instrumented `WavIo`, output slots contain original channel indices, and
prepared file count equals retained channels × cropped split parts.

- [ ] **Step 2: Add RED common-range WAV tests**

Create staggered mic/guitar onsets and assert every retained WAV starts at the
same absolute crop, has identical frame count, preserves relative onset delay,
and contains the untrimmed tail. Assert consumed frozen range remains larger
than encoded range.

- [ ] **Step 3: Add RED preflight/allocation tests**

Assert malformed/colliding/overflow paths fail before staging directory or WAV
open. Assert maximum operation allocation addresses/capacities remain stable
and do not vary with selected frame count.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test persistence_workspace --test export_wav`

Expected: old all-channel one-pass preparation fails sparse/crop assertions.

- [ ] **Step 5: Implement classification before path/staging work**

Call the detector pass before `plan_paths` or `create_staging`. If the decision
is already valid, reuse it without traversing detector state. Return the typed
skip immediately with no files/directories.

- [ ] **Step 6: Densely plan retained outputs**

Calculate split count from `end - export_start`. For each retained original
channel, render every part's absolute boundaries into reusable staged/final
path slots, compare every final pair, set the writer's dense first-output
offset, and leave omitted writer slots closed.

- [ ] **Step 7: Stream only the common cropped range**

Start the second frozen traversal at `export_start_frame`, write only retained
channels, rotate each dense writer at shared part boundaries, finalize active
writers, and preserve exact 24-bit PCM behavior.

- [ ] **Step 8: Verify workspace/export tests**

Run: `cargo test --test persistence_workspace --test export_wav --test activity && cargo clippy --all-targets -- -D warnings`

Expected: sparse preparation, aligned crop, split, errors, and allocation tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/persistence_workspace.rs src/capture_runtime.rs tests/persistence_workspace.rs tests/export_wav.rs
git commit -m "feat: prepare sparse cropped wav sets"
```

---

### Task 6: Strategy-based publication and sparse recovery

**Files:**
- Modify: `src/export_wav.rs`
- Modify: `src/recovery.rs`
- Modify: `src/persistence_workspace.rs`
- Modify: `tests/export_wav.rs`
- Modify: `tests/recovery.rs`
- Modify: `tests/dump_coordinator.rs`

**Interfaces:**
- Consumes: `PreparedPersistence::{FileSet, AtomicDirectory}` and actual sparse `FilePlan`.
- Produces: strategy-named manifest v2 while accepting v1 recall/dump manifests.
- Produces: nested custom file-set publication and unchanged atomic timestamp-directory publication.

- [ ] **Step 1: Add RED nested/sparse publication tests**

Publish custom paths containing channel/part/frame directories. Assert only
retained files exist, `PublishedOutput.files` exactly matches filesystem paths,
all distinct parents are synchronized, and an existing final collision rejects
without overwrite.

- [ ] **Step 2: Add RED sparse recovery tests**

Construct interrupted manifests omitting channels and covering partial/complete
file sets. Assert complete sparse sets survive, partial owned sets roll back,
foreign replacements survive, and omitted channel paths are never inferred.
Retain tests reading existing version-1 manifest fixtures.

- [ ] **Step 3: Add RED atomic preset tests**

Use explicit timestamp-directory for both recall and dump requests and assert
one complete directory rename remains the publication boundary. Confirm custom
`directoryPattern = "{timestamp}"` uses file-set checkpoints instead.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test export_wav --test recovery --test dump_coordinator`

Expected: old command-shaped publishers/recovery reject new strategy/nested paths.

- [ ] **Step 5: Generalize prepared dispatch, not atomic implementation**

Rename prepared variants and dispatch by `PublicationStrategy`. Preserve the
existing atomic-directory code path as a distinct implementation. Update the
file-set publisher so hidden partials are adjacent to each nested final and
sync every unique parent plus required ancestors.

- [ ] **Step 6: Implement manifest v2 compatibility**

Serialize `FileSet`/`AtomicDirectory`, actual sparse entry count, and arbitrary
contained file-set paths. During deserialization map v1 `Recall`/`Dump` to the
new strategies. For file sets, require partial and final to share a parent;
for atomic directory, retain direct-child restrictions.

- [ ] **Step 7: Preserve identity-safe cleanup**

Reject symlink ancestors, track transaction-created parent identities, remove
only empty matching parents during rollback, and keep all unmarked/foreign
paths. Preserve indeterminate-publication handoff and directory sync ordering.

- [ ] **Step 8: Verify publication/recovery suites**

Run: `cargo test --test export_wav --test recovery --test dump_coordinator --test persistence_workspace && cargo clippy --all-targets -- -D warnings`

Expected: all sparse, nested, v1 compatibility, atomic, and identity tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/export_wav.rs src/recovery.rs src/persistence_workspace.rs tests/export_wav.rs tests/recovery.rs tests/dump_coordinator.rs
git commit -m "feat: publish sparse layout transactions"
```

---

### Task 7: Daemon command semantics and app output-root integration

**Files:**
- Modify: `src/daemon.rs`
- Modify: `src/control.rs`
- Modify: `src/control_server.rs`
- Modify: `tests/daemon_fake.rs`
- Modify: `tests/daemon_idle.rs`
- Modify: `tests/config_validation.rs`

**Interfaces:**
- Consumes: typed resolved policy, command-independent prepare request, strategy publishers, new outcomes.
- Produces: session-owned mutable activity policy and centralized command/layout resolution.
- Removes: `app_dump_dir()` and all `$HOME/.cache/lamb/out` construction.

- [ ] **Step 1: Add RED daemon app-layout tests**

Start a profile fake/test session with a temporary `export.outputDir`. Invoke
dump and assert every final path is beneath it. Cover explicit flat layout on
dump, explicit timestamp layout on recall, and omitted legacy-command layouts.

- [ ] **Step 2: Add RED response/file tests**

Assert written responses contain only existing sparse files and both consumed
and export ranges. Assert silent and policy skips are successful and contain no
paths.

- [ ] **Step 3: Run RED daemon tests**

Run: `cargo test --test daemon_fake --test daemon_idle`

Expected: app dump still uses the hardcoded home cache and handlers remain command-shaped.

- [ ] **Step 4: Store one resolved policy in `CaptureSession`**

Replace `output_dir` with a session policy lock whose layout/output root is
stable and whose activity thresholds can later be replaced. Build coordinator
decision storage from the session plan. Recovery scans the configured output
root and shared staging root according to possible strategy manifests.

- [ ] **Step 5: Route recall/dump through policy**

Handlers supply only `ExportCommand`, timestamp, and session policy. Resolve
legacy command defaults in `export_policy`; never concatenate command paths in
daemon/workspace. Remove `HOME` error branches from app dump.

- [ ] **Step 6: Verify daemon regressions**

Run: `cargo test --test daemon_fake --test daemon_idle --test config_validation --lib && cargo clippy --all-targets -- -D warnings`

Expected: app/legacy roots and layouts, responses, shared cursor, status, and lifecycle tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/daemon.rs src/control.rs src/control_server.rs tests/daemon_fake.rs tests/daemon_idle.rs tests/config_validation.rs
git commit -m "fix: resolve daemon exports from profile policy"
```

---

### Task 8: Startup-planned future-frame calibration capture

**Files:**
- Modify: `src/memory_plan.rs`
- Modify: `src/capture_runtime.rs`
- Modify: `src/capture_arena.rs`
- Create: `tests/calibration.rs`
- Modify: `tests/capture_arena.rs`
- Modify: `tests/memory_plan.rs`

**Interfaces:**
- Produces: `CalibrationCaptureRequest { channel, frames }` and borrowing `CalibrationLease` over completed preallocated samples/stats.
- Produces: `CaptureArena::calibrate_channel(request, timeout) -> Result<CalibrationLease<'_>>`.
- Keeps: ordinary capture command slot independently available for `status`.

- [ ] **Step 1: Add RED memory/capture tests**

Assert profile runtimes reserve maximum 30-second mono samples plus maximum
10-ms-hop RMS/peak slots, include both in `memory.max`, and legacy runtimes can
reserve zero sample frames. Assert buffer addresses do not change per request.

- [ ] **Step 2: Add RED future-only calibration tests**

Write frames before accepting calibration, then future frames with distinct
values. Assert the lease contains only future selected-channel samples, exact
frame count, sample rate, complete-window stats, and no change to active/frozen
range or coordinator cursor.

- [ ] **Step 3: Add RED responsiveness/failure tests**

While calibration waits, call arena status and require a prompt response. Cover
invalid channel, 1/30-second boundaries, timeout/cancel, ingress drops, worker
fault, non-finite sample, and second concurrent calibration rejection.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test calibration --test capture_arena --test memory_plan`

Expected: calibration slot/storage APIs do not exist.

- [ ] **Step 5: Add exact calibration plan components**

Extend runtime inputs with `maximum_calibration_seconds`. Compute sample and
maximum overlapping-window counts with checked arithmetic. Allocate and page-
materialize mono F32, RMS F32, and peak F32 buffers before backend activation.

- [ ] **Step 6: Implement an independent calibration slot**

Use explicit atomic state transitions and one capture-worker writer. Starting
a request records current dropped count and target future frames. Each consumed
ingress block still writes the active ring and also copies only the requested
channel into calibration storage. Complete publication uses release/acquire;
the returned lease prevents slot reuse until dropped.

- [ ] **Step 7: Accumulate detector-compatible windows on capture side**

Use two staggered 20-ms accumulators starting every 10 ms, append complete RMS
and peak values to fixed stat arrays, and record partial-window metadata. Mark
the capture unusable on non-finite values or dropped-frame delta.

- [ ] **Step 8: Verify capture/memory tests**

Run: `cargo test --test calibration --test capture_arena --test memory_plan --test pipewire_backend && cargo clippy --all-targets -- -D warnings`

Expected: future-only capture, responsiveness, bounded memory, and backend regressions pass.

- [ ] **Step 9: Commit**

```bash
git add src/memory_plan.rs src/capture_runtime.rs src/capture_arena.rs tests/calibration.rs tests/capture_arena.rs tests/memory_plan.rs
git commit -m "feat: capture bounded calibration samples"
```

---

### Task 9: Calibration identity, sample store, and atomic config persistence

**Files:**
- Create: `src/calibration.rs`
- Modify: `src/lib.rs`
- Modify: `src/profile.rs`
- Modify: `src/app_config.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `tests/calibration.rs`
- Modify: `tests/app_config.rs`

**Interfaces:**
- Consumes: `CalibrationLease`, threshold config structures, detector constants.
- Produces: `StableInputIdentity`, `CalibrationMetadata`, `CalibrationStore`, `PreparedCalibrationGeneration`.
- Produces: `derive_calibrated_threshold(stats) -> Result<f32>` and `save_config_atomic(path, candidate) -> Result<()>`.

- [ ] **Step 1: Add RED identity/staleness tests**

Assert stable ids are deterministic across port reorder, differ for
backend/device/name/source changes, and do not use array index. Test 30-day
boundary, detector/sample-rate/live-identity mismatch, missing/corrupt metadata
or sample, and manual threshold non-expiration.

- [ ] **Step 2: Add RED F32LE WAV/threshold tests**

Persist known mono floats and assert IEEE-float WAV format, exact samples,
sample rate/frame count, and no 24-bit conversion. Verify p95 complete-window
RMS +10 dB, zero floor −120 dBFS, and rejection outside `[-120, 0]`.

- [ ] **Step 3: Add RED generation transaction tests**

Prepare old and new generations; inject sample/metadata/file-sync/config-
rename/parent-sync failures. Assert previous config/generation remains
authoritative and live installation callback is not called. On success assert
config references the new generation and old/unreferenced generations are
removed or reported for safe startup cleanup.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test calibration --test app_config`

Expected: state/identity/atomic persistence APIs are absent.

- [ ] **Step 5: Implement stable identity and state root**

Canonicalize length-delimited backend/device/name/source fields and hash with
SHA-256. Resolve `$XDG_STATE_HOME/lamb/calibration`, falling back to
`$HOME/.local/state/lamb/calibration`; reject non-absolute environment roots.

- [ ] **Step 6: Implement bounded F32LE WAV and metadata**

Write the 44-byte IEEE-float mono WAV header and little-endian `f32` samples,
flush/sync before metadata publication, and serialize versioned JSON metadata
with identity, stats, detector version, threshold, frames, rate, and timestamp.

- [ ] **Step 7: Implement immutable generation preparation**

Create a unique generation with `create_new`/`create_dir`, synchronize files and
directories, validate it by reopening metadata/header, and expose a prepared
handle. It becomes authoritative only after candidate config durably references
its id.

- [ ] **Step 8: Implement atomic config replacement and cleanup**

Serialize candidate TOML to an adjacent unique file, preserve target mode,
flush/sync, verify target identity, atomically replace, and fsync parent. Install
runtime policy only after this returns. Remove failed/unreferenced generations
identity-safely; scan startup state against config references.

- [ ] **Step 9: Verify calibration/config tests**

Run: `cargo test --test calibration --test app_config --test config_validation && cargo clippy --all-targets -- -D warnings`

Expected: identity, WAV, staleness, generation, and atomic failure tests pass.

- [ ] **Step 10: Commit**

```bash
git add src/calibration.rs src/lib.rs src/profile.rs src/app_config.rs Cargo.toml Cargo.lock tests/calibration.rs tests/app_config.rs
git commit -m "feat: persist calibration generations"
```

---

### Task 10: Daemon-owned threshold command family

**Files:**
- Modify: `src/control.rs`
- Modify: `src/control_server.rs`
- Modify: `src/daemon.rs`
- Modify: `src/main.rs`
- Create: `tests/threshold_cli.rs`
- Modify: `tests/daemon_idle.rs`
- Modify: `tests/daemon_fake.rs`
- Modify: `tests/calibration.rs`

**Interfaces:**
- Produces: typed `ThresholdRequest::{Calibrate, Set, Show, Reset}` and `ThresholdReport` response.
- Produces: CLI `threshold calibrate|set|show|reset` with optional socket and default control path.
- Consumes: capture calibration lease, generation store, candidate config atomic save, active policy replacement.

- [ ] **Step 1: Add RED clap/client tests**

Invoke the binary for all command forms. Assert default 5 seconds, accepted
1/30 boundaries, rejected 0/31, required profile/channel/dbfs, normal default
socket resolution, and `--socket` override.

- [ ] **Step 2: Add RED protocol/routing tests**

Round-trip request/report JSON. Assert set/reset/calibrate/show enter the bounded
operation lane, ordinary status responds while calibrate blocks, and a legacy
daemon returns the exact unsupported message.

- [ ] **Step 3: Add RED handler transaction tests**

Cover inactive profile set/reset/show, active set updating only future decisions,
pending frozen retry immutability, active calibration profile/name/source/live
identity checks and exact errors, successful state+config commit, disk failure
retaining previous live policy, and reset clearing sample reference/state.

- [ ] **Step 4: Run RED tests**

Run: `cargo test --test threshold_cli --test daemon_idle --test daemon_fake --test calibration`

Expected: threshold clap/protocol/handlers do not exist.

- [ ] **Step 5: Add typed protocol and CLI clients**

Use a nested serde-tagged threshold request/report. Keep threshold fields
optional on general `ControlResponse` for compatibility. Resolve omitted socket
from `XDG_RUNTIME_DIR/lamb/control.sock` with a clear error when unavailable.

- [ ] **Step 6: Implement set/show/reset candidate transactions**

Under the operation lane, clone daemon config, resolve the stable named channel,
mutate a candidate, fully validate, atomically save, then replace daemon config.
For an active matching profile, replace only mutable resolved activity policy;
do not restart backend/session/coordinator. Reset removes state after config
commit; manual set may retain prior calibration sample reference.

- [ ] **Step 7: Implement live calibration handler**

Require exact active/capturing profile and stable name/source. Request future
capture, validate lease identity/statistics, derive threshold, prepare state
generation, build candidate config, commit config, install policy, then clean
old generation. Every pre-commit failure drops the new generation and leaves
previous policy/config/sample authoritative.

- [ ] **Step 8: Implement coherent show reports**

Report stored detector/source/value/timestamp/age/sample and live active status,
identity match, validity/staleness reason, effective threshold, and detector
version. Inactive profile live identity is `not currently resolved`.

- [ ] **Step 9: Verify command integration**

Run: `cargo test --test threshold_cli --test daemon_idle --test daemon_fake --test calibration --lib && cargo clippy --all-targets -- -D warnings`

Expected: CLI, protocol, atomic handlers, status responsiveness, and retry policy tests pass.

- [ ] **Step 10: Commit**

```bash
git add src/control.rs src/control_server.rs src/daemon.rs src/main.rs tests/threshold_cli.rs tests/daemon_idle.rs tests/daemon_fake.rs tests/calibration.rs
git commit -m "feat: add daemon threshold commands"
```

---

### Task 11: Documentation and complete regression verification

**Files:**
- Modify: `README.md`
- Modify: task-touched tests/docs only for verified corrections

**Interfaces:**
- Verifies every preceding interface; introduces no new runtime behavior.

- [ ] **Step 1: Document omission and detector distinctions**

Explain whole-export exact-zero compatibility, per-channel exact-zero,
windowed detector v1, threshold provenance, active/inactive/ambiguous, fail-open
missing/stale state, per-port modes, common crop, and full consumed accounting.

- [ ] **Step 2: Document every layout with rendered examples**

Show legacy command defaults, flat-detailed, atomic timestamp-directory, custom
nested patterns, token/part semantics, path validation, and preview ownership.

- [ ] **Step 3: Document threshold commands and state**

Show calibrate/set/show/reset, active-profile calibration requirement, 1–30
seconds/default 5, `$XDG_STATE_HOME` sample/metadata, latest-generation behavior,
30-day staleness, and no topology/cursor effects.

- [ ] **Step 4: Run focused suites**

Run:

```bash
cargo test --test activity \
  --test export_policy \
  --test persistence_workspace \
  --test export_wav \
  --test recovery \
  --test dump_coordinator \
  --test calibration \
  --test threshold_cli \
  --test daemon_fake \
  --test daemon_idle
```

Expected: all focused tests pass.

- [ ] **Step 5: Run required Rust gates**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Expected: each exits 0 with no warnings/failures.

- [ ] **Step 6: Run Nix checks/build**

Run:

```bash
nix flake check
nix build "path:.#lamb"
```

Expected: all declared checks and the dirty-tree package build pass.

- [ ] **Step 7: Inspect scope and artifacts**

Run:

```bash
git diff --check
git status --short
git diff --stat
```

Expected: only scoped source/tests/docs/lockfile changes; no WAV, calibration,
manifest, staging, result, or state artifacts are tracked.

- [ ] **Step 8: Commit final documentation/fixes**

```bash
git add README.md
git commit -m "docs: explain export activity policies"
```

## Plan self-review

- **Spec coverage:** Tasks 1–2 cover typed config/layout/preview; Tasks 3–5 cover exact-zero, algorithmic detection, disposition, crop, memory, and immutable retry; Tasks 6–7 cover strategy publication, recovery, command semantics, and app output root; Tasks 8–10 cover preallocated future capture, retained sample/state, staleness, atomic config, and all threshold commands; Task 11 covers required documentation and every Rust/Nix gate.
- **Type consistency:** `ResolvedExportPolicy`, `ResolvedActivityPolicy`, `FrozenExportDecision`, `PublicationStrategy`, `PreparedPersistence`, `CalibrationLease`, and threshold protocol names are introduced before their consumers and retained throughout later tasks.
- **Safety review:** No task moves filesystem work into capture, changes callback topology, reclassifies a valid retry, publishes omitted files, routes timestamp-directory through custom publication, or installs live threshold state before durable config.
- **Placeholder review:** Every task names exact files, interfaces, RED command, implementation action, verification command, and commit boundary; no deferred detector parameter model is accidentally introduced for reserved variants.
