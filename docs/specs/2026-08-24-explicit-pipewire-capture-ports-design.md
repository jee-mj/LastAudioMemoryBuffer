# Explicit PipeWire Capture Ports

**Date:** 2026-08-24  
**Status:** Approved for implementation

## Summary

LAMB will require every PipeWire configuration to select source ports explicitly.
The order of `capturePorts` defines the interleaved capture-channel order. Each
entry's `name` remains the downstream channel label used for per-channel WAV
filenames; PipeWire port names such as `capture_AUX0` never replace it.

JACK and fake-backend behavior remain unchanged. Existing PipeWire
configurations that use `channels`, `channelMap`, or implicit target-node
autoconnection must be migrated.

## Goals

- Fail before capture starts when a PipeWire configuration omits explicit
  source-port selection.
- Resolve every selected port against the chosen PipeWire input/source node.
- Reject missing, duplicate, wrong-node, wrong-direction, ambiguous, and
  non-audio source ports with actionable errors.
- Capture channels in exactly the configured order.
- Preserve configured output names for status, recall, dump, and WAV export.
- Support both legacy and profile configuration modes.
- Migrate the current Scarlett setup to `capture_AUX0` through `capture_AUX3`.

## Non-goals

- Changing JACK port configuration or capture behavior.
- Adding automatic port-name guessing or fallback to `AUTOCONNECT`.
- Supporting aliases or object paths in `source`; matching is against exact
  `port.name` metadata.
- Redesigning runtime hotplug recovery after a successful startup.
- Implementing a custom PipeWire filter or node.

## Configuration

### Legacy mode

Add a camelCase `capturePorts` array to `LambConfig`:

```toml
backend = "pipewire"
target = "alsa_input.usb-Focusrite_Scarlett_16i16_4th_Gen_S6GMC8F4901BC0-00.pro-input-0"

capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
  { source = "capture_AUX2", name = "percL" },
  { source = "capture_AUX3", name = "percR" },
]
```

For `backend = "pipewire"`:

- `capturePorts` must contain at least one entry.
- `source` and `name` are trimmed and must be present and non-empty.
- Normalized `source` values must be unique.
- Normalized `name` values must be unique.
- The presence of `channels` is an error, even when its value agrees with the
  port count.
- The presence of `channelMap` is an error, including `channelMap = []`.

`channelMap` must therefore retain field-presence information during
deserialization rather than collapsing omission and an empty array. Fake
configuration continues to accept `channels` and `channelMap` with its current
validation rules.

The effective PipeWire channel count is `capturePorts.len()`. Ordered output
channel names are the normalized `name` values.

### Profile mode

Add `capturePorts` beneath the existing `pipewire` table:

```toml
[profiles.scarlett]
backend = "pipewire"

[profiles.scarlett.pipewire]
target = "alsa_input.usb-Focusrite_Scarlett_16i16_4th_Gen_S6GMC8F4901BC0-00.pro-input-0"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
  { source = "capture_AUX2", name = "percL" },
  { source = "capture_AUX3", name = "percR" },
]
```

Profile validation applies the same required-value and uniqueness rules and
produces ordered `ResolvedCapturePort` values. A PipeWire profile rejects the
presence of legacy `pipewire.channelMap`, including an empty array. Non-empty
generic `capture.ports` or `capture.sources` are rejected because those fields
belong to JACK profiles. A prior `channelMap`-only PipeWire profile cannot
start.

## Internal Models

`PipeWireCaptureConfig` will carry ordered capture-port selections instead of
`channels` and `channel_map`. Each backend selection contains the normalized
`source` and configured output `name`. Its effective channel count is derived
from the vector length.

PipeWire graph discovery will add an `AvailablePort` model containing:

- global object ID;
- owning `node.id`;
- object type;
- `port.direction`;
- numeric node-local `port.id`;
- `port.name`;
- `format.dsp`; and
- `audio.format` when advertised.

The graph snapshot also retains the discovered factory name whose
`factory.type.name` creates `PipeWire:Interface:Link` objects. This avoids
assuming that the factory is always named `link-factory`.

A resolved source port contains the selected global port ID, owning node ID,
node-local port ID, and source name. `ResolvedTarget.channels` is the number of
resolved source ports, not the target node's total `audio.channels` value.

## Graph Resolution

Resolution is implemented as pure, synthetic-testable logic over discovered
nodes and ports:

1. Select the target using the existing node matching and input/source checks.
2. For each configured `source`, collect ports whose exact `port.name` matches.
3. Prefer matches owned by the selected target node.
4. If the name exists only on another node, report that node and the expected
   target.
5. If no match exists, report the selected target and list its available audio
   output port names.
6. Reject more than one matching port on the selected target as ambiguous.
7. Require `port.direction = "out"`.
8. Classify a port as audio when its normalized `format.dsp` ends in the
   standalone word `audio`, or when it advertises a non-empty `audio.format`.
   Reject MIDI, video, control, and unknown formats.
9. Return source ports in configuration order, independently of registry event
   order.

Configuration validation catches duplicate selections first. The backend
resolver repeats the uniqueness check defensively for programmatically
constructed `PipeWireCaptureConfig` values.

## Explicit Link Startup

