# Preallocated Persistence Runtime Design

## Scope

This patch follows the transactional incremental recall/dump implementation in
commit `91f0a11`. It addresses:

1. capture drops caused by pinning the active ring during stabilization;
2. recording-size heap allocation during persistence;
3. an unresponsive synchronous control server during export;
4. crash leftovers and incomplete durability around staging/publication;
5. loss hidden by `NoNewAudio` after a clear; and
6. capture-drop counts being separate from persistence loss reporting.

Timestamp uniqueness and configurable noise detection remain outside scope.

## Invariants

- For every absolute source frame admitted by capture, LAMB eventually accounts
  for it exactly once as published, intentionally silent/cleared,
  retention-lost, or capture-dropped, without filesystem activity ever
  blocking the realtime producer.
- `recall` and `dump` share one monotonic `committed_until` source-frame cursor
  per capture session.
- A successful write or intentional exact-zero discard consumes its complete
  half-open range. Failures consume nothing.
- Recall keeps its existing flat detailed filenames under configured
  `outputDir` and prepares files under `/tmp/LAMB/staging/<unique-id>/`.
- Dump keeps atomic timestamp-directory publication with simple channel names.
- Final publication never overwrites an existing path.
- The transaction mutex spans selection, stabilization, silence handling,
  publication, and cursor commit. Capture never acquires it.
- No ring/capture lock is held during silence scanning, WAV encoding, copying,
  synchronization, or rename.
- Exact-zero remains the only silence definition.
- All RAM proportional to retention size is calculated, allocated, and page
  touched before capture starts.

## Capture-session architecture

Each session owns a `CaptureArena` containing two complete ring epochs:

```text
CaptureSession
├── CaptureIngress
│   └── fixed page-touched SPSC audio-block slots
├── CaptureWorker
│   └── sole mutable owner of both ring epochs
├── CaptureArena
│   ├── epoch[0]: full retention ring
│   ├── epoch[1]: full retention ring
│   ├── active_epoch index
│   ├── epoch base-frame metadata
│   └── one preallocated command/result slot
├── PersistenceCoordinator
│   ├── transaction mutex
│   ├── committed_until
│   ├── pending frozen transaction
│   └── acknowledged loss counters
├── PersistenceWorkspace
└── lifecycle/control state
```

Capture callbacks only copy complete frames into the preallocated SPSC ingress.
They perform no allocation, filesystem work, ring locking, or persistence
locking. Queue saturation accounts the unqueued remainder immediately as
`capture_dropped_frames`. Callback blocks larger than one slot are split across
fixed slots without allocation.

One prestarted capture worker drains ingress slots and is the sole mutable owner
of both ring epochs. It assigns absolute source positions, writes the active
ring, and processes freeze/clear/release commands between audio blocks. This
removes callback/epoch-switch races and makes every successfully enqueued frame
belong to exactly one epoch.

At persistence selection:

1. Hold the transaction mutex.
2. If a failed frozen transaction exists, retry it unchanged.
3. Otherwise capture the current absolute write end.
4. If there is no new recoverable audio, return `NoNewAudio` with pending loss
   counters.
5. Send a freeze command through the arena's single preallocated command slot.
6. The capture worker completes its current audio block, captures the absolute
   end, resets the standby epoch with that base, and switches active epochs.
7. The worker returns the frozen epoch identity through the preallocated result
   slot.
8. Treat the former active epoch as an immutable `FrozenCaptureEpoch` containing
   the exact selected source range.

The frozen epoch itself is the owned stabilized snapshot. It is no longer the
rolling ring and requires no pins or recording-size copy. Persistence streams
from it with fixed scratch buffers while capture runs exclusively in the other
epoch.

On successful publication or exact-zero discard, advance `committed_until`,
reset the frozen epoch, and return it as standby. On failure, keep it unchanged
for exact retry while capture continues in the active epoch. Newer active audio
is not added to the failed range.

If active capture wraps while a failed frozen transaction awaits retry, normal
retention rules apply. After the failed range succeeds, the next operation
reports any active-epoch retention loss.

## Absolute frame coordinates

Individual rings retain local frame coordinates. Each epoch has an immutable
absolute base while active or frozen:

```text
absolute_frame = epoch_base + local_frame
```

Resetting a standby ring sets local head/clear state to zero and assigns the new
base. This avoids requiring local chunk offsets to align with the global source
frame number. Cursor and control-protocol ranges remain absolute and monotonic.

