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
audio. A truly empty interval succeeds without creating files. An interval in
which every source sample on every channel is exactly zero is likewise consumed
successfully without creating files.

`recall` prepares its publication through `/tmp/LAMB/staging`, then retains the
flat, detailed files in the configured output directory. `dump` instead
publishes a timestamp directory containing simple per-channel files. If
publication fails, the source range remains retryable and is not consumed. Ring
buffer retention can overwrite unhandled old frames; when this wrap loses old
frames, the command reports the loss.

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
