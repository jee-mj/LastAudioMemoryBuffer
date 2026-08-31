# Persistent Degraded Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the legacy PipeWire `channels`/`capturePorts` startup regression and keep one inspectable daemon process and control socket alive across expected configuration, target, and capture failures.

**Architecture:** Generalize the existing app-config idle listener into the single supervisor for legacy and app configuration. Static configuration is prepared immutably before the initial socket bind; capture attempts return attempt-owned runtime state that is committed only on success. Typed lifecycle state, a dormant generation-aware retry scheduler, and backend fault notifications keep capture recovery in-process while systemd remains a bounded safety net for fatal process failure.

**Tech Stack:** Rust 2021, serde/serde_json/TOML, Unix domain sockets, PipeWire and JACK backends, standard-library threads/Mutex/Condvar, Cargo tests, Nix flakes, NixOS systemd modules.

**Spec:** `docs/specs/2026-08-29-persistent-degraded-daemon-design.md`

## Global Constraints

- Do not edit `~/.config/lamb/lamb.toml`; it has `capturePorts` and no explicit `channels`.
- Do not change PipeWire, WirePlumber, JACK, or system audio configuration.
- `LambConfig` is immutable after parsing; no runtime component receives `&mut LambConfig`.
- Negotiated channel count, sample rate, and resolved target live only in runtime/session state.
- Complete static validation, static memory planning, and legacy session-export-policy resolution before the initial control socket bind.
- Keep expected configuration and capture errors inside the daemon whenever a control socket can be derived.
- Permanent faults never retry automatically or allocate capture resources.
- Transient retry delays are `1s, 2s, 5s, 10s, 30s, 60s`, capped at 60 seconds thereafter.
- Preserve the same daemon PID and socket across transient retries.
- Preserve existing `state`, `last_error`, and `resolved_target` JSON fields; new camelCase fields are additive.
- Do not add `lamb-retry`; `lamb-start-capture` is the explicit retry command.
- Do not add third-party dependencies.
- Do not commit, amend, push, or otherwise modify repository history unless the user explicitly requests it.
- Use test doubles and synthetic PipeWire graphs; tests must not change live audio configuration.
- Stop and report if an unrelated worktree change directly conflicts with a planned edit; do not revert unrelated changes.

## File Map

- Create `src/daemon_lifecycle.rs`: pure lifecycle transitions, capped backoff, retry scheduler, generation invalidation, clock seam.
- Create `tests/daemon_supervisor.rs`: process-level degraded-state, reload, socket, and exit-code acceptance tests.
- Modify `src/lib.rs`: register the lifecycle module.
- Modify `src/control.rs`: public wire enums, additive status fields, structured command-error context, golden JSON tests.
- Modify `src/config.rs`: separate legacy deserialization from the existing load-and-validate API.
- Modify `src/capture_runtime.rs`: extract allocation-free runtime planning from allocation.
- Modify `src/capture_fake.rs`: idempotent stop and `Drop` cleanup.
- Modify `src/capture_pipewire.rs`: typed target-resolution failures and first-fault notification.
- Modify `src/daemon.rs`: immutable preparation, attempt-owned capture, shared supervisor, socket owner, commands, retries.
- Modify `src/error.rs`: typed non-restartable pre-listener error and process exit-code mapping.
- Modify `src/main.rs`: use typed process exit codes.
- Modify `src/control_server.rs` only if an internal-job admission helper is needed; preserve the generic bounded FIFO.
- Modify `tests/config_validation.rs`: parse-vs-validation and explicit conflict tests.
- Modify `tests/daemon_idle.rs`: existing app fallback compatibility and socket cleanup assertions.
- Modify `tests/daemon_fake.rs`: legacy shared-supervisor compatibility.
- Modify `tests/pipewire_backend.rs`: typed resolution compatibility.
- Modify direct `ControlResponse`/`DaemonStatus` literals in `src/control.rs`, `src/daemon.rs`, `tests/dump_coordinator.rs`, and `tests/threshold_cli.rs` by adding the prescribed default flattened fields without changing existing values.
- Modify `nix/module.nix`: exit 78 wrapper preflight, restart suppression, and start limits.
- Modify `flake.nix`: effective NixOS module-policy check.

---

### Execution Preflight: Contain the Existing Restart Storm

**Files:** None.

**Interfaces:**
- Preserves current failure evidence before service containment.
- Stops repeated multi-gigabyte allocations while implementation and verification run.
- Does not delete the stale socket or edit user configuration.

- [ ] **Step 1: Record current service evidence**

Run:

```bash
systemctl show lamb.service \
  -p ActiveState \
  -p SubState \
  -p MainPID \
  -p NRestarts \
  -p ExecMainStatus
journalctl -u lamb.service -n 20 --no-pager
```

- [ ] **Step 2: Stop the deterministic restart loop**

Run:

```bash
sudo systemctl stop lamb.service
systemctl is-active lamb.service
```

Expected: `inactive`. Do not remove `/run/user/1002/lamb/control.sock`; the new
socket owner and regression tests must prove safe stale-path handling.

---

### Task 1: Lifecycle Wire Contract and Pure State Machine

**Files:**
- Create: `src/daemon_lifecycle.rs`
- Modify: `src/lib.rs:1-22`
- Modify: `src/control.rs:61-70,389-403`
- Modify: every direct literal listed by `rg -n 'ControlResponse \{|DaemonStatus \{' src tests`
- Test: inline tests in `src/daemon_lifecycle.rs` and `src/control.rs`

**Interfaces:**
- Produces public wire enums `DaemonState`, `CaptureState`, `ErrorClass`, and `RetryPolicy` in `control.rs`.
- Produces `DaemonLifecycleStatus`, flattened into `DaemonStatus` so all requested camelCase fields serialize at the top level.
- Produces `ControlErrorContext`, flattened into `ControlResponse` for structured command failures.
- Produces crate-private `LifecycleState`, `RetryInstant`, `RetryClock`, `retry_delay`, and `RETRY_DELAYS` in `daemon_lifecycle.rs`.
- Consumes existing serde conventions and existing status/response fields without renaming them.

- [ ] **Step 1: Add failing backoff and lifecycle transition tests**

Create `src/daemon_lifecycle.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{CaptureState, DaemonState, ErrorClass, RetryPolicy};
    use std::time::Duration;

    #[test]
    fn backoff_sequence_caps_at_sixty_seconds() {
        let actual = (1..=8).map(retry_delay).collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(30),
                Duration::from_secs(60),
                Duration::from_secs(60),
                Duration::from_secs(60),
            ]
        );
    }

    #[test]
    fn permanent_fault_has_manual_policy_without_retry() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_permanent("channels conflicts with capturePorts".to_string());

        assert_eq!(state.daemon_state, DaemonState::Degraded);
        assert_eq!(state.capture_state, CaptureState::Faulted);
        assert_eq!(state.error_class, Some(ErrorClass::Permanent));
        assert_eq!(state.retry_policy, RetryPolicy::Manual);
        assert_eq!(state.retry_attempt, 0);
        assert_eq!(state.next_retry_at, None);
    }

    #[test]
    fn successful_capture_resets_transient_retry_state() {
        let mut state = LifecycleState::ready_stopped(None);
        state.mark_transient(
            CaptureState::WaitingForDevice,
            "target missing".to_string(),
            RetryInstant::from_millis(10_000),
        );
        state.mark_running(None, Some("scarlett".to_string()));

        assert_eq!(state.daemon_state, DaemonState::Ready);
        assert_eq!(state.capture_state, CaptureState::Running);
        assert_eq!(state.error_class, None);
        assert_eq!(state.retry_policy, RetryPolicy::None);
        assert_eq!(state.retry_attempt, 0);
        assert_eq!(state.next_retry_at, None);
    }
}
```

- [ ] **Step 2: Run the focused lifecycle tests and verify RED**

Run:

```bash
cargo test --lib daemon_lifecycle::tests::backoff_sequence_caps_at_sixty_seconds -- --exact
cargo test --lib daemon_lifecycle::tests::permanent_fault_has_manual_policy_without_retry -- --exact
```

Expected: compilation fails because the lifecycle module and types do not exist.

- [ ] **Step 3: Add public wire enums and additive flattened status fields**