## Startup memory planning and commitment

`SessionMemoryPlan` uses checked arithmetic and reports each component:

- two complete ring sample stores;
- both rings' chunk descriptors, synchronization objects, and index storage;
- one maximum-chunk interleaved scratch buffer;
- fixed per-channel PCM/WAV I/O buffers;
- maximum split-part, path, and manifest slots derived from retention frames,
  split threshold, channel count, and configured names/paths;
- one fixed capture-ingress queue and slot metadata;
- one capture-worker stack budget;
- one bounded persistence-operation queue;
- one persistence-worker stack budget; and
- conservative fixed runtime metadata.

The plan computes:

```text
committed_bytes = sum(all allocated components)
required_with_headroom = ceil(committed_bytes * memory.headroom)
```

When `memory.max` is configured, startup rejects
`required_with_headroom > memory.max`. Allocation happens only after validation
and before backend activation.

Every sample arena, scratch buffer, byte buffer, metadata slot arena, bounded
queue backing store, and explicit reserve is page-touched with volatile writes
at page intervals. Ring reset reuses allocations. Persistence performs no heap
allocation proportional to selected frame count. Small protocol values also use
pre-sized reusable slots so export-time allocation failure is not part of the
normal path. Accounting uses owned arenas and conservative page-rounded
metadata/allocator reserves rather than assumptions about private `Arc` layout
or `Vec` excess capacity.

The OS can still reject file opens, disk writes, or thread creation at session
startup. Those remain explicit startup or persistence I/O errors rather than
late recording-size RAM failures.

## Streaming WAV preparation and silence handling

The frozen epoch is traversed chronologically with one fixed interleaved sample
scratch buffer. A reusable `StreamingWavTransaction` owns fixed per-channel
encoding buffers and file/path slots.

For each block it:

1. copies local frozen-ring samples into the fixed scratch buffer;
2. checks source `f32` values for exact-zero silence;
3. converts and writes each configured channel to transaction-private staged
   WAVs; and
4. rotates split parts at existing frame boundaries.

WAV headers know the selected frame count and split plan before writing. No full
channel vectors are created.

If the complete source range is zero, all temporary WAVs are finalized, closed,
and removed without final publication, then the cursor advances. Any nonzero
sample publishes the complete channel set. Temporary WAV creation during the
scan is intentional and approved; no final output is exposed for silence.

## Control concurrency and lifecycle ordering

Keep the existing single control accept/parser path. Add only one fixed,
prestarted persistence-operation worker and one bounded queue whose memory is
part of `SessionMemoryPlan`.

- The control path parses each request with a bounded read timeout.
- Status and other read-only requests are handled directly.
- Recall, dump, clear, and capture/session lifecycle requests transfer the
  request and connected stream to the operation worker; that worker writes the
  eventual response, allowing the accept path to continue immediately.
- Queue saturation returns a deterministic busy response. No per-connection or
  per-operation threads are spawned.
- Additional recall/dump requests cannot select duplicate ranges.
- `clear` closes the persistence admission gate, waits for the active
  transaction, executes clear in cursor order, then reopens admission.
- `stop` and `stop-capture` reject new persistence requests, let the active
  transaction finish, then stop capture and the operation worker cleanly.
- Status remains available while export runs.

App capture replacement/reload drains and stops the old session before creating
and activating a new preallocated session. Session workers own the paired arena,
coordinator, and backend lifecycle so a stopped/replaced ring cannot be mixed
with another session's cursor.

## Loss accounting

Persistence outcomes expose:

```text
retention_lost_frames
cleared_frames
capture_dropped_frames
lost_frames = saturating sum of the three
```

`lost_frames` remains for protocol compatibility. The three cause-specific
fields are added to `Written`, `SkippedSilent`, and `NoNewAudio`.

Coordinator state tracks the last acknowledged cumulative dropped-frame count
and pending retention/clear loss. A successful outcome—including
`NoNewAudio`—acknowledges and reports each loss once. A failed operation
acknowledges nothing, so retry includes all still-unreported loss.

`clear` runs in transaction order. It computes already-lost retention frames,
counts remaining recoverable uncommitted frames as cleared, advances the logical
cursor to the clear head, resets available epochs, and stores the loss breakdown
for the next successful persistence outcome. Thus clear followed by no new audio
returns `NoNewAudio` with nonzero `cleared_frames`.

