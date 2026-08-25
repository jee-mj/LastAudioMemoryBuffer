# Export Policy, Activity Detection, and Calibration Design

Date: 2026-08-26

## Scope

This change makes channel retention, activity detection, output layout, and
threshold management typed core policy. It fixes three existing mismatches:

1. profile recall and dump derive different output roots and layouts in daemon
   command handlers;
2. persistence decides silence with one transaction-wide `any_nonzero` flag
   and therefore publishes silent peer channels; and
3. path construction is owned by persistence/daemon details rather than one
   reusable core renderer.

The design also introduces the first algorithmic detector and a bounded
calibration command family. This is required because a modern profile with
omitted activity fields defaults to the algorithmic detector, while exact-zero
is an explicit simple/compatibility policy.

The change does not add a GUI, waveform display, realtime meter, trailing-silence
trim, adaptive detector changes during an export, spectral gating, or capture
topology changes.

## Existing architecture and constraints

LAMB has one capture-session `DumpCoordinator`, one immutable frozen epoch for
the selected source range, and one preallocated `PersistenceWorkspace`.
Successful publication or intentional omission advances a shared recall/dump
cursor; failure retains the frozen epoch for exact retry. Publication is
manifest-backed, identity-checked, no-overwrite, and crash-recoverable.

The capture callback only writes to bounded ingress. The capture worker owns
the rings. Silence scanning, WAV preparation, path rendering, manifests, and
filesystem operations remain outside the callback.

The new design preserves these boundaries and invariants:

- source selection remains one absolute half-open range `[start, end)`;
- every successful outcome consumes that complete selected range;
- leading audio omitted from WAVs is still consumed by the source cursor;
- failures consume nothing;
- a frozen retry uses an immutable file set and crop boundary;
- memory proportional to retention, channel count, split capacity, detector
  state, and maximum calibration duration is planned and allocated at startup;
- no allocation is proportional to the selected export duration;
- final paths are fully preflighted before final publication;
- publication never overwrites an existing file or directory;
- invalid/uncertain classification fails open by retaining audio.

## Public core model

Two focused public modules form the seam used by the daemon, tests, and a
future lamb-ui process:

- `activity`: channel modes, detector kinds, threshold records, detector
  results, detector implementations, calibration identity/metadata, and frozen
  channel decisions;
- `export_policy`: typed layouts, validated patterns, canonical rendering,
  path previews, publication strategy, and resolved export policy.

The core enums are deliberately small:

```text
ChannelExportMode:
  Always | Auto | Never

ActivityDetectorKind:
  ExactZero | WindowedRmsPeak | FixedLevel | CalibratedNoiseFloor

ActivityResult:
  Active | Inactive | Ambiguous

SilencePolicyPreset:
  AllChannelsExactZero | PerChannelExactZero

ExportLayoutKind:
  FlatDetailed | TimestampDirectory | Custom

ThresholdSource:
  Manual | Calibrated
```

`FixedLevel` and `CalibratedNoiseFloor` reserve stable serialized names but are
rejected during profile validation. They do not yet have parameter models.
`WindowedRmsPeak` is detector version 1. Threshold provenance is independent of
the detector algorithm.

`ActivityDetector` is a bounded core trait implemented by typed in-tree
detectors. Runtime dispatch uses a closed enum rather than heap-owned dynamic
plugins. The trait consumes blocks from an immutable frozen epoch and caller-
provided fixed workspace. A future detector may add startup-planned state
without changing disposition, crop, path-plan, manifest, or publication APIs.

## Profile configuration

Each profile capture port gains an optional mode:

```toml
[[profiles.scarlett.pipewire.capturePorts]]
source = "capture_AUX3"
name = "aux3"
exportMode = "auto"
```

The same optional `exportMode` is available on JACK `capture.ports`. It is not
added to legacy `configVersion = 1` capture topology.

Profile export configuration gains typed fields:

