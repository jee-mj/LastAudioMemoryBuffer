# Preallocated Publication and Response Completion Design

Date: 2026-08-26

## Status and authority

This design closes the final-review defect found after completing
`2026-08-26-export-policy-activity-calibration-design.md`. That design remains
binding. In particular, memory proportional to split capacity must be planned
and allocated at session startup, and no live allocation may grow with the
selected export duration.

The correction is wire-compatible and introduces no user-visible command,
layout, omission, crop, recovery, or threshold behavior.

## Root cause

`PersistenceWorkspace::prepare` already renders sparse paths and WAV parts into
startup-owned slots. Prepared publication then defeats that boundary by copying
the plan into heap collections:

- `planned: Vec<(PathBuf, PathBuf)>` clones two paths per output;
- file-set rollback retains `partials`, `created_finals`, and open files in
  output-sized vectors;
- parent synchronization constructs and deduplicates a path vector;
- atomic publication constructs another final-path vector;
- durable recovery retains an owned `PublishedOutput` and clones it at failure
  checkpoints; and
- `PublishedOutput`, `DumpOutcome`, and the daemon response retain an owned
  `Vec<PathBuf>` until JSON serialization.

The number of outputs is retained channels multiplied by WAV split parts. Split
parts increase with the selected source duration, so these are precisely the
duration-scaled allocations forbidden by the binding design.

The original preallocated runtime intentionally kept these non-realtime vectors
outside its arena. The later sparse-policy design strengthened the invariant to
cover split capacity and canonical persistence paths, but preparation was
corrected without carrying that ownership boundary through publication and
response delivery.

## Scope

The strict boundary is the daemon's prepared persistence transaction:

```text
frozen epoch
  -> classify/render/prepare
  -> publish/recover
  -> commit cursor and loss accounting
  -> serialize the written response to the accepted Unix stream
  -> reclaim publication scratch
```

No output-count-sized collection may be allocated in that path. Fixed-count
errors, status values, OS file handles used one at a time, and filesystem I/O
remain outside realtime capture and may fail normally.

Legacy snapshot helper APIs (`export_snapshot_wav`, `publish_recall`, and
`publish_dump`) remain allocating compatibility surfaces. The daemon and
`CaptureSession` must not call them. They are not the prepared persistence path
governed by this correction.

The persistence CLI will consume a written response incrementally instead of
retaining the complete file list. General `send_request` remains an owned,
wire-compatibility API for status, threshold commands, and external callers.

Canonical prepared-policy construction also requires `outputDir` and every
rendered final path to be valid UTF-8. This is checked with the existing lexical
and capacity validation before staging is created. Pattern/profile/channel/
timestamp values are already UTF-8 strings; this closes the remaining
programmatic non-UTF-8 `PathBuf` root case before any durable side effect.

## Alternatives considered

### Selected: workspace-backed publication plus borrowed response delivery

Keep the canonical plan in `PersistenceWorkspace`, use fixed publication
scratch for all mutation and recovery bookkeeping, expose a borrowed completed
output view, and serialize that view before workspace reuse.

This follows the existing ownership architecture and makes startup memory
accounting authoritative.

### Rejected: startup-owned movable result lease

A pool of preallocated path vectors could move through publisher, coordinator,
and daemon and return on drop. It duplicates the canonical path arena, makes
workspace admission depend on lease return, and creates difficult failure and
shutdown ownership cases.

### Rejected: omit file paths from the response

Reporting only a directory/count/manifest would remove result ownership but
would break the existing control protocol and the requirement that a written
response list exactly the retained files which exist after publication.

## Startup-owned publication scratch

`SessionMemoryPlan` and `PersistenceWorkspace` gain an explicit publication
scratch component. Every allocation is validated, page-accounted, allocated,
and touched before backend activation.

The scratch contains:

- one current unjournaled-artifact slot containing a path coordinate, identity,
  and artifact kind;
- one fixed array of directory-sync coordinates, sized to the existing
  worst-case manifest-directory capacity;
- two NUL-terminated component buffers of `maximum_path_bytes + 1` bytes for
  descriptor-relative `openat`, `mkdirat`, and `renameat2` calls; and
- reusable fixed path slots for adjacent partial and manifest paths.

The existing path arena has five fixed slots and currently assigns only the
transaction root, staging parent, and final root. The two remaining fixed slots
become the adjacent-partial and manifest-path scratch. They retain the existing
`maximum_path_bytes` bound and require no extra path allocation.

Publication allocation addresses join the existing workspace-address
regression surface. Exact startup accounting includes allocator/page overhead
for every new buffer and array.

## Borrowed canonical file plan

`FilePlan` stops borrowing the complete workspace. It becomes a read-only view
over disjoint output and path slices plus the active output count. An
`OwnedTransactionArtifacts` split-view method returns, in one borrow:

- the immutable file plan;
- mutable manifest entry/directory/path/serialization arenas; and
- mutable publication scratch.

This lets publication iterate canonical staged/final paths while mutating only
disjoint preallocated scratch. No `PathBuf` plan copy is required.

