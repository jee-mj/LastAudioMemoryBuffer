# Persistent Degraded Daemon and Legacy PipeWire Startup Recovery

**Date:** 2026-08-29
**Status:** Approved for implementation

## Summary

LAMB will separate daemon lifetime from capture-session lifetime. The daemon and
control socket will remain available when configuration is invalid, a configured
device is absent, or capture startup fails. Permanent faults will wait for an
operator command. Transient faults will retry capture in-process with bounded
backoff while preserving the daemon PID and control socket.

The legacy PipeWire startup regression will be fixed at its source. Static user
configuration will remain immutable, and negotiated channel count, sample rate,
and target identity will live only in resolved runtime state. Static validation
and session-export-policy resolution will finish before the initial control
socket bind. The socket pathname will be owned by an RAII guard so fatal returns
after binding cannot leave a stale pathname.

The existing app-config idle/fallback daemon, bounded operation lane, and control
listener will be generalized. No second legacy supervisor loop or configuration
migration will be introduced.

## Confirmed Configuration Finding

The generated live configuration at `~/.config/lamb/lamb.toml` was inspected
read-only before design work.

1. Explicit duplicate configuration is absent. The file contains
   `capturePorts` and does not contain `channels`.
2. The conflict is created internally. Legacy PipeWire startup writes the
   negotiated four-channel count into `LambConfig.channels`, then calls policy
   resolution, which repeats static validation and interprets the mutated field
   as user-supplied `channels`.

The generated configuration will not be edited to conceal this defect. PipeWire
and system audio configuration are outside this change.

## Goals

- Preserve the semantic distinction between static user configuration and
  resolved runtime capture state.
- Restore the current four-port PipeWire configuration without changing its
  `capturePorts` behavior.
- Keep one daemon PID and one live control socket across expected configuration,
  target, and capture failures.
- Make permanent and transient failures inspectable through status.
- Retry transient capture failures in-process with bounded backoff.
- Avoid capture arena allocation while a permanent fault is waiting for operator
  action.
- Release all capture resources between failed transient attempts.
- Guarantee best-effort socket unlink on every normal return, error return, and
  unwind after a successful bind.
- Preserve abnormal process-death supervision while preventing deterministic
  pre-listener errors from producing an unlimited systemd restart loop.
- Preserve existing control protocol fields and command behavior unless this
  design explicitly adds semantics.

## Non-goals

- Changing PipeWire, WirePlumber, JACK, or system audio configuration.
- Migrating the legacy TOML to app/profile configuration.
- Adding automatic target selection or changing `capturePorts` ordering.
- Replacing the bounded control operation lane.
- Refactoring persistence, recall, export, calibration, or sample-ring behavior.
- Rebinding the control socket when a reload changes its configured path. A
  changed path takes effect on the next daemon process start.
- Adding a separate `lamb-retry` command. `lamb-start-capture` is the explicit
  retry operation.
- Providing indefinite retries for malformed or contradictory configuration.

## Existing Architecture to Reuse

The app-config path already supports a persistent listener when configuration is
missing or invalid. It uses one `IdleDaemonContext`, one bounded `OperationLane`,
and one serialized worker for capture mutations. Status and daemon stop bypass
the lane so they remain available while an operation is queued.

The design generalizes that ownership model to both app and legacy
configuration. Legacy `run_capture_config` will no longer own the full process
lifetime. Its backend/session construction becomes one capture-start operation
inside the shared supervisor.

## Static Preparation

Startup is divided into bootstrap, static preparation, listener ownership, and
capture attempt phases.

### Bootstrap

Bootstrap reads the configuration bytes and determines the configuration family.
It derives a control socket path without starting PipeWire or allocating capture
resources.

- A parseable legacy configuration supplies its `controlSocketPath`, even when
  later static validation fails.
- A valid app configuration supplies its daemon socket path.
- Missing or malformed configuration uses `%t/lamb/control.sock`, matching the
  existing app fallback behavior.
- `%t` expansion produces a local `PathBuf`; it does not mutate the source
  configuration object.
- If no runtime directory or safe socket path can be derived, startup exits with
  the typed non-restartable configuration status because no inspectable daemon
  can be created.

### Prepared configuration

Preparation produces either a valid immutable configuration or a permanent
fault. A representative internal model is:

```rust
enum PreparedDaemonConfig {
    Legacy(PreparedLegacyConfig),
    App(PreparedAppConfig),
    Faulted(PreparedConfigFault),
}

struct PreparedLegacyConfig {
    static_config: Arc<LambConfig>,
    session_export_policy: SessionExportPolicy,
}
```

`PreparedConfigFault` retains the configuration family when known, the derived
socket path, a permanent error class, and the operator-facing diagnostic.