```toml
[profiles.scarlett.export]
outputDir = "/home/user/Music/LAMB"
layout = "custom"
directoryPattern = ""
filenamePattern = "lamb-{timestamp}-{channel}-{startFrame}-{endFrame}{partSuffix}.wav"
defaultChannelMode = "auto"
activityDetector = "windowed-rms-peak"
```

`silencePolicy` is a mutually exclusive profile-wide shorthand:

```toml
silencePolicy = "all-channels-exact-zero"
```

or:

```toml
silencePolicy = "per-channel-exact-zero"
```

When `silencePolicy` is present, `defaultChannelMode` and
`activityDetector` must be omitted. Per-port `exportMode` remains legal.

The latest threshold result is stored under the configured channel name:

```toml
[profiles.scarlett.channels.aux3.activity]
thresholdDbFS = -63.4
thresholdSource = "calibrated"
updatedAtUnixSeconds = 1787616000
inputId = "<stable-input-id>"
calibrationId = "<calibration-generation-id>"
```

A manual record uses `thresholdSource = "manual"`. It may retain the latest
`calibrationId` so the previous calibration sample remains available for
inspection/recomputation. `threshold reset` removes the threshold record and
clears calibration state for that input.

Threshold values must be finite and within `[-120.0, 0.0]` dBFS. Threshold
record fields are validated as a coherent set. Channel table keys must resolve
to exactly one configured port name in the profile.

## Configuration resolution

Channel mode resolves in this order:

1. per-port `exportMode`;
2. profile policy default;
3. the selected compatibility/default policy.

Resolution policies are:

### Modern profile omission

When profile activity fields and `silencePolicy` are omitted:

- default mode is `Auto`;
- detector is `WindowedRmsPeak`;
- missing, stale, corrupt, or identity-mismatched threshold state produces
  `Ambiguous`, which is retained and prevents leading trim.

### Per-channel exact-zero preset

`silencePolicy = "per-channel-exact-zero"` expands to:

- default mode `Auto`;
- detector `ExactZero`;
- no whole-export gate;
- common two-second preroll trimming enabled.

### Whole-export exact-zero preset

`silencePolicy = "all-channels-exact-zero"` expands to:

- a transaction-wide exact-zero gate;
- default mode `Always` after that gate passes;
- exact-zero evidence analysis for legal per-port overrides;
- historical untrimmed export start.

`Always` overrides per-channel classification, but does not override this
explicit whole-export gate.

### Legacy `configVersion = 1`

Legacy configuration automatically retains historical behavior:

- transaction-wide exact-zero omission;
- all channels published after the gate passes;
- no leading trim;
- recall uses flat detailed names;
- dump uses one atomic timestamp directory;
- both commands use configured `outputDir`.

## Channel disposition and outcomes

Mode and detector result remain separate:

```text
Always -> Keep
Never  -> Drop
Auto + Active    -> Keep
Auto + Ambiguous -> Keep
Auto + Inactive  -> Drop
```

`Never` suppresses publication only. Capture topology, ring contents, selected
range, and cursor/loss accounting are unchanged.

Successful zero-file outcomes are distinguished:

- `SkippedSilent`: at least one potentially publishable `Auto` channel was
  removed because it was `Inactive`, or the whole-export exact-zero gate
  classified a non-policy-empty transaction as silent;
- `SkippedByPolicy`: every channel was excluded by `Never`, with no candidate
  auto/always channel;
- `NoNewAudio`: the selected source range is empty, unchanged from current
  behavior.

A mixture of `Never` channels and `Auto` channels that are all inactive returns
`SkippedSilent`. Every successful skip consumes the complete selected range.
Policy emptiness is decided before an optional whole-export gate: if every
channel is `Never`, the result is `SkippedByPolicy` regardless of sample values.
Otherwise the whole-export exact-zero gate examines the complete captured
multichannel range, including channels later suppressed by a per-port override.

`SkippedSilent` and `SkippedByPolicy` carry the same consumed range and loss
breakdown as the existing silent outcome; neither carries output paths.

## Detector version 1

### Exact-zero