The plan remains dense by retained channel then part, preserves original channel
indexes, and remains immutable for the complete frozen retry.

## File-set publication

File-set publication uses the manifest and one current-artifact slot as its
only rollback authority.

1. Iterate the borrowed file plan to reject existing finals.
2. Build all manifest entries directly from borrowed staged/final paths.
3. Preflight every final parent using descriptor-relative traversal.
4. Record transaction-created directory intents before creation and identities
   after creation exactly as today.
5. Persist and synchronize the prepared manifest before visible file mutation.
6. For each output, build its adjacent partial in reusable path scratch, create
   it with the reusable C-string buffer, and place its identity in the one
   current-artifact slot.
7. Copy, flush, and synchronize that partial; durably record its identity.
8. Rename no-overwrite to the final, update the current slot to the final path,
   and durably record final identity/phase before clearing the current slot.
9. Reopen and synchronize finals one at a time from the canonical plan; no open
   file collection survives an iteration.
10. Synchronize each distinct final parent/required ancestor from fixed
    directory-sync coordinates.
11. Mark and verify the manifest complete.

At most one artifact can exist without a durable manifest identity because
publication is serial. If an in-process failure occurs in that interval, the
fixed current-artifact slot supports identity-safe cleanup. Earlier artifacts
are already journaled. A crash in that interval remains conservative exactly as
today: recovery never removes an identity-unknown path.

Any failure after the prepared manifest is durable is classified as durable.
Recovery owns the result; it is never converted back to a retryable failure by
an allocation or response error.

### Parent synchronization

During parent preflight, every unique final-parent/ancestor coordinate is added
to the fixed sync array using bounded linear duplicate detection. A coordinate
is `(entry_index, UTF-8 prefix_len)` into an already stored final path, so no
path bytes are duplicated. Transaction-created directories continue to use the
separate manifest rollback journal.

The sync phase resolves each coordinate beneath the trusted output-root
descriptor and synchronizes it. Existing and newly-created parents are each
covered; symlink/no-follow and root-identity checks remain unchanged.

## Atomic timestamp-directory publication

Atomic publication also iterates the borrowed file plan directly:

1. reject an existing final directory;
2. build the sparse manifest from borrowed staged/final paths;
3. synchronize each staged file one at a time;
4. synchronize the staging directory;
5. persist the prepared manifest;
6. rename the complete staging directory no-overwrite;
7. synchronize the output parent;
8. mark and verify the manifest complete.

No `planned`, `final_files`, or cloned `PublishedOutput` collection is created.
The all-at-once visibility boundary is unchanged.

## Compact indeterminate recovery

`IndeterminatePublication` becomes a fixed descriptor containing transaction
kind and workspace transaction identity. It owns no artifact or output paths.
The associated workspace retains its canonical plan, manifest arenas, fixed
paths, and optional current-artifact slot until recovery resolves.

Recovery validates that the descriptor belongs to the same workspace
transaction, then:

- resolves the fixed current artifact identity-safely;
- runs existing manifest recovery from workspace-owned paths and arenas;
- returns `Complete`, `RolledBack`, or `Pending` without constructing an owned
  output; and
- preserves every slot on `Pending`.

For in-process recovery, the original workspace output/path slots remain intact
while manifest parsing uses only the disjoint manifest arenas. `Complete`
therefore marks those retained canonical slots ready for borrowed delivery; it
does not reconstruct or own paths. The completed view is derived from the
retained output count, final root, and ordered output slots, and those slots stay
valid until response serialization and completion finalization end.

Startup recovery after daemon restart is different: no client request, frozen
epoch, or in-memory output plan survives, so startup only completes/rolls back
the durable transaction and reports recovery diagnostics. It never fabricates
a prior command response.

`RolledBack` clears publication scratch and leaves the exact in-process frozen
decision/range retryable.

## Borrowed committed outcome

Prepared publication returns only one of:

```text
Published
RetryableFailure(error)
Indeterminate(error, fixed recovery descriptor)
```

After `Published` or recovered `Complete`, the coordinator commits cursor/loss
state and releases its state lock while the session still owns the workspace
lock. It then invokes a caller-supplied completion visitor with:

- consumed and encoded frame ranges;
- loss counters and duration;
- a borrowed output-directory path; and
- an exact-size iterator over borrowed final paths.

Skip and no-new-audio outcomes use the same visitor with no output iterator.
Test-only/legacy adapters may collect the borrowed view into owned vectors, but
the production daemon path must not.

The prepared production APIs are changed accordingly: `CaptureSession` and
`DumpCoordinator` expose an internal delivery/visitor operation rather than
returning `DumpOutcome::Written`, and prepared `publish_prepared` returns only
the marker/error enum above. Recall/dump handlers call this visitor operation
directly. They never construct `PublishedOutput`, owned `DumpOutcome::Written`,
`PersistenceOutcomeResponse::Written`, or an owned `ControlResponse` containing
files. Existing owned types remain only for legacy snapshot APIs, public wire
deserialization, and explicitly allocating test adapters.