For legacy configuration, preparation performs deserialization, static
validation, session-export-policy resolution, and static memory-plan validation.
It does not resolve a live PipeWire target, open a backend, or allocate the
capture arena. The prepared type's fields are private, capture-start APIs receive
only shared references, and no runtime component receives mutable access to
`LambConfig`. Preparation never writes negotiated values into `LambConfig`.

Static preparation completes before the initial socket bind. A preparation error
is stored rather than returned from the process; the daemon binds afterward and
reports the permanent fault. During reload, the already-owned supervisor socket
remains bound while the replacement configuration is prepared. Invalid reloads
do not replace the last valid prepared configuration or start capture. If
invalid reload occurs while capture is running, the active configuration,
session, and lifecycle status remain unchanged and the command returns the
permanent error. If no capture is active, the daemon enters `degraded/faulted`.

## Resolved Runtime State

Capture discovery and startup return explicit runtime facts. The minimum legacy
facts are:

```rust
struct ResolvedCaptureState {
    channel_count: usize,
    sample_rate: u32,
    resolved_target: Option<String>,
}
```

PipeWire derives `channel_count` from resolved `capturePorts`. Fake and JACK use
their existing explicit counts. `CaptureRuntime::build`, backend startup,
`CaptureSession`, and status receive these values directly.

The following mutations are prohibited:

- assigning negotiated channels to `LambConfig.channels`;
- assigning negotiated sample rate to a static field to communicate with later
  startup code;
- using source `Config` as an accumulator for target discovery;
- rerunning static validation against a configuration containing runtime-derived
  field presence.

The immutable legacy configuration remains available for command behavior and
reload comparison. Live status reads channel count and sample rate from the
active session, not from static optional fields. Regression tests compare the
entire static configuration snapshot before and after target resolution rather
than checking only `channels`.

## Supervisor State

The shared runtime uses typed state rather than an unstructured state string.

```text
daemonState:
  ready
  degraded
  stopping

captureState:
  stopped
  starting
  running
  waiting-for-device
  faulted

errorClass:
  permanent
  transient
  fatal

retryPolicy:
  none
  manual
  bounded-backoff
```

The runtime also retains:

- `last_error`;
- retry attempt number;
- optional next retry deadline;
- retry generation;
- active profile when using app configuration;
- actual resolved target when available;
- optional active backend and capture session; and
- the last prepared immutable configuration.

An expected startup error changes this state and returns control to the
supervisor. It does not escape through `main`.

## Status Protocol

`DaemonStatus` adds these camelCase JSON fields:

- `daemonState`;
- `captureState`;
- `errorClass`;
- `lastError`;
- `retryPolicy`;
- `retryAttempt`;
- `nextRetryAt`;
- `activeProfile`; and
- `resolvedTarget`.

`nextRetryAt` is an optional Unix epoch timestamp in seconds.
`resolvedTarget` is the actual backend endpoint or device identifier.

`daemonState`, `captureState`, `retryPolicy`, and `retryAttempt` are always
emitted. `errorClass`, `lastError`, `nextRetryAt`, `activeProfile`, and
`resolvedTarget` are also always emitted and use JSON `null` when not applicable.
State values use the exact kebab-case strings listed above. Golden response tests
cover healthy, permanent-fault, transient-wait, and stopping states.

Existing `state`, `last_error`, and `resolved_target` fields remain present for
compatibility. Existing app-mode `resolved_target` retains its current profile
projection; the new `resolvedTarget` carries the actual endpoint. Healthy legacy
and app clients continue to deserialize the existing fields.

Control command error responses retain their existing message and add optional
`errorClass`, `daemonState`, and `captureState` fields. This lets
`lamb-start-capture` report a structured attempt failure without terminating the
daemon or requiring a second status request.

## Failure Classification

Classification occurs at typed operation boundaries, not by matching rendered
error strings.

| Typed failure boundary | Class | Running state or process result | Retry |
| --- | --- | --- | --- |
| Missing or malformed config with a derived socket | permanent | `degraded/faulted` | manual |
| Static/profile/policy/memory-plan validation | permanent | `degraded/faulted` | manual |
| Unsupported backend or invalid target selector | permanent | `degraded/faulted` | manual |
| Missing target or selected port | transient | `degraded/waiting-for-device` | capped backoff |
| PipeWire/JACK unavailable, open failure, or stream-start failure | transient | `degraded/faulted` | capped backoff |
| Runtime device disconnect | transient | `degraded/waiting-for-device` | capped backoff |
| Runtime allocation failure after a valid plan | transient | `degraded/faulted` | capped backoff after full cleanup |
| Client parse error or saturated operation lane | client error | lifecycle unchanged | none |
| Socket path derivation, stale-path safety, permission, or initial bind failure | pre-listener permanent | exit 78 | systemd must not restart |
| Worker spawn failure, worker panic, listener failure, or invariant violation | fatal | nonzero process exit | systemd safety net |