Add to `src/control.rs`:

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonState {
    Ready,
    Degraded,
    Stopping,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::Ready
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureState {
    Stopped,
    Starting,
    Running,
    WaitingForDevice,
    Faulted,
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::Stopped
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorClass {
    Permanent,
    Transient,
    Fatal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RetryPolicy {
    None,
    Manual,
    BoundedBackoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DaemonLifecycleStatus {
    #[serde(rename = "daemonState")]
    pub daemon_state: DaemonState,
    #[serde(rename = "captureState")]
    pub capture_state: CaptureState,
    #[serde(rename = "errorClass")]
    pub error_class: Option<ErrorClass>,
    #[serde(rename = "lastError")]
    pub last_error: Option<String>,
    #[serde(rename = "retryPolicy")]
    pub retry_policy: RetryPolicy,
    #[serde(rename = "retryAttempt")]
    pub retry_attempt: u32,
    #[serde(rename = "nextRetryAt")]
    pub next_retry_at: Option<u64>,
    #[serde(rename = "activeProfile")]
    pub active_profile: Option<String>,
    #[serde(rename = "resolvedTarget")]
    pub resolved_target: Option<String>,
}

impl Default for DaemonLifecycleStatus {
    fn default() -> Self {
        Self {
            daemon_state: DaemonState::Ready,
            capture_state: CaptureState::Stopped,
            error_class: None,
            last_error: None,
            retry_policy: RetryPolicy::None,
            retry_attempt: 0,
            next_retry_at: None,
            active_profile: None,
            resolved_target: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ControlErrorContext {
    #[serde(rename = "errorClass", default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClass>,
    #[serde(rename = "daemonState", default, skip_serializing_if = "Option::is_none")]
    pub daemon_state: Option<DaemonState>,
    #[serde(rename = "captureState", default, skip_serializing_if = "Option::is_none")]
    pub capture_state: Option<CaptureState>,
}
```

Extend the existing structs without removing fields:

```rust
pub struct ControlResponse {
    pub ok: bool,
    pub message: String,
    pub status: Option<DaemonStatus>,
    #[serde(flatten, default)]
    pub error_context: ControlErrorContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_outcome: Option<PersistenceOutcomeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_report: Option<ThresholdReport>,
}

pub struct DaemonStatus {
    pub state: String,
    pub active_export_count: u32,
    pub pending_recall_count: u32,
    pub buffer_capacity_seconds: f64,
    pub retained_seconds: f64,
    pub dropped_frames: u64,
    pub target: Option<String>,
    pub resolved_target: Option<String>,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub format: String,
    pub last_error: Option<String>,
    #[serde(flatten, default)]
    pub lifecycle: DaemonLifecycleStatus,
}
```

Add `error_context: ControlErrorContext::default()` and
`lifecycle: DaemonLifecycleStatus::default()` to existing direct literals. Do not
change their prior field values.

- [ ] **Step 4: Implement the pure lifecycle state machine**

Add `pub(crate) mod daemon_lifecycle;` to `src/lib.rs`, then implement:

```rust
use crate::control::{CaptureState, DaemonLifecycleStatus, DaemonState, ErrorClass, RetryPolicy};
use std::time::Duration;

pub(crate) const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

pub(crate) fn retry_delay(attempt: u32) -> Duration {
    let index = attempt.saturating_sub(1) as usize;
    RETRY_DELAYS[index.min(RETRY_DELAYS.len() - 1)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RetryInstant(u64);

impl RetryInstant {
    pub(crate) fn from_millis(value: u64) -> Self {
        Self(value)
    }

    pub(crate) fn as_millis(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_add(self, delay: Duration) -> Option<Self> {
        let millis = u64::try_from(delay.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }
}

pub(crate) trait RetryClock: Send + Sync + 'static {
    fn now(&self) -> RetryInstant;
    fn unix_seconds(&self, instant: RetryInstant) -> u64;
}

pub(crate) struct SystemRetryClock {
    monotonic_origin: Instant,
    unix_origin: SystemTime,
}

impl Default for SystemRetryClock {
    fn default() -> Self {
        Self {
            monotonic_origin: Instant::now(),
            unix_origin: SystemTime::now(),
        }
    }
}

impl RetryClock for SystemRetryClock {
    fn now(&self) -> RetryInstant {
        let millis = self.monotonic_origin.elapsed().as_millis();
        RetryInstant::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
    }

    fn unix_seconds(&self, instant: RetryInstant) -> u64 {
        self.unix_origin
            .checked_add(Duration::from_millis(instant.as_millis()))
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(u64::MAX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleState {
    pub(crate) daemon_state: DaemonState,
    pub(crate) capture_state: CaptureState,
    pub(crate) error_class: Option<ErrorClass>,
    pub(crate) last_error: Option<String>,
    pub(crate) retry_policy: RetryPolicy,
    pub(crate) retry_attempt: u32,
    pub(crate) next_retry_at: Option<RetryInstant>,
    pub(crate) active_profile: Option<String>,
    pub(crate) resolved_target: Option<String>,
    pub(crate) generation: u64,
}
```

Implement `ready_stopped`, `mark_starting`, `mark_permanent`,
`mark_transient`, `mark_running`, `mark_stopped`, and `mark_stopping` with the
state transitions asserted above. Add:

```rust
pub(crate) fn status(&self, clock: &dyn RetryClock) -> DaemonLifecycleStatus {
    DaemonLifecycleStatus {
        daemon_state: self.daemon_state,
        capture_state: self.capture_state,
        error_class: self.error_class,
        last_error: self.last_error.clone(),
        retry_policy: self.retry_policy,
        retry_attempt: self.retry_attempt,
        next_retry_at: self.next_retry_at.map(|at| clock.unix_seconds(at)),
        active_profile: self.active_profile.clone(),
        resolved_target: self.resolved_target.clone(),
    }
}

pub(crate) fn begin_operation(&mut self) -> Result<u64> {
    let next = self
        .generation
        .checked_add(1)
        .ok_or(LambError::ControlInvariant("operation generation overflow"))?;
    self.generation = next;
    Ok(next)
}
```

Import `std::time::{Duration, Instant, SystemTime, UNIX_EPOCH}` and the crate
`LambError`/`Result` aliases. Generation overflow is a fatal control invariant
rather than a wraparound.

- [ ] **Step 5: Add and run golden compatibility tests**

In `src/control.rs`, serialize a status with `state = "capturing"` and assert:

```rust
let json = serde_json::to_value(status).unwrap();
assert_eq!(json["state"], "capturing");
assert_eq!(json["last_error"], serde_json::Value::Null);
assert_eq!(json["daemonState"], "ready");
assert_eq!(json["captureState"], "running");
assert_eq!(json["errorClass"], serde_json::Value::Null);
assert_eq!(json["retryPolicy"], "none");
assert_eq!(json["retryAttempt"], 0);
assert_eq!(json["nextRetryAt"], serde_json::Value::Null);
assert_eq!(json["activeProfile"], serde_json::Value::Null);
assert_eq!(json["resolvedTarget"], "scarlett");
```

Add permanent, transient-wait, stopping, old-response deserialization, and
structured-error golden cases. Run:

```bash
cargo test --lib daemon_lifecycle::tests
cargo test --lib control::tests
```

Expected: PASS with existing wire fields unchanged and new status fields always
present.

- [ ] **Step 6: Inspect the Task 1 diff checkpoint**

Run:

```bash
git diff --check
```

Confirm there are no unrelated protocol changes and no history operation was
performed.

---

### Task 2: Static Legacy Preparation and Allocation-Free Planning

**Files:**
- Modify: `src/config.rs:289-308`
- Modify: `src/capture_runtime.rs:49-124`
- Modify: `src/daemon.rs:284-458`
- Test: `tests/config_validation.rs`
- Test: inline tests in `src/capture_runtime.rs` and `src/daemon.rs`

**Interfaces:**
- Produces `config::parse_config_text(path, text) -> Result<LambConfig>` while preserving `load_config_text` as parse plus static validation.
- Produces `CaptureRuntime::validate_plan(params, sample_rate, channels) -> Result<()>` using the same calculations as `build` without allocation.
- Produces private `PreparedLegacyConfig { static_config: Arc<LambConfig>, session_export_policy: ResolvedExportPolicy, runtime_params: CaptureRuntimeParams }`.
- Produces `prepare_legacy_config(cfg) -> Result<PreparedLegacyConfig>`.
- Consumes statically derived PipeWire channel count from `capturePorts.len()` and existing fake channel count.

- [ ] **Step 1: Add failing parse-versus-validation tests**

In `tests/config_validation.rs`, add:

```rust
#[test]
fn parse_retains_socket_before_static_conflict_is_reported() {
    let text = r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
channels = 4
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
outputDir = "/tmp/lamb-out"
controlSocketPath = "/tmp/lamb-invalid.sock"
controlPermissions = "0600"
"#;
    let path = std::path::Path::new("legacy.toml");
    let parsed = lamb::config::parse_config_text(path, text).unwrap();

    assert_eq!(
        parsed.control_socket_path,
        std::path::PathBuf::from("/tmp/lamb-invalid.sock")
    );
    assert!(parsed
        .validate_static()
        .unwrap_err()
        .to_string()
        .contains("channels conflicts with capturePorts"));
    assert!(lamb::config::load_config_text(path, text).is_err());
}
```

- [ ] **Step 2: Run the parse test and verify RED**

Run: `cargo test --test config_validation parse_retains_socket_before_static_conflict_is_reported -- --exact`

Expected: compilation fails because `parse_config_text` is not public.

- [ ] **Step 3: Split parsing from the existing validated loader**

In `src/config.rs`, implement:

```rust
pub fn parse_config_text(path: &Path, text: &str) -> Result<LambConfig> {
    toml::from_str(text).map_err(|err| {
        LambError::Config(format!("failed to parse {}: {err}", path.display()))
    })
}

pub fn load_config_text(path: &Path, text: &str) -> Result<LambConfig> {
    let cfg = parse_config_text(path, text)?;
    cfg.validate_static()?;
    Ok(cfg)
}
```

Keep `load_config_file` behavior unchanged.

- [ ] **Step 4: Add failing allocation-free planning tests**

Move the geometry fixture currently used by `CaptureRuntime::build` tests into a
helper, then add:

```rust
#[test]
fn validate_plan_rejects_memory_limit_without_allocating() {
    let mut params = test_runtime_params();
    params.memory_max = Some(1);

    let error = CaptureRuntime::validate_plan(params, 48_000, 4).unwrap_err();
    assert!(error.to_string().contains("memory"));
}

#[test]
fn validate_plan_and_build_accept_the_same_geometry() {
    let params = test_runtime_params();
    CaptureRuntime::validate_plan(params.clone(), 48_000, 4).unwrap();
    let (_runtime, _ingress) = CaptureRuntime::build(params, 48_000, 4).unwrap();
}
```

Run: `cargo test --lib capture_runtime::tests::validate_plan_rejects_memory_limit_without_allocating -- --exact`

Expected: compilation fails because `validate_plan` does not exist.

- [ ] **Step 5: Extract one shared runtime plan calculation**

In `src/capture_runtime.rs`, add:

```rust
struct PlannedCaptureRuntime {
    plan: SessionMemoryPlan,
    retention_frames: u64,
    chunk_frames: u32,
    chunk_count: u32,
}

fn plan_runtime(
    params: &CaptureRuntimeParams,
    sample_rate: u32,
    channels: u32,
) -> Result<PlannedCaptureRuntime> {
    let retention_frames = u64::from(params.seconds)
        .checked_mul(u64::from(sample_rate))
        .ok_or_else(|| LambError::Validation("retention frame count overflow".to_string()))?;
    let chunk_frames =
        crate::math::derive_chunk_frames(sample_rate, params.chunk_frames_override)?;
    let chunk_count = retention_frames.div_ceil(u64::from(chunk_frames)).max(1);
    let chunk_count = u32::try_from(chunk_count)
        .map_err(|_| LambError::Validation("chunk count exceeds u32".to_string()))?;
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames,
        channels,
        sample_rate,
        sample_format: SampleFormat::F32Le,
        chunk_frames,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: params.split_when_over_bytes,
        control_queue_capacity: params.control_queue_capacity,
        worker_stack_bytes: params.worker_stack_bytes,
        capture_queue_slots: params.capture_queue_slots,
        capture_slot_frames: chunk_frames,
        capture_worker_stack_bytes: params.capture_worker_stack_bytes,
        io_buffer_bytes_per_channel: params.io_buffer_bytes_per_channel,
        maximum_path_bytes: params.maximum_path_bytes,
        maximum_calibration_seconds: params.maximum_calibration_seconds,
        headroom: params.headroom,
    })?;
    plan.validate_max(params.memory_max)?;
    Ok(PlannedCaptureRuntime {
        plan,
        retention_frames,
        chunk_frames,
        chunk_count,
    })
}
```

Add:

```rust
pub(crate) fn validate_plan(
    params: CaptureRuntimeParams,
    sample_rate: u32,
    channels: u32,
) -> Result<()> {
    plan_runtime(&params, sample_rate, channels).map(|_| ())
}
```

Refactor `build` to call `plan_runtime(&params, sample_rate, channels)` and use
its four fields for the existing allocations. Do not duplicate arithmetic.

- [ ] **Step 6: Add failing immutable preparation tests**

In `src/daemon.rs` tests, add exact static fixtures through the unvalidated parser:

```rust
fn test_legacy_pipewire_config() -> LambConfig {
    config::parse_config_text(
        Path::new("pipewire.toml"),
        r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
  { source = "capture_AUX2", name = "percL" },
  { source = "capture_AUX3", name = "percR" },
]
seconds = 30
sampleRate = 44100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-test-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "/tmp/lamb-test.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#,
    )
    .unwrap()
}

fn test_legacy_fake_config() -> LambConfig {
    config::parse_config_text(
        Path::new("fake.toml"),
        r#"
configVersion = 1
user = "test"
backend = "fake"
channels = 2
channelMap = ["left", "right"]
seconds = 5
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-test-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "/tmp/lamb-test.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#,
    )
    .unwrap()
}
```

Then add:

```rust
#[test]
fn legacy_pipewire_preparation_accepts_capture_ports_without_channels() {
    let cfg = test_legacy_pipewire_config();
    let before = cfg.clone();
    let prepared = prepare_legacy_config(cfg).unwrap();

    assert_eq!(prepared.static_config.as_ref(), &before);
    assert_eq!(prepared.static_config.channels, None);
    assert_eq!(
        prepared.session_export_policy.activity.channels.len(),
        before.capture_ports.len()
    );
}

#[test]
fn legacy_preparation_rejects_explicit_channels_with_capture_ports() {
    let mut cfg = test_legacy_pipewire_config();
    cfg.channels = Some(4);
    let error = prepare_legacy_config(cfg).unwrap_err();
    assert!(error
        .to_string()
        .contains("channels conflicts with capturePorts"));
}

#[test]
fn legacy_policy_error_occurs_during_preparation() {
    let mut cfg = test_legacy_fake_config();
    cfg.output_dir = std::path::PathBuf::from("/tmp/root/../escape");
    assert!(prepare_legacy_config(cfg).is_err());
}
```

Run: `cargo test --lib daemon::tests::legacy_pipewire_preparation_accepts_capture_ports_without_channels -- --exact`

Expected: compilation fails because `PreparedLegacyConfig` and
`prepare_legacy_config` do not exist.

- [ ] **Step 7: Implement immutable legacy preparation**

In `src/daemon.rs`, add private types and helper:

```rust
#[derive(Debug, Clone)]
struct PreparedLegacyConfig {
    static_config: Arc<LambConfig>,
    session_export_policy: ResolvedExportPolicy,
    runtime_params: CaptureRuntimeParams,
}

fn prepare_legacy_config(cfg: LambConfig) -> Result<PreparedLegacyConfig> {
    cfg.validate_static()?;
    let channel_count = match cfg.backend.as_str() {
        "pipewire" => u32::try_from(cfg.resolved_capture_ports()?.len()).map_err(|_| {
            LambError::Validation("capturePorts exceeds supported channel count".to_string())
        })?,
        "fake" => cfg.channels.unwrap_or(2),
        backend => {
            return Err(LambError::Validation(format!(
                "unsupported legacy backend {backend}"
            )))
        }
    };
    let runtime_params = legacy_runtime_params(&cfg);
    CaptureRuntime::validate_plan(runtime_params.clone(), cfg.sample_rate, channel_count)?;
    let session_export_policy = cfg.resolved_session_export_policy()?;
    Ok(PreparedLegacyConfig {
        static_config: Arc::new(cfg),
        session_export_policy,
        runtime_params,
    })
}
```

Extract `legacy_runtime_params(&LambConfig) -> CaptureRuntimeParams` from the
current `run_capture_config` literal. Keep fields private and pass only
`&PreparedLegacyConfig` after this point.

- [ ] **Step 8: Run Task 2 tests and checkpoint**

Run:

```bash
cargo test --test config_validation
cargo test --lib capture_runtime::tests
cargo test --lib daemon::tests::legacy_pipewire_preparation_accepts_capture_ports_without_channels -- --exact
cargo test --lib daemon::tests::legacy_preparation_rejects_explicit_channels_with_capture_ports -- --exact
```

Expected: PASS. Confirm no capture resolver, backend constructor, socket bind, or
arena allocation is called by `prepare_legacy_config`.

---

### Task 3: Resolved Runtime Facts and Attempt-Owned Capture Resources

**Files:**
- Modify: `src/daemon.rs:407-458,618-636,1367-1469`
- Modify: `src/capture_fake.rs:8-46`
- Test: inline tests in `src/daemon.rs` and `src/capture_fake.rs`
- Test: `tests/pipewire_backend.rs`

**Interfaces:**
- Produces `ResolvedCaptureState`, `ResolvedLegacyBackend`, and `ResolvedLegacyCapture` without mutating static config.
- Produces resolver seam `resolve_legacy_capture_with` and production wrapper `resolve_legacy_capture`.
- Produces `ActiveCapture`, which drops backend before session resources.
- Produces `start_legacy_capture(prepared, fault_sink) -> Result<ActiveCapture, CaptureAttemptError>`.
- Extends `CaptureBackend` with the fake backend while retaining JACK/PipeWire ownership.
- Preserves existing public PipeWire signatures in this task.

- [ ] **Step 1: Add the four-channel immutability regression test**

In `src/daemon.rs`, add this exact `ResolvedTarget` fixture:

```rust
fn test_resolved_target(channels: u32, sample_rate: u32) -> ResolvedTarget {
    ResolvedTarget {
        id: Some(70),
        name: "studio-input".to_string(),
        description: Some("Test input".to_string()),
        channels,
        sample_rate,
        format: "F32LE".to_string(),
        source_ports: (0..channels)
            .map(|index| ResolvedSourcePort {
                global_id: 100 + index,
                node_id: 70,
                port_id: index,
                name: format!("capture_AUX{index}"),
            })
            .collect(),
        durable_live_key: None,
    }
}
```

Then add:

```rust
#[test]
fn resolving_four_pipewire_ports_preserves_static_config() {
    let prepared = prepare_legacy_config(test_legacy_pipewire_config()).unwrap();
    let before = prepared.static_config.as_ref().clone();
    let resolved = resolve_legacy_capture_with(&prepared, |_| {
        Ok(test_resolved_target(4, 44_100))
    })
    .unwrap();

    assert_eq!(resolved.state.channel_count, 4);
    assert_eq!(resolved.state.sample_rate, 44_100);
    assert_eq!(prepared.static_config.as_ref(), &before);
    assert_eq!(prepared.static_config.channels, None);
}
```

Run: `cargo test --lib daemon::tests::resolving_four_pipewire_ports_preserves_static_config -- --exact`

Expected: compilation fails because the resolved-state seam does not exist.

- [ ] **Step 2: Add explicit resolved-state types and resolver seam**

In `src/daemon.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCaptureState {
    channel_count: u32,
    sample_rate: u32,
    resolved_target: Option<String>,
}

enum ResolvedLegacyBackend {
    Fake,
    PipeWire {
        config: PipeWireCaptureConfig,
        target: ResolvedTarget,
    },
}

struct ResolvedLegacyCapture {
    state: ResolvedCaptureState,
    channel_names: Vec<String>,
    backend: ResolvedLegacyBackend,
}

fn resolve_legacy_capture_with<F>(
    prepared: &PreparedLegacyConfig,
    resolve_pipewire: F,
) -> Result<ResolvedLegacyCapture>
where
    F: FnOnce(&PipeWireCaptureConfig) -> Result<ResolvedTarget>,
{
    let cfg = prepared.static_config.as_ref();
    match cfg.backend.as_str() {
        "fake" => Ok(ResolvedLegacyCapture {
            state: ResolvedCaptureState {
                channel_count: cfg.channels.unwrap_or(2),
                sample_rate: cfg.sample_rate,
                resolved_target: None,
            },
            channel_names: cfg.channel_map.clone().unwrap_or_default(),
            backend: ResolvedLegacyBackend::Fake,
        }),
        "pipewire" => {
            let config = PipeWireCaptureConfig::from_lamb_config(cfg)?;
            let channel_names = config.channel_names();
            let target = resolve_pipewire(&config)?;
            let resolved_target = Some(match target.id {
                Some(id) => format!("{} ({id})", target.name),
                None => target.name.clone(),
            });
            Ok(ResolvedLegacyCapture {
                state: ResolvedCaptureState {
                    channel_count: target.channels,
                    sample_rate: target.sample_rate,
                    resolved_target,
                },
                channel_names,
                backend: ResolvedLegacyBackend::PipeWire { config, target },
            })
        }
        backend => Err(LambError::Validation(format!(
            "unsupported legacy backend {backend}"
        ))),
    }
}

fn resolve_legacy_capture(
    prepared: &PreparedLegacyConfig,
) -> Result<ResolvedLegacyCapture> {
    resolve_legacy_capture_with(prepared, crate::capture_pipewire::resolve_target)
}
```

Delete the assignments to `cfg.channels` and `cfg.sample_rate`. Do not replace
them with any other static-config mutation.

- [ ] **Step 3: Add failing fake-capture drop tests**

In `src/capture_fake.rs`, add a test-only worker-exit probe and:

```rust
#[test]
fn drop_stops_and_joins_fake_capture_worker() {
    let stop = Arc::new(AtomicBool::new(false));
    let exited = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker_exited = Arc::clone(&exited);
    let handle = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        worker_exited.store(true, Ordering::Release);
    });
    let capture = FakeCapture {
        stop,
        handle: Some(handle),
    };
    drop(capture);
    assert!(exited.load(Ordering::Acquire));
}
```

Run: `cargo test --lib capture_fake::tests::drop_stops_and_joins_fake_capture_worker -- --exact`

Expected: FAIL because dropping `FakeCapture` detaches the producer.

- [ ] **Step 4: Make fake capture cleanup symmetric with JACK and PipeWire**

Refactor without changing the public `start` signature:

```rust
impl FakeCapture {
    fn stop_inner(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}
```

The direct private-field construction is available to the inline test module and
proves that `Drop` joins before returning; no production test hook is needed.

- [ ] **Step 5: Add failing attempt-publication and cleanup tests**

First add a generic seam that production instantiates with `ActiveCapture` and
tests can instantiate with a small value:

```rust
fn start_legacy_capture_with<R, S, T>(
    prepared: &PreparedLegacyConfig,
    resolve_pipewire: R,
    start_resolved: S,
) -> Result<T>
where
    R: FnOnce(&PipeWireCaptureConfig) -> Result<ResolvedTarget>,
    S: FnOnce(&PreparedLegacyConfig, ResolvedLegacyCapture) -> Result<T>,
{
    let resolved = resolve_legacy_capture_with(prepared, resolve_pipewire)?;
    start_resolved(prepared, resolved)
}
```

In the inline test module, define five explicit resource counters:

```rust
#[derive(Clone, Copy)]
enum ProbeResource {
    Backend = 0,
    Session = 1,
    Arena = 2,
    Workspace = 3,
    Descriptor = 4,
}

#[derive(Default)]
struct AttemptResourceCounters {
    live: [AtomicUsize; 5],
}

impl AttemptResourceCounters {
    fn lease(self: &Arc<Self>, resource: ProbeResource) -> AttemptResourceLease {
        self.live[resource as usize].fetch_add(1, Ordering::SeqCst);
        AttemptResourceLease {
            counters: Arc::clone(self),
            resource,
        }
    }

    fn live(&self, resource: ProbeResource) -> usize {
        self.live[resource as usize].load(Ordering::SeqCst)
    }
}

struct AttemptResourceLease {
    counters: Arc<AttemptResourceCounters>,
    resource: ProbeResource,
}

impl Drop for AttemptResourceLease {
    fn drop(&mut self) {
        self.counters.live[self.resource as usize].fetch_sub(1, Ordering::SeqCst);
    }
}
```

Then add:

```rust
#[test]
fn failed_capture_attempt_publishes_no_active_resources() {
    let prepared = prepare_legacy_config(test_legacy_pipewire_config()).unwrap();
    let counters = Arc::new(AttemptResourceCounters::default());
    let attempt_counters = Arc::clone(&counters);
    let result: Result<()> = start_legacy_capture_with(
        &prepared,
        |_| Ok(test_resolved_target(4, 44_100)),
        move |_, _| {
            let _backend = attempt_counters.lease(ProbeResource::Backend);
            let _session = attempt_counters.lease(ProbeResource::Session);
            let _arena = attempt_counters.lease(ProbeResource::Arena);
            let _workspace = attempt_counters.lease(ProbeResource::Workspace);
            let _descriptor = attempt_counters.lease(ProbeResource::Descriptor);
            Err(LambError::Capture("injected attempt failure".to_string()))
        },
    );
    assert!(result.is_err());
    assert_eq!(counters.live(ProbeResource::Backend), 0);
    assert_eq!(counters.live(ProbeResource::Session), 0);
    assert_eq!(counters.live(ProbeResource::Arena), 0);
    assert_eq!(counters.live(ProbeResource::Workspace), 0);
    assert_eq!(counters.live(ProbeResource::Descriptor), 0);
}

#[test]
fn legacy_attempt_passes_resolved_runtime_dimensions_to_builder() {
    let state = start_legacy_capture_with(
        &prepare_legacy_config(test_legacy_pipewire_config()).unwrap(),
        |_| Ok(test_resolved_target(4, 44_100)),
        |_, resolved| Ok(resolved.state),
    )
    .unwrap();
    assert_eq!(state.channel_count, 4);
    assert_eq!(state.sample_rate, 44_100);
}
```

Later supervisor retry tests reuse these counters around the injected capture
starter so every failed attempt must return all five values to zero before the
next attempt begins.

- [ ] **Step 6: Implement attempt-owned active capture**

Add:

```rust
enum CaptureBackend {
    Fake(FakeCapture, Vec<String>),
    Jack(JackCapture, Vec<String>),
    PipeWire(PipeWireCapture, Vec<String>),
}

struct ActiveCapture {
    backend: Option<CaptureBackend>,
    session: Option<Arc<CaptureSession>>,
    resolved: ResolvedCaptureState,
}

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        drop(self.backend.take());
        drop(self.session.take());
    }
}
```

Add `CaptureSession::from_legacy_runtime`, consuming the already resolved policy
and dimensions:

```rust
fn from_legacy_runtime(
    runtime: CaptureRuntime,
    prepared: &PreparedLegacyConfig,
    resolved: &ResolvedCaptureState,
    channel_names: Vec<String>,
) -> Self
```

Implement `start_legacy_capture` so backend/runtime/session values remain local
until all fallible construction succeeds. `CaptureRuntime::build` receives
`resolved.sample_rate` and `resolved.channel_count`; the static config does not.

- [ ] **Step 7: Run Task 3 tests and backend compatibility**

Run:

```bash
cargo test --lib daemon::tests::resolving_four_pipewire_ports_preserves_static_config -- --exact
cargo test --lib daemon::tests::failed_capture_attempt_publishes_no_active_resources -- --exact
cargo test --lib capture_fake::tests
cargo test --test pipewire_backend
```

Expected: PASS. Inspect `git diff -- src/daemon.rs` and confirm no assignment to
`LambConfig.channels` or negotiated `sample_rate` remains.

---

### Task 4: Error-Safe Control Socket and Typed Bootstrap Exit

**Files:**
- Modify: `src/daemon.rs:342-391,677-768`
- Modify: `src/error.rs:1-56`
- Modify: `src/main.rs:122-145`
- Test: inline tests in `src/daemon.rs` and `src/error.rs`
- Test: create initial cases in `tests/daemon_supervisor.rs`

**Interfaces:**
- Produces `ControlSocketOwner::bind`, `listener`, and `cleanup`.
- Produces safe stale-path handling and inode identity checks.
- Produces `LambError::NonRestartableBootstrap`, `LambError::process_exit_code`, and `EX_CONFIG = 78`.
- Consumes only pre-listener failures for exit 78; post-listener fatal errors remain exit 1.

- [ ] **Step 1: Add failing socket-owner cleanup and safety tests**

In `src/daemon.rs`, add:

```rust
#[test]
fn socket_owner_unlinks_after_post_bind_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let error = ControlSocketOwner::bind_with_setup(path.clone(), |_| {
        Err(LambError::Control("injected post-bind failure".to_string()))
    })
    .unwrap_err();
    assert!(error.to_string().contains("injected post-bind failure"));
    assert!(!path.exists());
}

#[test]
fn socket_owner_refuses_regular_stale_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    std::fs::write(&path, b"do not delete").unwrap();
    assert!(ControlSocketOwner::bind(path.clone()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"do not delete");
}

#[test]
fn socket_owner_does_not_unlink_replacement_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.sock");
    let owner = ControlSocketOwner::bind(path.clone()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(owner);
    assert_eq!(std::fs::read(path).unwrap(), b"replacement");
}
```

Run: `cargo test --lib daemon::tests::socket_owner_unlinks_after_post_bind_error -- --exact`

Expected: compilation fails because `ControlSocketOwner` does not exist.

- [ ] **Step 2: Implement inode-aware RAII socket ownership**

Add imports from `std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt}`
and implement:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

#[derive(Debug)]
struct ControlSocketOwner {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    cleaned: bool,
}

impl ControlSocketOwner {
    fn bind(path: PathBuf) -> Result<Self> {
        Self::bind_with_setup(path, |_| Ok(()))
    }

    fn bind_with_setup<F>(path: PathBuf, setup: F) -> Result<Self>
    where
        F: FnOnce(&UnixListener) -> Result<()>,
    {
        remove_stale_control_socket(&path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        let listener = UnixListener::bind(&path).map_err(|source| io_error(&path, source))?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|source| io_error(&path, source))?;
        let owner = Self {
            listener,
            path,
            identity: SocketIdentity {
                dev: metadata.dev(),
                ino: metadata.ino(),
            },
            cleaned: false,
        };
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&owner.path, permissions)
            .map_err(|source| io_error(&owner.path, source))?;
        setup(&owner.listener)?;
        Ok(owner)
    }

    fn listener(&self) -> &UnixListener {
        &self.listener
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.dev() == self.identity.dev
                    && metadata.ino() == self.identity.ino =>
            {
                std::fs::remove_file(&self.path)
                    .map_err(|source| io_error(&self.path, source))?;
            }
            Ok(_) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error(&self.path, source)),
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for ControlSocketOwner {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn remove_stale_control_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path).map_err(|source| io_error(path, source))
        }
        Ok(_) => Err(LambError::Control(format!(
            "refusing to replace non-socket control path {}",
            path.display()
        ))),
    }
}
```

Because `symlink_metadata` does not follow symlinks,
`remove_stale_control_socket` removes only an existing Unix socket and rejects
symlinks, regular files, and directories.

- [ ] **Step 3: Add failing exit-code classification tests**

In `src/error.rs`, add:

```rust
#[test]
fn only_non_restartable_bootstrap_maps_to_exit_78() {
    let bootstrap = LambError::non_restartable_bootstrap(LambError::Control(
        "cannot bind listener".to_string(),
    ));
    assert_eq!(bootstrap.process_exit_code(), 78);
    assert_eq!(LambError::ControlInvariant("worker failed").process_exit_code(), 1);
    assert_eq!(LambError::Config("bad cli input".to_string()).process_exit_code(), 1);
}
```

Run: `cargo test --lib error::tests::only_non_restartable_bootstrap_maps_to_exit_78 -- --exact`

Expected: compilation fails because the typed variant and method do not exist.

- [ ] **Step 4: Add the dedicated bootstrap error and main exit mapping**

In `src/error.rs`:

```rust
pub const EX_CONFIG: i32 = 78;

#[error("cannot establish inspectable daemon: {0}")]
NonRestartableBootstrap(#[source] Box<LambError>),
```

Add:

```rust
impl LambError {
    pub fn non_restartable_bootstrap(source: LambError) -> Self {
        Self::NonRestartableBootstrap(Box::new(source))
    }

    pub fn process_exit_code(&self) -> i32 {
        match self {
            Self::NonRestartableBootstrap(_) => EX_CONFIG,
            _ => 1,
        }
    }
}
```

In `src/main.rs`, replace only the hard-coded error exit:

```rust
if let Err(err) = result {
    eprintln!("lamb: {err}");
    std::process::exit(err.process_exit_code());
}
```

Do not map broad `Config`, `Validation`, `Io`, or `Control` variants to 78.

- [ ] **Step 5: Make listener functions own the socket guard**

Change shared listener signatures to consume `ControlSocketOwner`:

```rust
fn run_idle_listener(
    ctx: Arc<IdleDaemonContext>,
    socket: ControlSocketOwner,
) -> Result<()>;

fn run_idle_listener_with_hook<F>(
    ctx: Arc<IdleDaemonContext>,
    mut socket: ControlSocketOwner,
    before_operation: F,
) -> Result<()>
where
    F: Fn(&ControlRequest) + Send + 'static;
```

Borrow `socket.listener()` in the accept loop. After worker/scheduler shutdown,
call `socket.cleanup()?`. Delete manual `remove_file` calls. Wrap only failures
that prevent initial ownership with `LambError::non_restartable_bootstrap` at the
outer bootstrap boundary.

- [ ] **Step 6: Run Task 4 tests and checkpoint**

Run:

```bash
cargo test --lib daemon::tests::socket_owner
cargo test --lib error::tests::only_non_restartable_bootstrap_maps_to_exit_78 -- --exact
cargo test --test daemon_idle
```

Expected: PASS, normal stop removes the socket, and an injected post-bind return
does not leave a pathname.

---

### Task 5: Route Legacy Startup Through the Shared Supervisor

**Files:**
- Modify: `src/daemon.rs:284-745,864-1264`
- Test: inline tests in `src/daemon.rs`
- Test: `tests/daemon_idle.rs`
- Test: `tests/daemon_fake.rs`

**Interfaces:**
- Produces `ConfigFamily::{App, Legacy}` and extends existing `AppRuntimeState` with lifecycle, prepared legacy config, resolved runtime state, and active fake backend support.
- Produces one startup path that prepares before bind, binds once, then attempts capture.
- Preserves current app idle/fallback behavior and existing session-based recall/dump/clear paths.
- Removes `run_capture_config` only after all legacy behavior is reachable through the shared listener.

- [ ] **Step 1: Add failing permanent-degraded process tests**

In `tests/daemon_idle.rs`, add:

```rust
#[test]
fn invalid_legacy_config_keeps_configured_socket_alive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let text = r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
channels = 4
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-test-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "__SOCKET__"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
    .replace("__SOCKET__", socket.to_str().unwrap());
    fs::write(&config, text).unwrap();
    let watch = SocketWatch::arm(&socket);
    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    watch.wait(&mut child, "invalid legacy daemon");
    let pid = child.id();

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status_output = output_bounded(status_command);
    assert!(status_output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert_eq!(status["errorClass"], "permanent");
    assert_eq!(status["retryPolicy"], "manual");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("channels conflicts with capturePorts"));
    assert_eq!(child.id(), pid);
    assert!(child.try_wait().unwrap().is_none());

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    assert!(output_bounded(stop_command).status.success());
    wait_child_bounded(child);
    assert!(!socket.exists());
}
```

Use existing child/socket/client helpers rather than creating a second protocol
harness. Add a valid legacy fake case that records PID, stops capture, confirms
the child remains alive, and then stops the daemon.

- [ ] **Step 2: Run the process test and verify RED**

Run: `cargo test --test daemon_idle invalid_legacy_config_keeps_configured_socket_alive -- --exact`

Expected: FAIL because legacy validation exits before binding.

- [ ] **Step 3: Extend the existing runtime state minimally**

Keep app configuration fields used by calibration/profile code and add orthogonal
legacy state:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFamily {
    App,
    Legacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureAttemptError {
    class: ErrorClass,
    capture_state: CaptureState,
    message: String,
}

trait LegacyCaptureStarter: Send + Sync {
    fn start(
        &self,
        prepared: &PreparedLegacyConfig,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError>;
}

struct RealLegacyCaptureStarter;

impl LegacyCaptureStarter for RealLegacyCaptureStarter {
    fn start(
        &self,
        prepared: &PreparedLegacyConfig,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
        start_legacy_capture(prepared).map_err(classify_legacy_attempt_error)
    }
}

struct AppRuntimeState {
    config: app_config::AppConfig,
    config_family: ConfigFamily,
    prepared_legacy: Option<PreparedLegacyConfig>,
    resolved_capture: Option<ResolvedCaptureState>,
    lifecycle: LifecycleState,
    state: String,
    last_error: Option<String>,
    config_load_error: Option<String>,
    active_profile: Option<profile::ResolvedProfile>,
    capture: Option<CaptureBackend>,
    capture_health: Option<PipeWireHealth>,
    session: Option<Arc<CaptureSession>>,
    #[cfg(test)]
    test_capture_attached: bool,
}
```

Update every initializer explicitly. Do not replace `config: AppConfig` with a
large generic configuration enum in this restoration.

- [ ] **Step 4: Split bootstrap from preparation and bind after preparation**

Add these private bootstrap types and signatures:

```rust
struct BootstrapConfig {
    config_path: PathBuf,
    text: Option<String>,
    family: ConfigFamily,
    control_socket_path: PathBuf,
}

enum PreparedBootstrap {
    App(app_config::LoadedAppConfig),
    Legacy(PreparedLegacyConfig),
    Faulted {
        family: ConfigFamily,
        fallback_app_config: app_config::AppConfig,
        message: String,
    },
}

fn bootstrap_config(path: &Path) -> Result<BootstrapConfig>;
fn prepare_bootstrap_config(bootstrap: &BootstrapConfig) -> PreparedBootstrap;
fn build_idle_context(
    bootstrap: BootstrapConfig,
    prepared: PreparedBootstrap,
) -> Result<Arc<IdleDaemonContext>>;
fn attempt_configured_start(
    ctx: &IdleDaemonContext,
    legacy_starter: &dyn LegacyCaptureStarter,
);
```

`bootstrap_config` reads the file at most once. A parseable legacy TOML derives
its configured socket path before validation. Missing/malformed input uses
`app_config::default_control_socket_path()` and `expand_runtime_socket_path`.
Only an inability to derive that path returns
`NonRestartableBootstrap`; parse/validation faults become `PreparedBootstrap::Faulted`.

Refactor `run_from_config_path` into this order:

```rust
pub fn run_from_config_path(path: &Path) -> Result<()> {
    let bootstrap = bootstrap_config(path)?;
    let prepared = prepare_bootstrap_config(&bootstrap);
    let socket = ControlSocketOwner::bind(bootstrap.control_socket_path.clone())
        .map_err(LambError::non_restartable_bootstrap)?;
    let ctx = build_idle_context(bootstrap, prepared)?;
    attempt_configured_start(&ctx, &RealLegacyCaptureStarter);
    run_idle_listener(ctx, socket)
}
```

`prepare_bootstrap_config` returns a value containing either prepared config or
a permanent diagnostic; it does not return the permanent diagnostic from the
process. For a parseable invalid legacy file, derive the configured socket path
from `parse_config_text`. For malformed/missing input, use the existing app
fallback `%t/lamb/control.sock` derivation.

All static validation and policy resolution happen before `ControlSocketOwner::bind`.
Live target resolution and capture allocation happen afterward.

- [ ] **Step 5: Publish legacy capture only after full success**

Add `CaptureBackend::Fake`. Adapt `attempt_configured_start` so:

```rust
match legacy_starter.start(prepared) {
    Ok(mut active) => {
        let resolved = active.resolved.clone();
        let session = active.session.as_ref().cloned().unwrap();
        let backend = active.backend.take().unwrap();
        let mut runtime = ctx.runtime.lock().unwrap();
        runtime.capture = Some(backend);
        runtime.session = Some(session);
        runtime.resolved_capture = Some(resolved.clone());
        runtime.lifecycle.mark_running(None, resolved.resolved_target);
    }
    Err(error) => publish_capture_attempt_error(ctx, error),
}
```

Use a dedicated commit method instead of duplicating this block if ownership
requires moving fields. Do not hold the runtime mutex during target resolution,
arena allocation, backend startup, or backend drop/join.

- [ ] **Step 6: Make status project typed lifecycle and live session facts**

Update `idle_status_response` so the old fields retain their current values and:

```rust
lifecycle: runtime.lifecycle.status(clock.as_ref()),
```

For legacy mode, source `sample_rate` and `channel_count` from `CaptureSession`,
and source new `resolvedTarget` from `ResolvedCaptureState`. Never read runtime
dimensions from `LambConfig.channels`.

- [ ] **Step 7: Remove the independent legacy process loop**

After valid fake and invalid legacy tests pass through the shared listener,
remove:

```text
run_capture_config
DaemonContext
the legacy accept loop
legacy-only manual socket cleanup
legacy status code superseded by shared status
```

Retain small session request helpers when the shared route still calls them. Do
not refactor persistence or export handlers.

- [ ] **Step 8: Run shared-supervisor compatibility tests**

Run:

```bash
cargo test --test daemon_idle
cargo test --test daemon_fake
cargo test --lib daemon::tests
cargo test --test config_validation
cargo test --test pipewire_backend
```

Expected: valid legacy fake capture, app idle fallback, invalid app config, and
invalid legacy config all use one listener implementation. The conflict case
stays alive without allocating capture resources.

---

### Task 6: Reload, Start-Capture, Stop-Capture, and Structured Errors

**Files:**
- Modify: `src/daemon.rs:924-1002,1367-1508,2731-2905`
- Modify: `src/control.rs:61-70`
- Test: inline tests in `src/daemon.rs` and `src/control.rs`
- Test: `tests/daemon_supervisor.rs`

**Interfaces:**
- Produces family-aware `reload_daemon_config`, `start_capture`, and `stop_capture` operations on the shared runtime.
- Produces `ControlResponse::success` and `ControlResponse::failure` constructors to avoid literal drift.
- Guarantees invalid reload while running preserves the active config/session/lifecycle.
- Guarantees resource drops occur outside the runtime mutex.

- [ ] **Step 1: Add failing command-state tests**

Add one reusable inline-test harness with this exact contract:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisorSnapshot {
    prepared_fingerprint: String,
    session_identity: Option<usize>,
    lifecycle: LifecycleState,
}

struct SupervisorHarness {
    _temp: tempfile::TempDir,
    config_path: PathBuf,
    ctx: Arc<IdleDaemonContext>,
    starter: ScriptedLegacyCaptureStarter,
    resources: Arc<AttemptResourceCounters>,
}

struct ScriptedLegacyCaptureStarter {
    outcomes: Mutex<VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>>,
}

impl LegacyCaptureStarter for ScriptedLegacyCaptureStarter {
    fn start(
        &self,
        _prepared: &PreparedLegacyConfig,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted capture outcome")
    }
}

impl SupervisorHarness {
    fn running() -> Self;
    fn stopped() -> Self;
    fn stopped_with_outcomes(
        outcomes: VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>,
    ) -> Self;
    fn reload_with_text(&self, text: &str) -> ControlResponse;
    fn start_capture(&self) -> ControlResponse;
    fn stop_capture(&self) -> ControlResponse;
    fn snapshot(&self) -> SupervisorSnapshot;
    fn lifecycle(&self) -> LifecycleState;
    fn stop_flag(&self) -> bool;
    fn live_capture_resources(&self) -> usize;
}
```

`running()` writes a valid temporary legacy fake configuration, installs one
probe-backed active capture, and sets `ready/running`. `stopped()` uses the same
prepared config without an active capture and sets `ready/stopped`.
`reload_with_text` atomically rewrites only the temporary config and calls the
production reload operation with `ScriptedLegacyCaptureStarter`. `snapshot` fingerprints
the prepared config using `toml::to_string`, identifies the session with
`Arc::as_ptr`, and clones lifecycle state. `live_capture_resources` sums the five
Task 3 counters.

Then add daemon unit tests:

```rust
#[test]
fn invalid_reload_while_running_preserves_active_session_and_lifecycle() {
    let harness = SupervisorHarness::running();
    let before = harness.snapshot();
    let response = harness.reload_with_text("not valid toml");

    assert!(!response.ok);
    assert_eq!(response.error_context.error_class, Some(ErrorClass::Permanent));
    assert_eq!(harness.snapshot(), before);
}

#[test]
fn invalid_reload_while_stopped_enters_permanent_fault() {
    let harness = SupervisorHarness::stopped();
    let response = harness.reload_with_text("not valid toml");
    assert!(!response.ok);
    assert_eq!(harness.lifecycle().daemon_state, DaemonState::Degraded);
    assert_eq!(harness.lifecycle().capture_state, CaptureState::Faulted);
    assert_eq!(harness.lifecycle().retry_policy, RetryPolicy::Manual);
}

#[test]
fn stop_capture_releases_capture_without_stopping_daemon() {
    let harness = SupervisorHarness::running();
    harness.stop_capture();
    assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
    assert!(!harness.stop_flag());
    assert_eq!(harness.live_capture_resources(), 0);
}
```

Add these exact cases:

```rust
#[test]
fn corrected_legacy_reload_starts_capture() {
    let harness = SupervisorHarness::stopped_with_outcomes(VecDeque::from([
        Ok(test_active_capture()),
    ]));
    let valid_text = toml::to_string(&test_legacy_fake_config()).unwrap();
    let response = harness.reload_with_text(&valid_text);
    assert!(response.ok);
    assert_eq!(harness.lifecycle().daemon_state, DaemonState::Ready);
    assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
}

#[test]
fn explicit_start_permanent_error_is_structured_and_not_terminal() {
    let harness = SupervisorHarness::stopped_with_outcomes(VecDeque::from([
        Err(CaptureAttemptError {
            class: ErrorClass::Permanent,
            capture_state: CaptureState::Faulted,
            message: "invalid profile".to_string(),
        }),
    ]));
    let response = harness.start_capture();
    assert!(!response.ok);
    assert_eq!(response.error_context.error_class, Some(ErrorClass::Permanent));
    assert_eq!(harness.lifecycle().retry_policy, RetryPolicy::Manual);
    assert!(!harness.stop_flag());
}

#[test]
fn explicit_start_transient_error_is_structured_and_not_terminal() {
    let harness = SupervisorHarness::stopped_with_outcomes(VecDeque::from([
        Err(CaptureAttemptError {
            class: ErrorClass::Transient,
            capture_state: CaptureState::WaitingForDevice,
            message: "target missing".to_string(),
        }),
    ]));
    let response = harness.start_capture();
    assert!(!response.ok);
    assert_eq!(response.error_context.error_class, Some(ErrorClass::Transient));
    assert_eq!(harness.lifecycle().capture_state, CaptureState::WaitingForDevice);
    assert!(!harness.stop_flag());
}
```

Retain and extend existing app reload tests to assert auto mode reaches `running`
and manual mode reaches `ready/stopped`.

- [ ] **Step 2: Run command tests and verify RED**

Run: `cargo test --lib daemon::tests::invalid_reload_while_running_preserves_active_session_and_lifecycle -- --exact`

Expected: FAIL because current reload mutates/stops state before all validation
succeeds.

- [ ] **Step 3: Add response constructors with structured context**

In `src/control.rs`, add constructors that populate every field:

```rust
impl ControlResponse {
    pub(crate) fn success(message: impl Into<String>, status: Option<DaemonStatus>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            status,
            error_context: ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        }
    }

    pub(crate) fn failure(
        message: impl Into<String>,
        status: Option<DaemonStatus>,
        error_class: Option<ErrorClass>,
        daemon_state: DaemonState,
        capture_state: CaptureState,
    ) -> Self {
        Self {
            ok: false,
            message: message.into(),
            status,
            error_context: ControlErrorContext {
                error_class,
                daemon_state: Some(daemon_state),
                capture_state: Some(capture_state),
            },
            persistence_outcome: None,
            threshold_report: None,
        }
    }
}
```

This explicit enum signature keeps `control.rs` independent of the private
daemon lifecycle implementation. Update direct error responses incrementally.

- [ ] **Step 4: Implement two-phase reload and explicit start**

Add these private helpers:

```rust
fn prepare_config_from_disk(
    path: &Path,
    requested_profile: Option<&str>,
) -> std::result::Result<PreparedBootstrap, CaptureAttemptError>;

fn current_capture_is_running(ctx: &IdleDaemonContext) -> bool;

fn command_failure_without_state_mutation(
    ctx: &IdleDaemonContext,
    error: CaptureAttemptError,
) -> ControlResponse;

fn publish_permanent_fault(
    ctx: &IdleDaemonContext,
    error: &CaptureAttemptError,
);

fn current_failure_response(ctx: &IdleDaemonContext) -> ControlResponse;

fn install_prepared_then_follow_start_mode(
    ctx: &IdleDaemonContext,
    prepared: PreparedBootstrap,
    legacy_starter: &dyn LegacyCaptureStarter,
) -> ControlResponse;
```

Use this sequence for reload:

```rust
let replacement = prepare_config_from_disk(&ctx.config_path, requested_profile);
match replacement {
    Err(error) if current_capture_is_running(ctx) => {
        return command_failure_without_state_mutation(ctx, error);
    }
    Err(error) => {
        publish_permanent_fault(ctx, error);
        return current_failure_response(ctx);
    }
    Ok(prepared) => {
        install_prepared_then_follow_start_mode(ctx, prepared, legacy_starter)
    }
}
```

Explicit start rereads and prepares disk config, invalidates any older operation
generation, drops the old capture outside the lock, then performs exactly one
attempt. Publish only the new prepared config after preparation succeeds.

- [ ] **Step 5: Implement stop-capture and daemon-stop ordering**

Under the runtime mutex, take active backend/session values and publish stopped
or stopping state. Release the mutex before dropping/joining resources:

```rust
let active = {
    let mut runtime = ctx.runtime.lock().map_err(lock_error)?;
    runtime.lifecycle.mark_stopped();
    take_active_capture(&mut runtime)
};
drop(active);
```

Daemon stop publishes `stopping`, closes lane admission, joins the operation
worker, stops capture, and finally cleans the owned socket. Preserve direct status
and direct stop routing.

- [ ] **Step 6: Add process-level reload recovery**

In `tests/daemon_supervisor.rs`, start with an invalid legacy fake config, assert
the socket is available, atomically rewrite the temporary config to a valid fake
config, issue `reload`, and assert:

```rust
assert_eq!(child.id(), original_pid);
assert_eq!(socket_identity(&socket), original_socket_identity);
assert_eq!(status["daemonState"], "ready");
assert_eq!(status["captureState"], "running");
assert_eq!(status["errorClass"], serde_json::Value::Null);
```

Use the existing config file replacement pattern and control clients. Do not
touch the live user configuration.

- [ ] **Step 7: Run Task 6 tests and checkpoint**

Run:

```bash
cargo test --lib daemon::tests::invalid_reload_while_running_preserves_active_session_and_lifecycle -- --exact
cargo test --lib daemon::tests::stop_capture_releases_capture_without_stopping_daemon -- --exact
cargo test --test daemon_supervisor corrected_reload_recovers_on_same_pid_and_socket -- --exact
cargo test --test daemon_idle
cargo test --test daemon_fake
```

Expected: PASS. Verify no backend join occurs while holding `runtime`.

---

### Task 7: Generation-Aware In-Process Retry Scheduler

**Files:**
- Modify: `src/daemon_lifecycle.rs`
- Modify: `src/daemon.rs:681-745,864-1002`
- Consume unchanged: generic `OperationLane<T>` and `EnqueueError` in `src/control_server.rs`
- Test: inline tests in `src/daemon_lifecycle.rs` and `src/daemon.rs`

**Interfaces:**
- Produces `ScheduledOperation::{Retry, RuntimeFault}` and `RetrySchedulerHandle`.
- Produces `OperationJob::{Client, Internal}` while retaining `OperationLane<T>`.
- Produces generation invalidation for reload, explicit start, stop-capture, and daemon stop.
- Consumes the pure `LifecycleState` backoff transitions from Task 1.

- [ ] **Step 1: Add failing deterministic scheduler tests**

Add a `ManualClock` that stores logical milliseconds and wakes the scheduler on
advance. Use this exact test-support contract:

```rust
#[derive(Default)]
struct ManualClock {
    now_millis: AtomicU64,
    unix_origin_seconds: u64,
}

impl RetryClock for ManualClock {
    fn now(&self) -> RetryInstant {
        RetryInstant::from_millis(self.now_millis.load(Ordering::SeqCst))
    }

    fn unix_seconds(&self, instant: RetryInstant) -> u64 {
        self.unix_origin_seconds + instant.as_millis() / 1_000
    }
}

struct SchedulerHarness {
    clock: Arc<ManualClock>,
    submitted: Arc<(Mutex<Vec<ScheduledOperation>>, Condvar)>,
    lane_full: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<LifecycleState>>,
    handle: RetrySchedulerHandle,
    worker: Option<JoinHandle<()>>,
}

impl SchedulerHarness {
    fn new() -> Self;
    fn with_full_lane() -> Self;
    fn advance(&self, duration: Duration);
    fn schedule_retry(&self, generation: u64, due: RetryInstant);
    fn invalidate(&self, generation: u64);
    fn release_lane(&self);
    fn submitted_jobs(&self) -> Vec<ScheduledOperation>;
    fn take_job(&self) -> ScheduledOperation;
    fn lifecycle(&self) -> LifecycleState;
    fn timed_wait_count(&self) -> u64;
}

impl Drop for SchedulerHarness {
    fn drop(&mut self) {
        self.handle.stop();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
```

`new` starts the production scheduler with a submit closure that appends to the
`submitted` vector and notifies its condition variable. `with_full_lane` returns
`EnqueueError::Full` while `lane_full` is true. `advance` atomically increments
the manual clock and calls a test-only scheduler wake method. `take_job` waits on
the submitted condition variable, so these tests contain no timing sleeps.

Then add:

```rust
#[test]
fn healthy_scheduler_blocks_without_timer_wakeups() {
    let harness = SchedulerHarness::new();
    harness.advance(Duration::from_secs(600));
    assert_eq!(harness.submitted_jobs(), Vec::new());
    assert_eq!(harness.timed_wait_count(), 0);
}

#[test]
fn retry_is_submitted_at_each_capped_deadline() {
    let harness = SchedulerHarness::new();
    harness.schedule_retry(7, RetryInstant::from_millis(1_000));
    harness.advance(Duration::from_millis(999));
    assert_eq!(harness.submitted_jobs(), Vec::new());
    harness.advance(Duration::from_millis(1));
    assert_eq!(
        harness.submitted_jobs(),
        vec![ScheduledOperation::Retry { generation: 7 }]
    );
}

#[test]
fn invalidated_generation_discards_waking_retry() {
    let harness = SchedulerHarness::new();
    harness.schedule_retry(3, RetryInstant::from_millis(1_000));
    harness.invalidate(4);
    harness.advance(Duration::from_secs(2));
    assert!(harness.submitted_jobs().is_empty());
}

#[test]
fn full_lane_retains_job_without_advancing_attempt() {
    let harness = SchedulerHarness::with_full_lane();
    harness.schedule_retry(2, RetryInstant::from_millis(1_000));
    harness.advance(Duration::from_secs(1));
    assert_eq!(harness.lifecycle().retry_attempt, 1);
    harness.release_lane();
    assert_eq!(harness.take_job(), ScheduledOperation::Retry { generation: 2 });
    assert_eq!(harness.lifecycle().retry_attempt, 1);
}
```

Run: `cargo test --lib daemon_lifecycle::tests::healthy_scheduler_blocks_without_timer_wakeups -- --exact`

Expected: compilation fails because no scheduler exists.

- [ ] **Step 2: Implement scheduler state and handle**

In `src/daemon_lifecycle.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeCaptureFault {
    DeviceDisconnected(String),
    BackendFault(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScheduledOperation {
    Retry { generation: u64 },
    RuntimeFault {
        generation: u64,
        fault: RuntimeCaptureFault,
    },
}

#[derive(Clone)]
pub(crate) struct RuntimeFaultSink {
    notify: Arc<dyn Fn(RuntimeCaptureFault) + Send + Sync>,
}

impl RuntimeFaultSink {
    pub(crate) fn new<F>(notify: F) -> Self
    where
        F: Fn(RuntimeCaptureFault) + Send + Sync + 'static,
    {
        Self {
            notify: Arc::new(notify),
        }
    }

    pub(crate) fn notify(&self, fault: RuntimeCaptureFault) {
        (self.notify)(fault);
    }
}

#[derive(Clone)]
pub(crate) struct RetrySchedulerHandle {
    shared: Arc<(Mutex<SchedulerState>, Condvar)>,
}

impl RetrySchedulerHandle {
    pub(crate) fn schedule_retry(&self, generation: u64, due: RetryInstant);
    pub(crate) fn notify_fault(&self, generation: u64, fault: RuntimeCaptureFault);
    pub(crate) fn fault_sink(&self, generation: u64) -> RuntimeFaultSink;
    pub(crate) fn invalidate(&self, generation: u64);
    pub(crate) fn stop(&self);
    #[cfg(test)]
    fn wake_for_test(&self);
}

pub(crate) fn spawn_retry_scheduler<F>(
    handle: RetrySchedulerHandle,
    clock: Arc<dyn RetryClock>,
    submit: F,
) -> std::io::Result<JoinHandle<()>>
where
    F: Fn(ScheduledOperation) -> std::result::Result<(), EnqueueError>
        + Send
        + 'static;
```

The scheduler thread uses indefinite `Condvar::wait` when no retry/fault is
pending and `wait_timeout` only for an active retry deadline. It never locks
daemon runtime state. On lane saturation, retain the same operation and retry
admission without incrementing lifecycle attempt state.

- [ ] **Step 3: Route internal operations through the existing lane**

In `src/daemon.rs`, replace tuple jobs with:

```rust
enum OperationJob {
    Client {
        request: ControlRequest,
        stream: UnixStream,
    },
    Internal(ScheduledOperation),
}
```

Change route and worker signatures to `OperationLane<OperationJob>`. Client jobs
retain existing response behavior. Internal retries:

1. Check generation before any work.
2. Perform one capture attempt.
3. Check generation again before publishing success.
4. On transient failure, call `mark_transient`, release runtime lock, then
   schedule the new deadline.
5. On permanent failure, call `mark_permanent` and schedule nothing.

- [ ] **Step 4: Invalidate retries at every operator boundary**

Add one helper:

```rust
fn begin_new_operation_generation(ctx: &IdleDaemonContext) -> Result<u64> {
    let generation = {
        let mut runtime = ctx.runtime.lock().map_err(lock_error)?;
        runtime.lifecycle.begin_operation()?
    };
    ctx.scheduler.invalidate(generation);
    Ok(generation)
}
```

Call it exactly once at the start of reload, explicit start, stop-capture, and
daemon stop. Runtime-fault sinks and retry jobs carry the generation active when
their capture attempt began.

- [ ] **Step 5: Add daemon ordering and reset tests**

Add an inline `RetryOperationHarness` that owns a real
`OperationLane<OperationJob>`, one operation worker, a manual clock, a scripted
legacy starter, and an `Arc<Mutex<Vec<&'static str>>>` observation log. Its
`before_operation` hook records `"retry"`, `"start-capture"`, or
`"stop-capture"`; its methods are:

```rust
impl RetryOperationHarness {
    fn new(outcomes: VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>) -> Self;
    fn enqueue_internal(&self, operation: ScheduledOperation);
    fn enqueue_request(&self, request: ControlRequest);
    fn wait_for_operations(&self, count: usize);
    fn observed_order(&self) -> Vec<&'static str>;
    fn stop_capture(&self);
    fn starter_call_count(&self) -> usize;
    fn lifecycle(&self) -> LifecycleState;
}

fn test_active_capture() -> ActiveCapture {
    let prepared = prepare_legacy_config(test_legacy_fake_config()).unwrap();
    start_legacy_capture(&prepared).unwrap()
}
```

Then add:

```rust
#[test]
fn retry_and_client_capture_commands_share_fifo_order() {
    let harness = RetryOperationHarness::new(VecDeque::from([
        Ok(test_active_capture()),
        Ok(test_active_capture()),
    ]));
    harness.enqueue_internal(ScheduledOperation::Retry { generation: 1 });
    harness.enqueue_request(ControlRequest::StartCapture {
        profile: None,
        activate: false,
    });
    harness.wait_for_operations(2);
    assert_eq!(harness.observed_order(), vec!["retry", "start-capture"]);
}

#[test]
fn stale_retry_is_noop_after_stop_capture() {
    let harness = RetryOperationHarness::new(VecDeque::new());
    harness.stop_capture();
    let calls_before = harness.starter_call_count();
    harness.enqueue_internal(ScheduledOperation::Retry { generation: 1 });
    harness.wait_for_operations(1);
    assert_eq!(harness.starter_call_count(), calls_before);
    assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
}

#[test]
fn successful_retry_resets_attempt_deadline_and_error() {
    let harness = RetryOperationHarness::new(VecDeque::from([
        Ok(test_active_capture()),
    ]));
    harness.enqueue_internal(ScheduledOperation::Retry { generation: 1 });
    harness.wait_for_operations(1);
    let lifecycle = harness.lifecycle();
    assert_eq!(lifecycle.capture_state, CaptureState::Running);
    assert_eq!(lifecycle.error_class, None);
    assert_eq!(lifecycle.retry_attempt, 0);
    assert_eq!(lifecycle.next_retry_at, None);
}
```

`wait_for_operations` uses a condition variable notified by the observation
hook. `test_active_capture` uses Task 3 probe resources and a tiny fake session.
Do not use production-duration sleeps.

- [ ] **Step 6: Run Task 7 tests and checkpoint**

Run:

```bash
cargo test --lib daemon_lifecycle::tests
cargo test --lib control_server::tests
cargo test --lib daemon::tests::retry_and_client_capture_commands_share_fifo_order -- --exact
cargo test --lib daemon::tests::stale_retry_is_noop_after_stop_capture -- --exact
cargo test --lib daemon::tests::successful_retry_resets_attempt_deadline_and_error -- --exact
```

Expected: PASS with zero healthy polling and capped retry deadlines.

---

### Task 8: Typed PipeWire Failures and Runtime Fault Notification

**Files:**
- Modify: `src/capture_pipewire.rs:13-40,140-188,195-285,282-406`
- Modify: `src/daemon.rs`
- Test: inline tests in `src/capture_pipewire.rs` and `src/daemon.rs`
- Test: `tests/pipewire_backend.rs`

**Interfaces:**
- Produces `TargetResolutionError` with typed missing target/port, invalid selector, and backend-unavailable variants.
- Preserves existing `resolve_target` as a `LambError` compatibility wrapper.
- Produces generation-bound `RuntimeFaultSink` notification from `PipeWireHealth`.
- Extends `LegacyCaptureStarter::start`, `start_legacy_capture`, and the app PipeWire start helper with a `RuntimeFaultSink` argument.
- Ensures first fault notifies once and never calls scheduler code while holding the health mutex or PipeWire graph borrow.

- [ ] **Step 1: Add failing typed-resolution tests**

Using existing synthetic graph fixtures, add:

```rust
#[test]
fn missing_target_is_typed_transient_absence() {
    let error = resolve_target_from_graph_typed(&test_config("missing"), &test_graph())
        .unwrap_err();
    assert!(matches!(error, TargetResolutionError::TargetMissing(_)));
}

#[test]
fn invalid_target_selector_is_typed_permanent() {
    let error = resolve_target_from_graph_typed(&invalid_selector_config(), &test_graph())
        .unwrap_err();
    assert!(matches!(error, TargetResolutionError::InvalidSelector(_)));
}
```

Run: `cargo test --lib capture_pipewire::tests::missing_target_is_typed_transient_absence -- --exact`

Expected: compilation fails because typed resolution does not exist.

- [ ] **Step 2: Add typed resolution while preserving compatibility**

Add:

```rust
#[derive(Debug, thiserror::Error)]
pub(crate) enum TargetResolutionError {
    #[error("PipeWire target not found: {0}")]
    TargetMissing(String),
    #[error("PipeWire capture port not found: {0}")]
    PortMissing(String),
    #[error("{0}")]
    InvalidSelector(String),
    #[error("{0}")]
    BackendUnavailable(String),
}
```

Extract typed pure graph resolution. Keep:

```rust
pub fn resolve_target(cfg: &PipeWireCaptureConfig) -> Result<ResolvedTarget> {
    resolve_target_typed(cfg).map_err(|error| LambError::Capture(error.to_string()))
}
```

The supervisor calls the typed function and maps missing target/port or backend
unavailable to transient; invalid selector maps to permanent. No rendered-string
matching is allowed.

- [ ] **Step 3: Add failing runtime-fault notification tests**

Use the closure-backed sink from Task 7:

```rust
#[test]
fn first_runtime_fault_notifies_once() {
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&notifications);
    let sink = RuntimeFaultSink::new(move |fault| recorded.lock().unwrap().push(fault));
    let health = PipeWireHealth::with_fault_sink(sink.clone());
    health.record_fatal(RuntimeCaptureFault::DeviceDisconnected("gone".to_string()));
    health.record_fatal(RuntimeCaptureFault::BackendFault("later".to_string()));
    assert_eq!(notifications.lock().unwrap().len(), 1);
}

#[test]
fn normal_stop_emits_no_runtime_fault() {
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&notifications);
    let sink = RuntimeFaultSink::new(move |fault| recorded.lock().unwrap().push(fault));
    let health = PipeWireHealth::with_fault_sink(sink.clone());
    health.mark_normal_stop();
    assert!(notifications.lock().unwrap().is_empty());
}
```

- [ ] **Step 4: Notify after releasing PipeWire health locks**

First extend the capture-start contract consistently:

```rust
trait LegacyCaptureStarter: Send + Sync {
    fn start(
        &self,
        prepared: &PreparedLegacyConfig,
        fault_sink: RuntimeFaultSink,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError>;
}

fn start_legacy_capture(
    prepared: &PreparedLegacyConfig,
    fault_sink: RuntimeFaultSink,
) -> Result<ActiveCapture>;
```

Update `RealLegacyCaptureStarter`, `ScriptedLegacyCaptureStarter`, and every call
site in one compiler-guided edit. The app PipeWire branch receives the same sink
from the active operation generation. Fake and JACK may retain but ignore a sink
until they have a typed runtime-disconnect source.

Extend `PipeWireHealth` with an optional `RuntimeFaultSink`. The first-fault path
must:

```rust
let notification = {
    let mut fault = self.fault.lock().unwrap();
    if fault.is_some() {
        None
    } else {
        *fault = Some(message.clone());
        self.fault_sink.clone().map(|sink| (sink, typed_fault))
    }
};
if let Some((sink, fault)) = notification {
    sink.notify(fault);
}
```

Map disconnected/unconnected conditions to `DeviceDisconnected`; map core,
stream, and link failures to `BackendFault`. Bind the sink to the capture
generation used by that attempt.

- [ ] **Step 5: Handle runtime faults in the serialized worker**

For `ScheduledOperation::RuntimeFault`, check generation, take and drop the
active capture outside the mutex, publish transient state, and schedule the next
deadline. Add daemon tests proving notification triggers without calling status,
old-generation notifications are ignored, and resource counters reach zero
before the next attempt.

- [ ] **Step 6: Run Task 8 tests and checkpoint**

Run:

```bash
cargo test --lib capture_pipewire::tests
cargo test --lib daemon::tests::pipewire_disconnect_enqueues_recovery_without_status_request -- --exact
cargo test --lib daemon::tests::old_generation_pipewire_fault_is_ignored -- --exact
cargo test --test pipewire_backend
```

Expected: PASS with no live PipeWire server required.

---

### Task 9: End-to-End Supervisor Regression Matrix

**Files:**
- Create/complete: `tests/daemon_supervisor.rs`
- Modify: `tests/daemon_idle.rs`
- Modify: `tests/daemon_fake.rs`
- Modify: inline daemon test harnesses only where deterministic injection is required

**Interfaces:**
- Consumes all prior supervisor, lifecycle, socket, and capture-starter seams.
- Produces acceptance evidence for stable process/socket identity, permanent faults, corrected reload, transient backoff, resource release, fatal exit, and healthy no-poll behavior.

- [ ] **Step 1: Add permanent-fault process acceptance tests**

Build `tests/daemon_supervisor.rs` around this fixture contract, reusing the
bounded child/watchdog logic from `tests/daemon_idle.rs`:

```rust
fn conflicting_channels_and_capture_ports_toml() -> &'static str {
    r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
channels = 4
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-test-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "__SOCKET__"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
}

fn valid_fake_toml() -> &'static str {
    r#"
configVersion = 1
user = "test"
backend = "fake"
channels = 2
channelMap = ["left", "right"]
seconds = 5
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-test-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "__SOCKET__"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
}

struct DaemonProcessFixture {
    _temp: tempfile::TempDir,
    runtime: PathBuf,
    socket: PathBuf,
    config: PathBuf,
    child: Child,
}

impl DaemonProcessFixture {
    fn spawn(config_text: Option<&str>, configured_socket: bool) -> Self;
    fn pid(&self) -> u32;
    fn socket_identity(&self) -> (u64, u64);
    fn status_json(&self) -> serde_json::Value;
    fn command(&self, command: &str) -> Output;
    fn replace_config(&self, text: &str);
    fn try_wait(&mut self) -> Option<ExitStatus>;
    fn stop_and_wait(self) -> ExitStatus;
}
```

`spawn(None, false)` passes a missing config path and derives the default socket
from its temporary `XDG_RUNTIME_DIR`. `spawn(Some(text), true)` replaces
`__SOCKET__` in the supplied TOML with the temporary socket path. It arms
`SocketWatch` before spawning and returns only after the path exists.

Add exact process cases:

```text
permanent_validation_failure_keeps_pid_and_socket_alive
status_reports_validation_error_and_no_retry
invalid_profile_reports_permanent_no_retry
corrected_reload_recovers_on_same_pid_and_socket
stop_capture_keeps_daemon_and_socket_alive
daemon_stop_exits_zero_and_removes_socket
malformed_config_uses_default_socket_and_stays_inspectable
missing_config_uses_default_socket_and_stays_inspectable
```

The permanent validation case performs the complete stability assertion:

```rust
let mut fixture = DaemonProcessFixture::spawn(
    Some(conflicting_channels_and_capture_ports_toml()),
    true,
);
let pid = fixture.pid();
let socket = fixture.socket_identity();
let status = fixture.status_json();
assert_eq!(status["daemonState"], "degraded");
assert_eq!(status["captureState"], "faulted");
assert_eq!(status["errorClass"], "permanent");
assert_eq!(status["retryPolicy"], "manual");
assert_eq!(status["retryAttempt"], 0);
assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
std::thread::sleep(std::time::Duration::from_secs(6));
assert_eq!(fixture.pid(), pid);
assert_eq!(fixture.socket_identity(), socket);
assert!(fixture.try_wait().is_none());
assert!(fixture.stop_and_wait().success());
```

The six-second wait is used only once to prove the old five-second process loop
is gone; retry schedule tests remain manual-clock tests. The reload case records
the same PID/socket identity, calls `replace_config` with valid fake TOML, sends
`reload`, and requires `ready/running`. The stop-capture case requires
`ready/stopped` while `try_wait()` stays `None`. The malformed and missing cases
require the default socket plus `degraded/faulted`, a permanent error, and a
successful daemon stop. The invalid-profile case uses syntactically valid app
TOML with an unsupported profile backend and requires `permanent/manual`, attempt
zero, and no retry deadline.

- [ ] **Step 2: Add deterministic transient supervisor acceptance tests**

Inside `src/daemon.rs` tests, extend `RetryOperationHarness` with:

```rust
impl RetryOperationHarness {
    fn pid(&self) -> u32;
    fn socket_identity(&self) -> (u64, u64);
    fn advance_to_next_retry(&self);
    fn attempt_count(&self) -> usize;
    fn resource_counts(&self) -> [usize; 5];
    fn scheduler_timed_wait_count(&self) -> u64;
}
```

Run the real listener/lane/scheduler with a scripted starter containing six
transient missing-target failures followed by success. Assert these exact cases:

```text
missing_target_enters_waiting_for_device
transient_deadlines_follow_one_two_five_ten_thirty_sixty
transient_retries_keep_process_and_socket_identity
failed_attempts_release_all_resource_counters
successful_retry_resets_attempt_and_deadline
runtime_disconnect_retries_without_status_poll
healthy_capture_has_no_scheduler_wakeups_or_extra_attempts
```

For the backoff/resource/identity case:

```rust
let pid = harness.pid();
let socket = harness.socket_identity();
let expected_deadlines = [1, 2, 5, 10, 30, 60];
for (index, expected_seconds) in expected_deadlines.into_iter().enumerate() {
    assert_eq!(harness.lifecycle().retry_attempt, (index + 1) as u32);
    assert_eq!(
        harness.lifecycle().next_retry_at,
        Some(harness.clock_now().checked_add(Duration::from_secs(expected_seconds)).unwrap())
    );
    assert_eq!(harness.resource_counts(), [0; 5]);
    harness.advance_to_next_retry();
    assert_eq!(harness.pid(), pid);
    assert_eq!(harness.socket_identity(), socket);
}
```

After the scripted success, require `ready/running`, attempt zero, no deadline,
no error, and no additional scheduler wakeups while advancing the manual clock by
ten minutes. Add `clock_now(&self) -> RetryInstant` to the harness alongside the
methods above.

- [ ] **Step 3: Add pre-listener and fatal exit tests**

In `tests/daemon_supervisor.rs`, add:

```text
initial_bind_failure_exits_78
stale_regular_path_exits_78_without_deleting_file
```

For `initial_bind_failure_exits_78`, set `controlSocketPath` beneath a regular file
so parent creation fails deterministically; assert `status.code() == Some(78)`.
For the stale-path case, put a regular file at the exact socket path; assert exit
78 and unchanged contents. Never damage `/run/user/1002`.

Add an inline daemon boundary test that binds `ControlSocketOwner`, then returns
`LambError::ControlInvariant("injected fatal listener failure")` from a local
fallible closure while the owner is in scope. Assert the result's process exit
code is 1 and dropping the owner removed its path. Task 10's module-policy check
proves this non-78 failure is restartable. The two wrapper runtime-directory
branches are verified by the module-policy derivation, not by changing the live
runtime directory.

- [ ] **Step 4: Run the full targeted matrix**

Run:

```bash
cargo test --test daemon_supervisor
cargo test --test daemon_idle
cargo test --test daemon_fake
cargo test --test config_validation
cargo test --test pipewire_backend
cargo test --lib daemon::tests
cargo test --lib daemon_lifecycle::tests
cargo test --lib capture_fake::tests
cargo test --lib capture_pipewire::tests
```

Expected: PASS. If direct Cargo cannot discover JACK/PipeWire development
libraries, rerun the same commands through `nix develop -c` and report that
environment fact; do not weaken tests.

- [ ] **Step 5: Inspect regression scope**

Run:

```bash
git diff --check
```

Confirm no generated user configuration, audio settings, unrelated persistence,
or repository history changed.

---

### Task 10: NixOS Supervision Policy, Full Verification, and Deployment

**Files:**
- Modify: `nix/module.nix:10-24,84-105`
- Modify: `flake.nix:9-34`
- No edit: `/home/kalki/.site`; use a one-shot local input override

**Interfaces:**
- Produces wrapper exit 78 for the two runtime-directory preflight failures.
- Produces effective `Restart=on-failure`, `RestartPreventExitStatus=[78]`, `RestartSec=5`, `startLimitIntervalSec=60`, and `startLimitBurst=3`.
- Produces `checks.<system>.module-policy` that evaluates effective module values and wrapper exits.
- Preserves exit 1 restart behavior for fatal daemon/control-loop failures and exit 0 for `lamb-stop`.

- [ ] **Step 1: Add the module-policy check before changing the module**

In the `flake-utils.lib.eachDefaultSystem` `let`, add:

```nix
modulePolicyConfig = nixpkgs.lib.nixosSystem {
  inherit system;
  modules = [
    ./nix/module.nix
    {
      system.stateVersion = "26.05";
      users.groups.lamb-test.gid = 987;
      users.users.lamb-test = {
        isSystemUser = true;
        group = "lamb-test";
        home = "/var/lib/lamb-test";
        uid = 987;
      };
      services.lamb = {
        enable = true;
        user = "lamb-test";
        package = lamb;
      };
    }
  ];
};
modulePolicyService = modulePolicyConfig.config.systemd.services.lamb;
```

Change `checks.tests = lamb-tests;` to:

```nix
checks = {
  tests = lamb-tests;
  module-policy =
    assert modulePolicyService.serviceConfig.Restart == "on-failure";
    assert modulePolicyService.serviceConfig.RestartPreventExitStatus == [ 78 ];
    assert modulePolicyService.serviceConfig.RestartSec == 5;
    assert modulePolicyService.startLimitIntervalSec == 60;
    assert modulePolicyService.startLimitBurst == 3;
    pkgs.runCommand "lamb-module-policy" { } ''
      count="$(${pkgs.gnugrep}/bin/grep -c 'exit 78' ${modulePolicyService.serviceConfig.ExecStart})"
      test "$count" -eq 2
      touch "$out"
    '';
};
```

- [ ] **Step 2: Run the policy check and verify RED**

Run: `nix build .#checks.x86_64-linux.module-policy`

Expected: evaluation fails because the effective module lacks the prevented exit
status/start limits and the wrapper contains no `exit 78` branches.

- [ ] **Step 3: Implement the NixOS service policy**

In both runtime-directory wrapper failure branches, replace `exit 1` with
`exit 78`. Add at unit level:

```nix
startLimitIntervalSec = 60;
startLimitBurst = 3;
```

Keep and extend `serviceConfig`:

```nix
Restart = "on-failure";
RestartPreventExitStatus = [ 78 ];
RestartSec = 5;
```

Do not use `Restart=on-abnormal`; fatal Rust/control-loop errors exit nonzero and
must remain restartable.

- [ ] **Step 4: Run full repository verification**

Run from the repository root:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
nix build .#lamb
nix build .#checks.x86_64-linux.module-policy
nix flake check
```

If native Cargo lacks JACK/PipeWire headers, run the Cargo commands as:

```bash
nix develop -c cargo fmt --check
nix develop -c cargo test --all-targets
nix develop -c cargo clippy --all-targets -- -D warnings
```

Record complete exit status and failure counts. Do not claim success from a
partial suite.

- [ ] **Step 5: Inspect final worktree evidence before deployment**

Run:

```bash
git status --short
git diff --check
git diff --stat
git diff -- src/config.rs src/capture_runtime.rs src/capture_fake.rs \
  src/capture_pipewire.rs src/daemon.rs src/daemon_lifecycle.rs \
  src/control.rs src/control_server.rs src/error.rs src/main.rs \
  nix/module.nix flake.nix tests docs
```

Report every changed/untracked file and distinguish the approved spec/plan from
implementation files.

- [ ] **Step 6: Build the host closure with the local LAMB input**

Run before activation:

```bash
nix build --no-link --impure \
  --override-input lamb path:/home/kalki/agent-work/LastAudioMemoryBuffer \
  "/home/kalki/.site#nixosConfigurations.URIEL-LAB.config.system.build.toplevel"
```

Expected: exit 0. This does not edit `/home/kalki/.site/flake.lock`.

- [ ] **Step 7: Deploy through the supported site configuration**

Run:

```bash
sudo nixos-rebuild switch \
  --flake /home/kalki/.site#URIEL-LAB \
  --impure \
  --override-input lamb path:/home/kalki/agent-work/LastAudioMemoryBuffer
```

Expected: the `lamb.service` unit and package switch to the locally verified
source without editing the generated TOML or system audio configuration.

- [ ] **Step 8: Verify live restoration and stability**

Run:

```bash
sudo systemctl restart lamb.service
systemctl is-active lamb.service
systemctl --no-pager --full status lamb.service
systemctl show lamb.service \
  -p Restart \
  -p RestartUSec \
  -p RestartPreventExitStatus \
  -p StartLimitIntervalUSec \
  -p StartLimitBurst \
  -p MainPID \
  -p NRestarts
ss -xlpn | rg '/run/user/1002/lamb/control\.sock'
lamb-status --json
lamb-dump
```

Record `MainPID` and `NRestarts`, wait longer than the former restart interval,
then compare:

```bash
systemctl show lamb.service -p MainPID -p NRestarts
sleep 11
systemctl show lamb.service -p MainPID -p NRestarts
```

Acceptance requires:

- `lamb.service` remains `active`;
- `MainPID` is nonzero and unchanged;
- `NRestarts` does not increase;
- the control socket has a live listener;
- `lamb-status --json` reports `ready/running`, no error, retry attempt zero,
  the actual resolved target, and four channels;
- `lamb-dump` connects without `ECONNREFUSED`;
- the configured four `capturePorts` remain in effect; and
- no stale socket exists in the induced-failure regression tests.

## Requirement Coverage Matrix

- Original A/C, immutable runtime state: Tasks 2 and 3.
- Original B, explicit duplicate remains invalid before PipeWire/socket: Tasks 2 and 5.
- Original D, no stale pathname after post-bind error: Task 4.
- Original E, existing JACK/PipeWire tests: Tasks 3, 8, and 9.
- Lifecycle A-D, permanent active daemon/status/no retry/reload recovery: Tasks 1, 5, 6, and 9.
- Lifecycle E-H, missing target/backoff/same identity/resource release/reset: Tasks 3, 7, 8, and 9.
- Lifecycle I, fatal exit remains restartable: Tasks 4, 9, and 10.
- Lifecycle J, healthy startup has no polling/latency regression: Tasks 7 and 9.
- Systemd bounded safety net: Task 10.
- Full Cargo/Nix/deployment/live evidence: Task 10.