For each channel, every finite `+0.0` and `-0.0` sample is silent. The first
sample comparing unequal to zero is the first evidence frame. A non-finite
sample makes classification `Ambiguous`, ensuring retention. For the explicit
whole-export gate, a non-finite sample prevents exact-zero omission.

### Windowed RMS/peak

Version 1 uses deterministic constants:

```text
window                         20 ms
hop                            10 ms (50% overlap)
open threshold                 thresholdDbFS
close/evidence threshold       thresholdDbFS - 6 dB
minimum sustained open         100 ms
transient bypass               peak >= thresholdDbFS + 12 dB
calibrated threshold           p95 window RMS + 10 dB
leading preroll                2 seconds
```

Window and hop frame counts are derived with checked integer arithmetic from
the session sample rate. Two staggered fixed accumulators per channel implement
50% overlap without a recording-sized or window-sized allocation.

RMS drives the open, close, and weaker-evidence decisions. Peak is used for the
transient bypass. This avoids treating normal crest factor as sustained open
evidence.

Classification is:

- `Inactive`: every complete/partial window remains below the close/evidence
  threshold and no transient bypass occurs;
- `Ambiguous`: RMS evidence rises above the close threshold but never satisfies
  sustained open, or any non-finite sample is observed;
- `Active`: the gate remains open for at least 100 ms, or a transient bypass
  occurs, provided no non-finite sample makes the complete classification
  ambiguous.

Sustained-open duration is measured from the first open window's absolute start
through the current open window's exclusive end. A window below the close
threshold closes and resets that run; a window in the hysteresis band keeps an
already-open gate open but does not open a closed gate.

The first evidence frame is the start of the earliest non-`Inactive` window.
For an exact-zero detector it is the exact first evidence sample. Missing or
unusable threshold state makes the channel `Ambiguous` with first evidence at
the transaction start.

## Common leading crop

Classification always covers the complete selected source range before WAV
preparation. After dispositions are known:

1. discard `Drop` channels;
2. among retained channels, find the minimum first evidence frame;
3. subtract exactly two seconds of sample-rate frames with checked arithmetic;
4. clamp to the selected transaction start;
5. use that one frame for every retained channel;
6. retain the original transaction end; do not trim trailing silence.

```text
exportStartFrame = max(
    transactionStartFrame,
    earliestRetainedEvidenceFrame - sampleRate * 2
)
```

`Always` channels are still analyzed for meaningful evidence, but confidently
inactive noise does not force the crop to the beginning. If at least one
`Always` channel is retained and no retained channel has evidence, the outcome
is `Written` from the original transaction start. There is no defensible onset
from which to crop.

Compatibility whole-export/legacy policies disable leading crop entirely.

## Frozen retry decision

The coordinator, not the transient path planner, owns a preallocated
`FrozenExportDecision` for the lifetime of a frozen transaction:

```text
FrozenExportDecision
├── classified flag and detector/policy generation
├── per-channel mode
├── per-channel ActivityResult
├── per-channel Keep/Drop disposition
├── per-channel first evidence frame
└── common exportStartFrame
```

Decision storage is allocated from the startup plan, moved logically into the
frozen transaction, and recycled only after commit/release. A failed path
preflight, WAV write, manifest operation, copy, sync, or rename never clears or
recomputes it. Threshold changes affect only the next newly frozen transaction.

If classification itself fails before the complete decision is marked valid,
no partial bitmap is authoritative and retry may restart classification. Once
valid, the bitmap, evidence frames, crop, and resulting sparse file set are
immutable.

## Outcome frame semantics

Existing outcome fields continue to describe the consumed source range:

```text
startFrame  = selected absolute start
endFrame    = selected absolute exclusive end
frames      = endFrame - startFrame
```

`Written` adds:

```text
exportStartFrame
exportFrames
```

Version 1 enforces:

```text
exportStartFrame >= startFrame
exportStartFrame + exportFrames == endFrame
```

Every retained channel has that same encoded range. Loss accounting remains
attached to the consumed range.