Permanent failures include:

- malformed or missing required configuration;
- static validation failures;
- invalid profiles;
- unsupported backends;
- contradictory fields such as `channels` plus `capturePorts`;
- invalid target selectors; and
- static memory or export policy that can never succeed without configuration
  changes.

Permanent behavior is `degraded/faulted`, `retryPolicy=manual`, no retry
deadline, and no capture resources. Reload or explicit start reparses and
validates the file before attempting capture.

Transient failures include:

- a configured target or selected port temporarily absent;
- PipeWire temporarily unavailable;
- device disconnection;
- a temporary backend open or stream-start failure; and
- an equivalent recoverable JACK startup failure.

Transient behavior is `degraded/waiting-for-device` when absence is known, or
`degraded/faulted` for another recoverable start failure. The retry policy is
bounded backoff.

Fatal failures include a failed supervisor/control-loop invariant, unrecoverable
listener failure, panic, fatal signal, or corrupted internal state. Fatal errors
leave the supervisor and allow systemd to apply process-level recovery.

## Capture Attempt Ownership

Every capture attempt constructs backend, arena, persistence workspace, session,
and related resources in local attempt-owned values. Runtime state is updated to
`running` only after the complete attempt succeeds.

On failure:

- local PipeWire and JACK backends stop and join through `Drop`;
- fake capture gains equivalent idempotent `Drop` cleanup;
- arena and persistence allocations are dropped before retry scheduling;
- no failed session remains visible as active; and
- permanent faults schedule no new allocation.

An active backend runtime fault emits a typed internal event. The serialized
operation worker consumes that event, removes and drops the failed session, and
applies transient retry policy. Runtime fault recovery therefore does not depend
on an operator polling status.

## Retry Coordination

The retry schedule is fixed for this initial implementation:

```text
1s, 2s, 5s, 10s, 30s, 60s, 60s, ...
```

"Bounded backoff" means the delay is capped at 60 seconds; retries continue at
that maximum interval while the failure remains transient. They stop immediately
on success, stop-capture, reload, explicit start, daemon stop, or
reclassification as permanent.

A condition-variable scheduler sleeps until a retry is due or state changes. It
does not poll while capture is healthy or while a permanent fault is active. At
the deadline it submits an internal retry job to the existing operation lane.

Internal jobs carry a generation number. Reload, explicit start, stop-capture,
and daemon stop increment the generation so queued or waking stale retries become
no-ops. Capture mutations remain serialized with client commands.

If the bounded lane is temporarily full, the scheduler retains the same attempt
and retries admission without allocating capture resources. It does not advance
the capture backoff until an actual attempt runs.

A successful capture start clears the error, retry deadline, and attempt count.

## Command Semantics

### `lamb-reload`

Reload reads and prepares the file without replacing runtime configuration until
preparation succeeds. A corrected permanent fault is cleared. `startMode=auto`
starts capture immediately; manual mode remains `ready/stopped`. An invalid
reload leaves the daemon and socket active and reports the new permanent fault.
If capture is already running, failed preparation leaves that session and its
lifecycle status unchanged. If capture is not running, failed preparation enters
`degraded/faulted`.

### `lamb-start-capture`

Explicit start invalidates pending retries, reloads and prepares current disk
configuration, and performs one immediate capture attempt. A failed attempt
returns a structured error while preserving the daemon. A transient failure then
uses bounded retry; a permanent failure waits for another operator command.

### `lamb-stop-capture`

Stop-capture invalidates pending retries, stops and drops backend/session
resources, and enters `ready/stopped`. It never stops the daemon.

### `lamb-stop`

Stop publishes `stopping`, closes operation admission, cancels the retry
scheduler, stops capture, joins workers, and returns from the daemon. The control
socket guard then removes the pathname. Exit status is zero, so systemd does not
restart an operator-requested stop.

## Control Socket Ownership

The listener and pathname are owned by a small RAII type immediately after a
successful bind. It is responsible for:

- removing a stale pathname before bind using existing safety checks;
- binding the listener;
- applying permissions and nonblocking configuration while guarded;
- unlinking the owned pathname on `Drop`; and
- reporting explicit cleanup failures on normal shutdown while retaining `Drop`
  as the error/unwind fallback.

The operation worker is spawned only after listener setup succeeds. Any later
fallible startup step is covered by the guard. Expected configuration and capture
faults do not drop the guard because they no longer return from the supervisor.

## Systemd Supervision

The daemon handles all expected errors for which it can establish a control
socket. Systemd remains a safety net for process death.

The NixOS unit will use:

```nix
systemd.services.lamb = {
  startLimitIntervalSec = 60;
  startLimitBurst = 3;

  serviceConfig = {
    Restart = "on-failure";
    RestartPreventExitStatus = [ 78 ];
    RestartSec = 5;
  };
};
```

Exit status 78 is reserved for permanent bootstrap failures where no control
socket can be derived or bound, including an unusable user runtime directory.
Ordinary fatal daemon/control-loop errors continue returning a different
nonzero status and are restartable. Signals, panics, and OOM termination remain
restartable. The start limit bounds repeated fatal crashes.

## Test Design

Tests use a deterministic clock and capture-attempt seam. They do not change live
PipeWire configuration and do not wait through production backoff delays.

### Static and runtime separation

- A legacy PipeWire config with `capturePorts` and no `channels` prepares
  successfully.
- A resolved four-channel target produces runtime channel count four without
  mutating `LambConfig.channels` or any other static field.
- Explicit `channels` plus `capturePorts` remains a static error.
- Static errors occur before target-resolution, capture-allocation, and initial
  socket-bind hooks.
- Session-export-policy errors occur before the initial socket bind.
- Existing JACK, fake, and PipeWire validation behavior remains green.

### Socket lifetime

- A deliberately injected error after bind drops the socket owner and leaves no
  pathname.
- Permission or listener-setup failure after bind also unlinks the pathname.
- Normal daemon stop removes the pathname.
- A fatal control-loop return removes the pathname before process exit.

### Permanent degraded state

- A deterministic validation error keeps one daemon process and listener alive.
- Status reports `degraded/faulted`, `permanent`, the exact diagnostic,
  `manual`, attempt zero, and no deadline.
- Missing config, malformed config, unusable runtime directory, stale-path
  safety failure, and initial bind failure produce their specified listener or
  exit-78 behavior.
- A parseable invalid config retains its configured socket path; malformed or
  missing config uses the default runtime path.
- No automatic attempt or capture arena allocation occurs while faulted.
- Correcting configuration followed by reload recovers using the same PID and
  socket and obeys `startMode`.

### Transient retry state

- A missing PipeWire target enters `waiting-for-device`.
- Attempts follow `1, 2, 5, 10, 30, 60` second deadlines.
- Retries retain daemon PID and socket identity.
- Instrumented counters prove each failed attempt leaves zero backend threads,
  sessions, arenas, persistence workspaces, and attempt-owned descriptors.
- A successful retry resets attempt and deadline state.
- Stop-capture, reload, and explicit start invalidate stale retries.
- A runtime disconnect schedules recovery without a status request.

### Fatal and healthy behavior

- An injected fatal supervisor/control-loop failure exits nonzero.
- A module-policy flake check proves that fatal nonzero exits remain restartable
  and status 78 is excluded.
- The start-limit options evaluate to 60 seconds and three starts.
- Healthy capture schedules no timer wakeups, extra attempts, or polling.
- Existing daemon command latency and normal startup tests remain green.
- Golden JSON tests prove new fields are stable and existing legacy/app fields
  retain their previous values and types.

## Verification

Repository verification includes:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
nix build .#lamb
nix flake check
```

The flake adds `checks.<system>.module-policy`, which evaluates the NixOS module
with a minimal enabled LAMB configuration and asserts `Restart=on-failure`,
`RestartPreventExitStatus=78`, `RestartSec=5`, `startLimitIntervalSec=60`, and
`startLimitBurst=3`. Run it directly with:

```bash
nix build .#checks.x86_64-linux.module-policy
```

This is required because `nix build .#lamb` builds the package but does not
exercise the module.

## Deployment

The supported machine configuration is `~/.site#URIEL-LAB`. Its `lamb` input is
normally GitHub-backed. Deployment will use a one-shot local input override so
the generated TOML and site sources remain unchanged:

```bash
sudo nixos-rebuild switch \
  --flake /home/kalki/.site#URIEL-LAB \
  --impure \
  --override-input lamb path:/home/kalki/agent-work/LastAudioMemoryBuffer
```

After deployment, verification includes:

```bash
sudo systemctl restart lamb.service
systemctl is-active lamb.service
systemctl --no-pager --full status lamb.service
ss -xlpn | rg '/run/user/1002/lamb/control\.sock'
lamb-status --json
lamb-dump
```

The restart counter and main PID are sampled over more than the former
five-second restart interval. Acceptance requires a stable active unit, a live
listener, successful status/dump connections, four configured capture ports,
and no stale socket after an induced test failure.

## Scope Control

Implementation is limited to configuration preparation, daemon/capture state,
retry/event coordination, control status/error serialization, socket ownership,
targeted backend cleanup notification, tests, and the NixOS service policy.
Unrelated persistence, export, audio-routing, and configuration migrations are
excluded.
