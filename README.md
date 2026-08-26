# LAMB — LastAudioMemoryBuffer

Rolling audio memory buffer daemon. Continuously captures multichannel audio
into a bounded ring buffer and exports per-channel 24-bit WAV files on command.

**v0.2.0** — Rust daemon + CLI with PipeWire and JACK backends.

## Quick Start

```bash
# Build (Nix)
nix build

# Build (Cargo — requires pipewire + jack2 dev headers)
cargo build --release

# Run daemon (legacy config)
lamb daemon --config ~/.config/lamb/lamb.toml

# CLI control (socket defaults to $XDG_RUNTIME_DIR/lamb/control.sock)
lamb recall --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
lamb dump --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
lamb status --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
lamb stop --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
lamb start-capture --socket "$XDG_RUNTIME_DIR/lamb/control.sock" --profile my-profile --activate
lamb stop-capture --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
lamb reload --socket "$XDG_RUNTIME_DIR/lamb/control.sock"
```

## Incremental Recall and Dump

Within a capture session, `recall` and `dump` share one consumption cursor. Each
command saves only the new source frames since either command last handled
audio. A truly empty interval succeeds without creating files. Successful
publication and intentional omission consume the complete selected source
range; failures consume nothing and leave the same frozen decision retryable.
Loss accounting (`retention_lost_frames`, `cleared_frames`, and
`capture_dropped_frames`) always describes that full consumed range, even when
some channels are omitted or leading frames are cropped from every WAV.

`recall` prepares its publication through `/tmp/LAMB/staging`, then retains the
flat, detailed files in the configured output directory by default. `dump`
instead defaults to one atomically published timestamp directory containing
simple per-channel files. Both commands use the configured `outputDir`. Ring
buffer retention can overwrite unhandled old frames; when this wrap loses old
frames, the command reports the loss.

## Export Activity Policy

Legacy `configVersion = 1` and the explicit
`silencePolicy = "all-channels-exact-zero"` compatibility preset apply one
whole-export gate: only an all-channel, finite exact-zero interval is omitted;
otherwise every `Always` channel is published from the historical untrimmed
start. The gate examines the complete captured multichannel range, including a
channel later suppressed by a per-port override. The explicit
`"per-channel-exact-zero"` preset instead classifies each `Auto` channel and
omits only channels whose finite samples are all `+0.0` or `-0.0`.

For a modern profile, omitting `silencePolicy`, `defaultChannelMode`, and
`activityDetector` means `Auto` plus the version-1 `windowed-rms-peak`
detector. Its 20 ms windows/10 ms hops distinguish sustained or transient
activity from low-level input using a channel threshold: opening must persist
for 100 ms unless peak is at least 12 dB over threshold, while close/evidence is
6 dB below threshold. Calibration derives threshold from p95 window RMS plus
10 dB. Threshold provenance is independent of the algorithm: a windowed
threshold may be `manual` or `calibrated`. Results are `Active`, `Inactive`, or
`Ambiguous`. Non-finite samples make a channel `Ambiguous` and retain it, while
missing, corrupt, stale, or identity-mismatched threshold state additionally
supplies transaction-start evidence and therefore retains the channel without
leading trim. The versioned, bounded in-tree detector enum/trait allows future
algorithms without changing crop, path, manifest, or publication interfaces.

Each profile capture port may set `exportMode = "always"`, `"auto"`, or
`"never"` (`pipewire.capturePorts` and JACK `capture.ports`). `Always` retains,
`Never` suppresses publication only, and `Auto` retains `Active` and
`Ambiguous` but drops `Inactive`. Modes do not change capture topology, ring
contents, the selected range, or cursor/loss accounting. All-`Never` yields a
policy skip; inactive `Auto` candidates yield a silent skip. Both consume the
full selected range and publish no paths.

After all channels are classified, retained channels share one leading crop:
the earliest retained evidence frame minus exactly two seconds, clamped to the
selected start. Every stem ends at the original exclusive end; trailing silence
is not trimmed. Compatibility whole-export/legacy policies disable this crop.
Outcome `startFrame`, `endFrame`, and `frames` still report the full consumed
range, while a written outcome's `exportStartFrame` and `exportFrames` report
the common encoded range.

An app profile can select policy explicitly:

```toml
[[profiles.studio.pipewire.capturePorts]]
source = "capture_AUX0"
name = "mic"
exportMode = "auto"

[profiles.studio.export]
outputDir = "/home/user/Music/LAMB"
layout = "custom"
directoryPattern = "{profile}/{channel}/{part}"
filenamePattern = "{timestamp}-{startFrame}-{endFrame}{partSuffix}.wav"
defaultChannelMode = "auto"
activityDetector = "windowed-rms-peak"
```

`silencePolicy` is a mutually exclusive shorthand for
`defaultChannelMode`/`activityDetector`; per-port `exportMode` remains valid
with either preset.

## Export Layouts and Preview

Layout omission preserves the legacy command defaults under `outputDir`. For a
preview context with timestamp `20260826T130000`, channel `left`, 48 kHz audio,
and a split part covering absolute source range `[1000000, 1104000)`, rendering
is:

| Layout | Example final path | Publication |
| --- | --- | --- |
| omitted + `recall`, or `layout = "flat-detailed"` | `/exports/lamb-20260826T130000-left-48000Hz-001000000-001104000-part001.wav` | manifest-backed file set |
| omitted + `dump`, or `layout = "timestamp-directory"` | `/exports/20260826T130000/left-part001.wav` | one synchronized, atomically renamed directory |
| `layout = "custom"` with the patterns above (profile `studio`, channel `right`, part 2) | `/exports/studio/right/2/20260826T130000-1104000-1208000-part002.wav` | manifest-backed file set |

Custom layout requires both patterns, although `directoryPattern` may be empty.
The complete token set is `{timestamp}`, `{channel}`, `{sampleRate}`,
`{startFrame}`, `{endFrame}`, `{part}`, `{partSuffix}`, and `{profile}`.
Frame tokens are each WAV part's absolute half-open source boundaries after the
common crop. `{part}` is one-based per retained channel; `{partSuffix}` is empty
for an unsplit channel and otherwise `-part001`, `-part002`, and so on. Omitted
channels produce no preview or publication entries.

The public typed preview and persistence preflight share the one canonical
`export_policy` parser/renderer. Preview accepts profile, timestamp, sample
rate, retained channels, export range, and split threshold, and owns only a
bounded returned list of typed channel, part, range, relative-directory,
filename, and final-path entries; persistence renders the same plan into
preallocated slots. Patterns reject unknown/malformed tokens. `outputDir` must
be absolute and lexically canonical; rendered directories must be empty or safe
relative descendants, and filenames must be one nonempty normal component.
Absolute paths/platform prefixes, traversal, NUL, `.`, `..`, unsafe empty
components, separators in filenames, symlink ancestors, duplicate paths,
file/parent conflicts, existing outputs, and paths over the configured maximum
are rejected before publication. Startup checks worst-case token widths and
part indexes; runtime preflights the complete actual sparse plan before staging.
Final publication is no-overwrite.

## Threshold Commands and Calibration State

Threshold commands are daemon clients using the normal control socket (or an
optional `--socket` override); they do not edit TOML directly:

```bash
lamb threshold calibrate --profile studio --channel mic
lamb threshold calibrate --profile studio --channel mic --seconds 30
lamb threshold set       --profile studio --channel mic --dbfs -60
lamb threshold show      --profile studio
lamb threshold reset     --profile studio --channel mic
```

Calibration defaults to 5 seconds and accepts 1–30 seconds. It requires capture
to be running, the requested profile to be exactly the active profile, and the
channel to resolve exactly once in that live session. It observes only future
captured frames. `show` distinguishes stored and live identity, threshold
source/value and age, sample availability, detector/version, active-profile
status, calibration validity/staleness, and the effective threshold.

The latest calibration sample is retained as F32LE WAV beside metadata under:

```text
$XDG_STATE_HOME/lamb/calibration/<stable-input-id>/<calibration-id>/sample.wav
$XDG_STATE_HOME/lamb/calibration/<stable-input-id>/<calibration-id>/metadata.json
```

If `XDG_STATE_HOME` is unset, the root is `$HOME/.local/state/lamb/calibration`.
Generations are immutable, and only the generation referenced by profile config
is authoritative; a replacement generation is synchronized before the config
and live future policy are installed. `set` records a manual threshold without
requiring a sample and may retain the latest sample reference. `reset` first
removes the threshold/reference atomically, then removes calibration state
identity-safely.

Calibrated thresholds become stale after 30 days, or immediately when detector
version, backend/device, configured name/source, sample rate, sample/metadata,
stable identity, or generation reference does not match. Manual thresholds do
not age out but still require matching stable input identity. Missing or stale
state fails open as `Ambiguous` from the transaction start.