## Typed layout model

The resolved layout is one of:

### Flat detailed

Final files are direct children of `outputDir`, use detailed collision-
resistant names, and publish through the manifest-backed file-set strategy.
The preset includes timestamp, channel, sample rate, absolute part boundaries,
and split suffix.

### Timestamp directory

Equivalent rendering is:

```text
directoryPattern = "{timestamp}"
filenamePattern = "{channel}{partSuffix}.wav"
```

This preset is implemented separately and retains the stronger guarantee that
one complete directory is synchronized and atomically renamed no-overwrite.
It is not routed through custom file-set publication.

### Custom

Both patterns are required. `directoryPattern` may be empty. It is rendered for
every retained channel and split part and may use channel/part/frame tokens,
therefore different files may target different relative directories. Custom
layouts always use manifest-backed file-set publication.

### Legacy command defaults

Layout omission resolves centrally, not in daemon handlers:

- recall: historical flat detailed basename and file-set publication;
- dump: historical atomic timestamp-directory basename/layout.

Both final roots are `ResolvedExportPolicy.outputDir`. App-mode dump no longer
uses `$HOME/.cache/lamb/out`.

## Pattern language and token semantics

The complete token set is:

- `{timestamp}`
- `{channel}`
- `{sampleRate}`
- `{startFrame}`
- `{endFrame}`
- `{part}`
- `{partSuffix}`
- `{profile}`

Patterns are parsed once during configuration validation into literal/token
segments. Unknown tokens, unmatched braces, nested braces, and malformed empty
tokens are rejected. There is no second parser in daemon, GUI, or workspace
code.

For every individual WAV part:

- `{startFrame}` and `{endFrame}` are the absolute half-open source boundaries
  encoded in that part after common cropping;
- `{part}` is one-based per retained channel;
- `{partSuffix}` is empty when the channel is unsplit, otherwise
  `-part001`, `-part002`, and so on (minimum three-digit padding);
- omitted channels render no paths.

The timestamp is the existing compact UTC label. Channel and profile values
come from resolved core data and are subjected to the same rendered-path
validation as literals.

## Canonical validation, rendering, and preview

`export_policy` exposes one renderer used by:

- profile validation;
- worst-case startup capacity validation;
- persistence preflight;
- public layout preview.

The preview accepts typed context (profile, timestamp, sample rate, retained
channels, export range, and split threshold) and returns typed entries
containing channel, part, absolute part range, relative directory, filename,
and final path. Preview may allocate a bounded result vector; persistence writes
the same rendering into preallocated reusable path slots.

Validation rules are:

- `outputDir` is absolute and lexically canonical;
- a rendered directory is empty or a relative path beneath `outputDir`;
- absolute paths, NUL, `.`, `..`, traversal, unsafe empty components, and path
  prefixes are rejected;
- a rendered filename is exactly one nonempty normal component and contains no
  `/` or NUL;
- all final paths are lexically contained by `outputDir`;
- existing symlink path components are rejected during filesystem preflight;
- every final path is unique across all retained channels and parts;
- file/parent conflicts are rejected;
- existing final files/directories are rejected before WAV staging where
  possible and are still protected by no-overwrite publication against races;
- every final, staged, partial, manifest, and transaction path fits
  `maximum_path_bytes`.

Startup validates worst-case token widths (including `u64` absolute frame
values and maximum part index) for every potentially retained channel. Runtime
preflight renders every actual sparse output before creating a staging
transaction. Patterns do not introduce frame-count-proportional allocation.

## Sparse workspace planning and WAV preparation

Preparation becomes a strict sequence:

1. recover/finish prior workspace cleanup;
2. classify the frozen epoch if its frozen decision is not already valid;
3. apply the optional whole-export gate and channel modes;
4. return a typed skip if no channel remains;
5. calculate the common encoded range;
6. render and preflight paths only for retained channels and actual split
   parts;
7. create private staging;
8. open writers only for retained channels;
9. traverse the immutable frozen epoch again and encode from the common start
   through the original end;