The current target-node `AUTOCONNECT` path is replaced with explicit links on
the PipeWire capture thread:

1. Register one registry listener before stream connection and collect nodes,
   ports, removals, and the link factory.
2. Re-resolve the configured target and source names on this connection. This
   catches ports removed or replaced after the caller's initial discovery.
3. Create the existing multichannel F32LE input stream without
   `target.object` and without `StreamFlags::AUTOCONNECT`.
4. Connect it with `StreamFlags::INACTIVE`, `MAP_BUFFERS`, and `RT_PROCESS` so
   no capture callback can run before links are complete.
5. Wait for the stream's node ID and input-port globals. Select audio input
   ports owned by that node, require exactly the configured count, require
   unique numeric `port.id` values, and sort them by `port.id`.
6. Pair configured source port `n` with sorted stream input port `n`.
7. Create one non-lingering `Link` through the discovered factory using global
   output and input port IDs and their owning node IDs.
8. Keep every link proxy and listener alive for the capture thread's lifetime.
9. Use a core synchronization barrier and listener error state to confirm that
   object creation succeeded.
10. Activate the stream and signal startup readiness only after the explicit
    link set is complete.

The stream adapter exposes one destination port per channel and interleaves
those ports in node-local `port.id` order. Pairing source selections to that
ordered destination list therefore makes buffer channel order equal to
`capturePorts` order while retaining one synchronized process callback.

Stopping capture drops the non-lingering links and stream; no persistent graph
objects remain. Runtime device hotplug recovery remains outside this change.

## Daemon and Export Labels

Legacy startup derives `CaptureSession.channel_names` from
`PipeWireCaptureConfig.capture_ports[*].name`. Profile startup continues to
derive names from `ResolvedProfile.ports`, which now contains the explicit
PipeWire selections. Neither path derives output labels from discovered
PipeWire metadata.

The existing persistence and export layers remain unchanged and continue to
receive ordered configured labels such as `mic`, `gtr`, `percL`, and `percR`.

## Errors

All configuration failures occur before graph capture starts. Messages include
the relevant field/index and values. Representative failures are:

- `capturePorts is required for pipewire backend`
- `capturePorts[2].source duplicates capturePorts[0].source`
- `profile scarlett: pipewire.channelMap conflicts with pipewire.capturePorts`
- `source port 'capture_AUX2' belongs to node 'other-node', expected 'scarlett-node'`
- `source port 'playback_AUX0' has direction 'in'; expected 'out'`
- `source port 'midi_out' is not audio (format.dsp='8 bit raw midi')`

PipeWire proxy/core errors during stream or link creation are wrapped as
capture errors and returned through the existing startup-ready channel. The
daemon must not construct an active `CaptureSession` after any such failure.

## Tests

### Legacy configuration

- Valid explicit ports derive ordered names and channel count.
- Omitted and empty `capturePorts` fail for PipeWire.
- Missing and blank `source`/`name` values fail with their index.
- Duplicate normalized sources and names fail.
- `channels` presence fails, including a matching value.
- `channelMap` presence fails, including an empty or matching value.
- Existing fake-backend channel validation remains green.

### Profile configuration

- TOML parses camelCase `pipewire.capturePorts`.
- Profile resolution preserves source and name order.
- Omitted/empty ports, blanks, duplicates, old `pipewire.channelMap`, and
  non-empty generic JACK capture fields fail.
- Resolved PipeWire backend configuration carries the same ordered selections.

### PipeWire backend

- Synthetic registry order does not affect configured resolution order.
- Missing, duplicate, wrong-node, wrong-direction, non-audio, and ambiguous
  ports fail independently.
- Stream destination ports are ordered by numeric `port.id`.
- The pure link plan pairs configured sources with destination ports by index.
- Legacy-to-backend conversion carries ports and derives four channels.
- Existing F32LE chunk-offset, size, stride, and capture runtime tests remain
  green.

### Verification

Run `cargo fmt --check`, the full Cargo test suite, and Clippy with warnings
denied. Then run a short, low-retention live smoke configuration against the
current Scarlett graph. Verify four explicit links from `capture_AUX0` through
`capture_AUX3` to LAMB input ports `0` through `3`, daemon status reports four
channels, and configured labels remain `mic`, `gtr`, `percL`, and `percR`.

## Documentation and Migration

README examples will use required explicit PipeWire ports in both legacy and
profile modes. Migration notes will explain that Pro Audio exposes individual
ports, `source` selects one exact PipeWire `port.name`, array order determines
capture order, and `name` determines WAV filenames.

After implementation verification, update `~/.config/lamb/lamb.toml` to the
new Pro Audio node and the four requested mappings, removing `channels` and
`channelMap`.

## Confirmed Feasibility

The installed `pipewire 0.10.0` Rust API exposes registry port globals,
`Core::create_object::<Link>`, link listeners, and all required property keys.
The live Scarlett Pro Audio input node advertises 18 output ports; its first
four are `capture_AUX0` through `capture_AUX3`, with audio DSP metadata and
node-local IDs 0 through 3. A transient unconnected four-channel PipeWire
recording stream exposed four audio input ports with node-local IDs 0 through
3, confirming that explicit ordered source-to-stream links fit the existing
single-stream capture architecture.