Calibration and threshold changes never start/switch capture, reload or replace
topology, freeze/consume/clear audio, advance the recall/dump cursor, or create
export WAVs. A durable successful update affects the next newly frozen range;
an already frozen retry retains its original channel decisions and crop.

## Preallocated Persistence Runtime

Each capture session reserves its worst-case memory up front, before capture
starts, so persistence never performs allocation proportional to the selected
recording length:

- Two complete dual-epoch retention rings are allocated and page-touched at
  startup. Capture callbacks copy frames into a fixed ingress queue; one
  sole-owner capture worker writes the active ring, so filesystem persistence
  never blocks the realtime producer.
- A capture session computes a full memory plan (rings, persistence workspace,
  capture ingress, control queues, worker stacks) and validates it against
  `memory.max` before any backend starts. A plan that exceeds `memory.max`
  fails at startup with a component report rather than during persistence.
- Persistence streams the selected range into synchronized temporary WAVs using
  fixed reusable buffers; exact-zero silence is discarded without final output.

Control stays responsive during persistence: the control server handles
`status` directly and hands mutating commands (`recall`, `dump`, `clear`, and
capture lifecycle) to one bounded, prestarted operation worker.

Publication is transactional and crash-recoverable. Recall and dump write a
versioned, identity-checked manifest alongside their files; at startup LAMB
recovers only marked transactions, completing fully-published recordings,
rolling back incomplete owned sets, and never touching unmarked or foreign
files. Loss is reported by cause — `retention_lost_frames`, `cleared_frames`,
and `capture_dropped_frames` — with `lost_frames` as their total.

Publication requires Linux `renameat2` and directory `fsync`; unsupported
targets fail to compile rather than failing at first publication.

## Configuration

### Legacy mode (`configVersion = 1`)

```toml
configVersion = 1
user = "<USERNAME>"
backend = "pipewire"
target = "alsa_input.usb-YourDevice-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 1800
sampleRate = 44100
sampleFormat = "F32LE"
outputDir = "/home/<USERNAME>/.cache/lamb/out"
dontRemix = true
maxActiveSnapshots = 4
allowQueuedRecall = false
controlSocketPath = "%t/lamb/control.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
```

### App-config mode (profile-based)

```toml
[daemon]
startMode = "manual"
activeProfile = "my-profile"

[profiles.my-profile]
backend = "pipewire"

[profiles.my-profile.pipewire]
target = "alsa_input.usb-YourDevice-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]

[profiles.my-profile.buffer]
seconds = 1800

[profiles.my-profile.export]
outputDir = "/home/<USERNAME>/Music/LAMB"
mode = "per-channel"
format = "wav"
```

Use the PipeWire Pro Audio profile so the device exposes independently
linkable source ports. Each `source` is an exact PipeWire `port.name` on the
selected target, and array order defines captured channel order. Each `name`
defines that channel's WAV filename.

PipeWire configurations without `capturePorts` fail before capture. When
migrating an existing configuration, remove legacy `channels`, `channelMap`,
and profile `pipewire.channelMap` keys.

See `lamb config init` and `lamb config show` for managing profiles.

## NixOS Module

```nix
{
  inputs.lamb.url = "github:jee-mj/LastAudioMemoryBuffer";

  outputs = { nixpkgs, lamb, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        lamb.nixosModules.default
        {
          services.lamb = {
            enable = true;
            user = "<USERNAME>";
          };
        }
      ];
    };
  };
}
```

The module installs `lamb`, helper wrappers (`lamb-recall`, `lamb-clear`,
`lamb-status`, `lamb-stop`, `lamb-dump`, `lamb-start-capture`,
`lamb-stop-capture`, `lamb-reload`), and a systemd service.

## Architecture

```
Audio Interface → PipeWire/JACK callback → SampleRing (chunked ring buffer)
                                                 │
                                    snapshot_last_frames()
                                                 │
                                         Snapshot (descriptor list)
                                                 │
                                    read_channel_samples()
                                                 │
                                    export_wav (24-bit per-channel WAV)
```

- **Capture-path boundedness**: avoids disk I/O in the capture callback and drops frames rather than blocking on pinned chunks.
- **Snapshot descriptors**: no data copy — export reads from the ring under pin-count protection
- **Writer drops under contention**: if a chunk is pinned, frames are dropped (counted) rather than blocking the RT thread
- **Split-safe WAV**: files split on frame boundaries before RIFF limits, written atomically via `.partial` → rename

## License

GPL-3.0-only