10. finalize only the actual sparse file plan.

The activity pass uses fixed per-channel detector state. The WAV pass reuses
the existing interleaved scratch and per-channel PCM buffers. No silent WAV is
written and later deleted. `output_count`, manifests, publisher output, and
recovery all describe only retained channel/part files.

Output slots are densely packed by retained channel then part while preserving
the original capture channel index in each slot. Writer slots carry the dense
output offset; inactive/never channels never open a file.

## Publication strategy and recovery

Publication strategy is selected by resolved layout, not protocol command:

```text
FlatDetailed       -> FileSet
Custom             -> FileSet
TimestampDirectory -> AtomicDirectory
Legacy recall      -> FileSet
Legacy dump        -> AtomicDirectory
```

Prepared persistence and manifests use strategy names rather than recall/dump
names. Manifest schema version 2 records the actual sparse entry count and
strategy. Recovery continues to read existing version-1 recall/dump manifests
and maps them to the equivalent strategy.

### File-set publication

Staged files use internal slot-derived names, avoiding basename collisions when
custom paths target different directories. For each planned final file:

1. ensure its validated parent hierarchy exists beneath `outputDir`, rejecting
   symlink ancestors;
2. create a transaction-hidden adjacent partial with `create_new`;
3. copy, flush, and synchronize it;
4. rename no-overwrite to the final path;
5. record identity and phase in the manifest.

All distinct final parent directories are synchronized before commit. On
failure/recovery, only transaction-owned partials/finals with matching
identities are removed. Nested final and partial paths must be canonically
contained; each partial must be adjacent to its corresponding final. Empty
directories created solely to host rolled-back outputs are removed best-effort
when identity and emptiness checks permit, but foreign siblings are never
removed.

### Atomic timestamp-directory publication

All files are staged as direct children of one hidden sibling directory,
synchronized, represented by one manifest, and renamed no-overwrite to the
timestamp directory. The output parent is synchronized before commit. This
preserves the existing all-at-once directory visibility guarantee.

`PublishedOutput.files` is derived only from the prepared sparse plan. A
`Written` response must report only paths that exist after publication.

## Calibration command family

The CLI gains:

```text
lamb threshold calibrate --profile scarlett --channel aux3 [--seconds 5]
lamb threshold set       --profile scarlett --channel aux3 --dbfs -60
lamb threshold show      --profile scarlett
lamb threshold reset     --profile scarlett --channel aux3
```

All commands use the normal control socket by default and accept `--socket` as
an override. They are daemon control clients; they never mutate TOML directly.
Legacy-config daemons return:

```text
profile threshold commands are unsupported for legacy configuration
```

`set`, `reset`, and `calibrate` run on the bounded serialized operation lane.
`show` also uses the lane for a coherent daemon-owned snapshot and may wait
behind calibration. Ordinary `status` stays on its existing responsive path.

`show` distinguishes stored and live state, including detector, threshold
source/value, timestamp/age, sample availability, active-profile status,
identity match, calibration validity/staleness, effective threshold, and
detector version. Inactive profiles report live identity as not currently
resolved.

## Live calibration boundary

Calibration requires:

- capture is currently running;
- the requested profile exactly equals the active profile;
- the requested channel name resolves exactly once in that live session;
- the configured stable name/source pair and live backend/device/sample-rate
  identity can be captured.

It never starts capture, switches profiles, reloads topology, replaces the
session, freezes an epoch, consumes/clears audio, advances the persistence
cursor, or creates export WAVs.

Duration defaults to five seconds and is restricted to one through thirty
seconds. Observation begins only after the request is accepted and includes
future captured frames. Queue drops, timeout, runtime failure, non-finite
calibration input, insufficient frames/windows, or an out-of-range derived
threshold fails without changing config/state.

A dedicated preallocated calibration request/result slot is independent of the
existing capture command slot. The capture worker copies the selected channel
from admitted future ingress blocks into a startup-planned mono sample buffer
and accumulates overlapping RMS/peak windows in fixed arrays. Status commands
can continue using the ordinary command slot while the operation worker waits.