Capture drops remain outside source-frame coordinates because no ring frame was
written for them. They are reported separately and included only in the
compatibility total.

## Transaction manifests, recovery, and durability

Only new manifest-backed transactions are automatically recovered. Existing
unmarked legacy staging artifacts are left untouched.

### Recall

`/tmp/LAMB/staging/<unique-id>/` contains staged WAVs and a versioned manifest.
The manifest records the configured output directory, expected final basenames,
hidden adjacent partials, and transaction-owned device/inode identities as
publication progresses.

After every staged WAV is finalized and closed:

1. copy to a unique hidden partial inside configured `outputDir`;
2. flush and `sync_all` the partial;
3. publish with no-overwrite adjacent rename;
4. update and synchronize the manifest;
5. after every final is present, synchronize `outputDir`;
6. commit the cursor; and
7. remove the manifest/staging directory best-effort.

Recovery preserves a transaction when every expected final exists with its
recorded identity. If only a subset exists, it removes transaction-owned finals,
partials, and staging artifacts best-effort. Replaced foreign identities are
never intentionally removed.

### Dump

Dump uses an adjacent hidden directory and sibling manifest under the dump
parent. Every WAV and the temporary directory are synchronized before the
no-overwrite directory rename. The parent directory is synchronized after
rename and before cursor commit.

Recovery removes an owned hidden temporary directory when publication never
occurred. If the complete final directory exists with all recorded identities,
it is preserved and the manifest is removed. No unmarked path is recovered or
deleted.

Manifest updates use temporary-file plus no-overwrite/replace-within-owned-name
publication followed by parent-directory synchronization. Recovery validates
version, ownership, path containment, and identities before acting.

## Error handling

- Memory-plan overflow, limit violation, allocation failure, page-touch failure,
  worker startup failure, or backend startup failure aborts session creation.
- Stabilization/switch failures leave the cursor unchanged and keep capture on a
  valid active epoch.
- WAV, manifest, sync, copy, or rename failure leaves the frozen transaction for
  retry and performs identity-aware cleanup best-effort.
- A panic or poisoned transaction lane faults that capture session and rejects
  further persistence until restart; capture status exposes the fault.
- Stop drains only the active transaction; queued but unstarted persistence
  requests receive a shutting-down response.

## Testing strategy

### Memory and allocation

- Verify checked component-by-component memory calculations.
- Verify two rings, workspace, queues, and stack budgets are included.
- Verify `memory.max` rejects before backend activation.
- Instrument allocations and assert no frame-count-proportional allocation
  occurs during maximum-size persistence.
- Verify page-touch routines cover every allocated page and run at startup.

### Capture continuity and epoch switching

- Capture while a maximum retained epoch is scanned and encoded; assert no
  export-induced dropped frames.
- Switch with an in-flight callback and verify every frame belongs to exactly
  one epoch.
- Repeated alternating epochs preserve absolute half-open ranges.
- Failed frozen export retries exactly while active capture continues.
- Active wrap during failed retry reports subsequent retention loss.

### Control concurrency

- Status responds while persistence is deliberately blocked.
- Concurrent recall/dump never duplicate ranges.
- Clear waits for active publication, then affects only later ranges.
- Stop rejects new persistence, drains the active operation, and exits.
- Queue capacity cannot create threads or unbounded allocations.

### Recovery and durability

- Complete interrupted recall is preserved.
- Partial flat recall is rolled back by identity.
- Complete dump directory is preserved; hidden incomplete dump is removed.
- Foreign replacement paths and unmarked legacy artifacts are untouched.
- File and parent-directory synchronization calls are exercised through an
  injectable filesystem boundary.

### Loss reporting

- Retention, clear, and capture-drop losses are reported separately and summed.
- `NoNewAudio` carries and acknowledges pending losses exactly once.
- Failures do not acknowledge cursor or loss counters.
- Protocol deserializes older responses containing only `lost_frames`.

## Compatibility

- Existing recall filenames and flat output layout remain unchanged.
- Dump timestamp-directory layout remains unchanged.
- Existing `lost_frames` remains available as a compatibility total.
- PipeWire/JACK capture behavior and sample format remain unchanged; backends
  write through the new dual-epoch capture arena.
- NixOS options remain unchanged. Higher deterministic memory requirements may
  cause configurations with a tight `memory.max` to fail early with a detailed
  component report rather than fail during persistence.