The visitor result is a delivery result, not a persistence result. A JSON or
socket write failure after durable publication never restores the frozen range,
changes the committed cursor, or republishes files. Workspace cleanup and frozen
release still run before the transport error is surfaced to daemon diagnostics.

An internal completion guard owns the committed frozen transaction and mutable
workspace finalization obligation after coordinator state commit. Explicit
finalization performs completed-manifest/staging cleanup, resets publication
scratch, releases the frozen epoch, and records an existing pending release when
the bounded release attempt fails. `Drop` executes the same idempotent path if
the delivery visitor returns early or unwinds. Thus visitor success, transport
error, and panic cannot skip or duplicate cleanup/release. Cleanup/release
errors remain post-commit diagnostics and cannot reclassify publication.

## Streaming wire response

The JSON object shape and all field names remain unchanged.

The daemon's operation worker special-cases recall/dump delivery through the
borrowed completion visitor. A borrowed serializable response writes directly
to `UnixStream` with `serde_json::Serializer`; the `files` sequence pulls each
validated UTF-8 path from the canonical file-plan iterator. It never constructs
an intermediate response `String`, `Vec<PathBuf>`, or per-file owned path.

Non-persistence responses continue through the existing owned
`ControlResponse` writer because their size does not depend on export duration.

The CLI's recall/dump path uses a streaming deserialization seed. Scalar fields
are retained in fixed local state. One reusable path/JSON-unescape buffer is
allocated once to the fixed control-path maximum and cleared between elements;
each file string is formatted directly to the output writer from that buffer.
No new `String`, `PathBuf`, vector slot, or other heap allocation occurs per
file. The general owned `ControlResponse` deserializer remains wire-compatible
for API callers and compatibility tests.

## Ordering and concurrency

- The operation lane remains the sole mutating-command serializer.
- Status remains directly routed and responsive.
- The workspace mutex remains held through borrowed response serialization so
  no later export can reuse path slots early.
- The coordinator state mutex is released before response delivery, allowing
  response status construction without recursive locking.
- Publication/cursor commit happens before delivery. Transport failure cannot
  make an already-visible transaction retryable.
- The completion guard makes workspace cleanup and frozen release exactly once
  regardless of delivery success, early return, or unwind.
- Stop/cancellation ordering and calibration behavior are unchanged.

## Error handling

- Capacity overflow is rejected at startup or before publication mutation.
- A fixed scratch overflow is an invariant/validation error and consumes
  nothing.
- A pre-manifest failure is retryable after staging cleanup.
- A post-manifest failure is indeterminate until identity-safe recovery returns
  complete or rolled back.
- A response delivery failure is recorded as a control transport error after
  persistence commit; it is never a persistence retry signal.
- Cleanup never removes foreign or identity-unknown artifacts.

## Test strategy

Tests are added RED before implementation.

### Allocation proofs

- Compare a one-output export with the startup maximum sparse/split output count
  across preparation, publication, coordinator commit, and response
  serialization for both file-set and atomic-directory strategies.
- Require equal allocation count and bytes in the daemon transaction boundary,
  with no growth from output count.
- Require stable workspace allocation addresses across success, retryable
  failure, indeterminate recovery, and reuse.
- Independently verify every new memory-plan component and exact-limit/one-byte-
  short `memory.max` behavior.

### Transaction and recovery proofs

- Preserve every existing publication checkpoint result and frozen retry.
- Exercise failure with one current unjournaled partial/final and prove only the
  matching inode is removed.
- Prove complete recovery returns the original sparse borrowed path sequence and
  rollback returns no paths.
- Prove nested existing/created parents are synchronized from fixed coordinates
  without duplicates and with existing crash-safe ordering.
- Prove atomic-directory visibility and no-overwrite remain unchanged.

### Response proofs

- Compare streamed JSON byte-for-byte at the semantic JSON level with the
  existing owned response for written, skipped, policy-skipped, and no-new-audio
  outcomes.
- Prove only retained existing files appear and ordering is unchanged.
- Inject a response-write failure after publication and prove cursor commit,
  exact-once cleanup, frozen release, and no republish on the next command.
- Prove the persistence CLI consumes a maximum file list without retaining an
  owned vector while existing public deserialization remains compatible.

### Gates

Run focused workspace/publication/recovery/coordinator/control/daemon suites,
all-target tests, strict Clippy, formatting, diff hygiene, `nix flake check`, and
the dirty `path:.#lamb` package build. Repeat the independent whole-plan review
against the original binding design plus this correction.

## Success criteria

The correction is complete when:

1. the current daemon's prepared persistence and response path has no live heap
   allocation whose count or retained bytes grows with sparse/split output count;
2. split-capacity publication scratch is present in startup memory accounting;
3. retry, recovery, durability, no-overwrite, identity, cursor, and loss
   semantics are unchanged;
4. written JSON remains wire-compatible and reports exactly existing retained
   files; and
5. all focused, complete Rust, Nix, and independent review gates pass.