The calibration result records p95 RMS, observed RMS/peak summaries, derived
threshold, detector version, frames, duration, timestamp, dropped-frame delta,
and live identity.

The p95 calculation uses complete 20 ms windows only. Window RMS/peak values
are accumulated into startup-sized arrays, and the operation worker performs a
deterministic in-place total-order selection/sort after capture completes. A
partial final calibration window is recorded in metadata but excluded from the
percentile.

## Calibration sample state

The latest actual mono calibration audio is retained as IEEE-float F32LE WAV,
bounded by the accepted observation duration. It is not embedded in TOML.

State root resolution is:

```text
$XDG_STATE_HOME/lamb/calibration
```

with the standard `$HOME/.local/state` fallback. A stable input id is a
collision-resistant digest of canonical backend/device/configured name/source
identity. Sample-rate and live resolved identity remain metadata and
compatibility checks, so a sample-rate change makes a calibration stale without
silently attaching it to another port.

State uses immutable calibration generations:

```text
<state-root>/<stable-input-id>/<calibration-id>/
    sample.wav
    metadata.json
```

Only the generation referenced by profile config is authoritative. A new
generation is fully written and synchronized before the candidate config can
reference it. This supplies a safe commit point even when config and state are
on different filesystems:

1. capture and derive into preallocated memory;
2. write a unique generation directory;
3. flush/synchronize both files and the generation/parent directories;
4. build and validate candidate `AppConfig` referencing that generation;
5. atomically persist candidate config;
6. install candidate daemon/live policy in memory;
7. remove the previously referenced generation best-effort.

If state preparation or config persistence fails, the previous config and
calibration remain authoritative and the new unreferenced generation is
removed. Startup/state maintenance removes unreferenced incomplete/orphan
generations. Thus there is one authoritative latest sample per input even if a
crash temporarily leaves a hidden/unreferenced cleanup artifact.

Manual thresholds do not require a sample and may retain the existing sample
reference. Reset atomically removes the threshold/reference from config first,
then identity-safely removes calibration generations.

## Atomic profile persistence

Threshold mutation follows:

```text
validate request
-> clone/mutate candidate AppConfig
-> resolve and validate candidate profile/policy
-> atomically persist candidate config
-> install daemon-owned candidate
-> update active policy for future transactions only
-> respond success
```

Atomic config persistence uses an adjacent unique temporary file, preserves
appropriate ownership/permissions, flushes and `sync_all`s the file, verifies
the owned target has not unexpectedly changed, atomically renames/replaces it,
and synchronizes the parent directory. Disk failure cannot leave live policy
ahead of persisted config.

An active session keeps its topology and cursor. Its mutable resolved activity
policy is replaced only after durable config success. A pending frozen
transaction ignores the update because it already owns a valid frozen export
decision. The next newly frozen range uses the new policy.

## Calibration staleness

A calibrated threshold is stale after 30 days. It is immediately stale when
detector version, backend, resolved device, stable name/source, sample rate,
sample presence, metadata, or generation reference does not match.

Manual thresholds do not age out, but the stable input identity must match.
Stale or missing state fails open as `Ambiguous` from transaction start.

## Startup memory plan

The memory plan adds explicit components for:

- frozen per-channel disposition/result/evidence slots;
- detector per-channel staggered window/gate state;
- one profile-session maximum-duration mono calibration sample buffer;
- bounded calibration RMS/peak window-stat slots for 30 seconds at the live
  sample rate;
- calibration command/result metadata.

All arithmetic is checked. Allocations are exact/page-materialized before
backend activation and included in `memory.max`. Legacy sessions may set
maximum calibration duration to zero because profile threshold commands are
unsupported, preserving their historical memory requirements apart from small
fixed metadata.

The persistence output/path/manifest slot maximum remains based on all channels
and maximum split parts. Sparse operations consume a prefix of those slots.
Cropping can reduce actual parts but never increases the startup maximum.

