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

## Configuration

### Legacy mode (`configVersion = 1`)

```toml
configVersion = 1
user = "<USERNAME>"
backend = "pipewire"
target = "alsa_input.usb-YourDevice-00.multichannel-input"
channels = 2
channelMap = ["mic", "gtr"]
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
backend = "jack"
clientName = "lamb"

[profiles.my-profile.capture]
ports = [
  { source = "system:capture_1", name = "mic" },
  { source = "system:capture_2", name = "gtr" },
]

[profiles.my-profile.buffer]
seconds = 1800

[profiles.my-profile.export]
outputDir = "/home/<USERNAME>/Music/LAMB"
mode = "per-channel"
format = "wav"
```

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