## Error handling and concurrency

- Detector configuration errors reject profile resolution before capture.
- Unsupported detector names produce explicit validation errors.
- Path/pattern/collision/capacity errors occur after a frozen decision but
  before staging/final publication; the range and decision remain retryable.
- WAV/publication/recovery failures retain the existing frozen transaction and
  exact sparse decision.
- Calibration errors leave prior threshold/sample/config/live policy intact.
- Threshold config write failures install no live update.
- Status remains responsive during export and calibration.
- Recall, dump, clear, threshold mutation, calibration, lifecycle changes, and
  stop remain serialized by the bounded operation lane.
- Stop may wait for the bounded active calibration operation; no unbounded
  calibration is admitted.

## Testing strategy

### Activity and crop

- four channels: active mic, one small guitar sample, exact-zero channel, and
  negative-zero channel; exact-zero auto retains only mic/guitar;
- one active channel among four;
- all auto channels exact zero returns `SkippedSilent` and no final files;
- all `Never` returns `SkippedByPolicy`;
- mixed `Never` plus inactive auto returns `SkippedSilent`;
- `Always` publishes a silent channel and uses full range when no evidence
  exists;
- NaN and both infinities produce `Ambiguous` and are retained;
- detector v1 window/hop, hysteresis, sustained-open, transient bypass, and
  evidence-frame behavior;
- missing/stale/mismatched threshold fails open without trim;
- earliest evidence across retained channels yields one two-second-preroll
  start; all stems align; no trailing trim;
- consumed and encoded ranges satisfy protocol invariants;
- sparse active channels split correctly.

### Retry and publication

- write/preflight/publication failure retains selected range, channel bitmap,
  evidence frames, crop, and sparse file set;
- a threshold update after failure does not alter pending retry decisions;
- `Written.files` contains only existing retained-channel files;
- sparse manifests contain only actual files;
- interrupted sparse file-set publication recovers/rolls back correctly;
- interrupted atomic timestamp directory preserves its stronger guarantee;
- version-1 manifests remain recoverable;
- no-overwrite and identity replacement protections remain intact.

### Layout

- flat-detailed preset;
- timestamp-directory preset and atomic rename;
- custom nested per-channel/part/frame rendering and public preview;
- legacy recall/dump defaults under configured output root;
- profile app dump respects `export.outputDir`;
- unknown/malformed tokens, absolute/traversing directories, unsafe empty
  components, filename separators, NUL, path overflow, symlink ancestors,
  duplicate paths, and cross-channel/part collisions reject before publication;
- maximum absolute frame and part widths fit startup path accounting.

### Calibration and threshold commands

- CLI parsing/default socket/override for all four commands;
- legacy daemon rejection;
- exact active profile and stable channel identity validation;
- future-only observation without freeze/cursor/topology changes;
- bounded duration and startup-planned sample/stat buffers;
- status responsiveness while calibration waits;
- deterministic p95/threshold derivation and F32LE sample round-trip;
- manual set, show, reset, age/staleness, and active/inactive profile reporting;
- atomic state/config failure injection retains the previous authoritative
  calibration and live policy;
- successful recalibration leaves one referenced latest generation;
- reset clears threshold and sample state;
- detector update affects the next new frozen transaction, not a retry.

### Regression gates

- allocation tests prove selected export duration does not change workspace
  allocation and calibration memory is bounded/planned;
- callback allocation/topology tests remain unchanged;
- formatting, strict Clippy, all Cargo tests, and Nix checks/build pass.

## Documentation

README documentation will distinguish:

- whole-export exact-zero compatibility omission;
- explicit per-channel exact-zero omission;
- calibrated/manual `windowed-rms-peak` classification;
- `Active`, `Inactive`, and fail-open `Ambiguous`;
- common leading crop versus full cursor consumption;
- all layout presets, custom validation, and rendered examples;
- threshold command usage, state/sample retention, identity, and staleness;
- future detector evolution from the versioned in-tree trait/enum boundary.
