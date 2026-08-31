use crate::app_config::{self, ConfigLoadState};
use crate::calibration::{ConfiguredInputIdentity, ResolvedLiveInputIdentity};
use crate::capture_arena::CaptureArenaStatus;
use crate::capture_fake::FakeCapture;
use crate::capture_jack::{JackCapture, JackCaptureConfig};
#[cfg(test)]
use crate::capture_pipewire::ResolvedSourcePort;
use crate::capture_pipewire::{
    PipeWireCapture, PipeWireCaptureConfig, PipeWireHealth, PipeWireStartupError, ResolvedTarget,
    TargetResolutionError,
};
use crate::capture_runtime::{
    CaptureRuntime, CaptureRuntimeParams, DEFAULT_CAPTURE_QUEUE_SLOTS,
    DEFAULT_CAPTURE_WORKER_STACK_BYTES, DEFAULT_CONTROL_QUEUE_CAPACITY,
    DEFAULT_IO_BUFFER_BYTES_PER_CHANNEL, DEFAULT_MAXIMUM_PATH_BYTES, DEFAULT_WORKER_STACK_BYTES,
};
use crate::config::{self, LambConfig};
#[cfg(test)]
use crate::control::PersistenceOutcomeResponse;
use crate::control::{
    write_persistence_response, CalibrationEvaluation, CalibrationReportStatus, CaptureState,
    ConfiguredInputReport, ControlRequest, ControlResponse, DaemonState, DaemonStatus, ErrorClass,
    RetryPolicy, StoredThresholdReport, ThresholdChannelReport, ThresholdReport, ThresholdRequest,
};
use crate::control_server::{spawn_operation_worker, EnqueueError, OperationLane};
use crate::daemon_lifecycle::{
    spawn_retry_scheduler, CaptureAttemptId, LifecycleState, RetryClock, RetryInstant,
    RetrySchedulerHandle, RuntimeCaptureFault, RuntimeFaultSink, ScheduledOperation,
    SystemRetryClock,
};
#[cfg(test)]
use crate::dump::DumpOutcome;
use crate::dump::{CommittedPersistenceRef, DumpCoordinator, PolicyPersistenceRequest};
use crate::error::{io_error, LambError, Result};
use crate::export_policy::{ExportCommand, ResolvedExportPolicy};
use crate::persistence_workspace::PersistenceWorkspace;
use crate::profile;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PERSIST_TIMEOUT: Duration = Duration::from_secs(60);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const APP_STAGING_ROOT: &str = "/tmp/LAMB/staging";
static NEXT_CAPTURE_ATTEMPT_ID: AtomicU64 = AtomicU64::new(0);

fn allocate_capture_attempt_id() -> std::result::Result<CaptureAttemptId, CaptureAttemptError> {
    NEXT_CAPTURE_ATTEMPT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()
        .and_then(CaptureAttemptId::checked_after)
        .ok_or_else(|| CaptureAttemptError {
            class: ErrorClass::Fatal,
            capture_state: CaptureState::Faulted,
            message: "capture attempt identity overflow".to_string(),
        })
}

#[cfg(test)]
static AFTER_START_CONFIG_PERSIST_HOOK: Mutex<Option<Arc<dyn Fn() + Send + Sync>>> =
    Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalMutationKind {
    LegacySuccess,
    AppSuccess,
    Failure,
    StopCapture,
    RunningStartNoop,
    PersistenceAdmission,
}

#[cfg(test)]
struct FinalMutationPause {
    kind: FinalMutationKind,
    entered: Arc<std::sync::Barrier>,
    release: Arc<std::sync::Barrier>,
}

/// The preallocated persistence runtime shared by one capture session. The
/// arena is `Arc`-shared so `status` stays responsive while persistence serializes
/// on the workspace mutex; capture never touches either.
struct CaptureSession {
    arena: Arc<crate::capture_arena::CaptureArena>,
    workspace: Mutex<PersistenceWorkspace>,
    coordinator: Arc<DumpCoordinator>,
    sample_rate: u32,
    channel_count: u32,
    profile_name: String,
    policy: Mutex<ResolvedExportPolicy>,
    configured_inputs: Vec<ConfiguredInputIdentity>,
    resolved_live_inputs: Vec<Option<ResolvedLiveInputIdentity>>,
    calibration_sample_frames: u64,
    #[cfg(test)]
    attempt_resource_probes: Vec<Box<dyn Send + Sync>>,
    #[cfg(test)]
    status_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct SessionCalibrationTarget {
    session: Arc<CaptureSession>,
    active_profile: profile::ResolvedProfile,
    channel: u32,
    configured: ConfiguredInputIdentity,
    live: ResolvedLiveInputIdentity,
    sample_rate: u32,
    capacity: u64,
}

impl CaptureSession {
    fn from_legacy_runtime(
        runtime: CaptureRuntime,
        prepared: &PreparedLegacyConfig,
        resolved: &ResolvedCaptureState,
        channel_names: Vec<String>,
    ) -> Self {
        let _ = channel_names;
        Self {
            arena: Arc::new(runtime.arena),
            workspace: Mutex::new(runtime.workspace),
            coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
                runtime.frozen_export_decision,
            )),
            sample_rate: resolved.sample_rate,
            channel_count: resolved.channel_count,
            profile_name: "legacy".to_string(),
            policy: Mutex::new(prepared.session_export_policy.clone()),
            configured_inputs: Vec::new(),
            resolved_live_inputs: Vec::new(),
            calibration_sample_frames: 0,
            #[cfg(test)]
            attempt_resource_probes: Vec::new(),
            #[cfg(test)]
            status_hook: None,
        }
    }

    #[cfg(test)]
    fn from_app_runtime(
        runtime: CaptureRuntime,
        profile: &profile::ResolvedProfile,
        sample_rate: u32,
        resolved_live_inputs: Vec<Option<ResolvedLiveInputIdentity>>,
    ) -> Result<Self> {
        let (configured_inputs, channel_count) =
            validate_app_session_inputs(profile, &resolved_live_inputs)?;
        Ok(Self::from_validated_app_runtime(
            runtime,
            profile,
            sample_rate,
            configured_inputs,
            resolved_live_inputs,
            channel_count,
        ))
    }

    fn from_validated_app_runtime(
        runtime: CaptureRuntime,
        profile: &profile::ResolvedProfile,
        sample_rate: u32,
        configured_inputs: Vec<ConfiguredInputIdentity>,
        resolved_live_inputs: Vec<Option<ResolvedLiveInputIdentity>>,
        channel_count: u32,
    ) -> Self {
        let calibration_sample_frames = runtime.calibration_sample_frames();
        Self {
            arena: Arc::new(runtime.arena),
            workspace: Mutex::new(runtime.workspace),
            coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
                runtime.frozen_export_decision,
            )),
            sample_rate,
            channel_count,
            profile_name: profile.name.clone(),
            policy: Mutex::new(profile.export_policy.clone()),
            configured_inputs,
            resolved_live_inputs,
            calibration_sample_frames,
            #[cfg(test)]
            attempt_resource_probes: Vec::new(),
            #[cfg(test)]
            status_hook: None,
        }
    }

    fn persist_with_delivery<F>(
        &self,
        command: ExportCommand,
        timestamp: &str,
        deliver: F,
    ) -> Result<()>
    where
        F: for<'view> FnOnce(CommittedPersistenceRef<'view>) -> Result<()>,
    {
        let policy = self
            .policy
            .lock()
            .map_err(|_| LambError::Control("session export policy lock poisoned".to_string()))?;
        let mut workspace = self
            .workspace
            .lock()
            .map_err(|_| LambError::Control("persistence workspace lock poisoned".to_string()))?;
        self.coordinator.persist_policy_with_delivery(
            &self.arena,
            &mut workspace,
            PolicyPersistenceRequest {
                command,
                policy: &policy,
                profile: &self.profile_name,
                staging_root: Path::new(APP_STAGING_ROOT),
                timestamp,
            },
            PERSIST_TIMEOUT,
            PERSIST_TIMEOUT,
            deliver,
        )
    }

    /// Allocating compatibility adapter retained solely for legacy unit tests.
    #[cfg(test)]
    fn persist(&self, command: ExportCommand, timestamp: &str) -> Result<DumpOutcome> {
        let policy = self
            .policy
            .lock()
            .map_err(|_| LambError::Control("session export policy lock poisoned".to_string()))?;
        let mut workspace = self
            .workspace
            .lock()
            .map_err(|_| LambError::Control("persistence workspace lock poisoned".to_string()))?;
        self.coordinator.persist_policy(
            &self.arena,
            &mut workspace,
            PolicyPersistenceRequest {
                command,
                policy: &policy,
                profile: &self.profile_name,
                staging_root: Path::new(APP_STAGING_ROOT),
                timestamp,
            },
            PERSIST_TIMEOUT,
        )
    }

    fn clear(&self) -> Result<()> {
        self.coordinator
            .clear_in_order(&self.arena, PERSIST_TIMEOUT)
    }

    fn status(&self) -> Result<CaptureArenaStatus> {
        #[cfg(test)]
        if let Some(hook) = self.status_hook.as_ref() {
            hook();
        }
        self.arena.status(PERSIST_TIMEOUT)
    }

    /// Recovers marked recall/dump transactions before admitting persistence,
    /// using the reserved manifest arenas of this session's workspace, and
    /// logs a summary plus an operator-visible warning for every failed or
    /// indeterminate recovery.
    fn recover_startup(&self, staging_root: &Path) -> crate::recovery::RecoveryScanSummary {
        let policy = self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut workspace = self
            .workspace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut summary = workspace.recover_recall_staging(staging_root, policy.output_dir());
        summary.merge(workspace.recover_dumps(policy.output_dir()));
        log_recovery_summary(&summary);
        summary
    }
}

fn validate_app_session_inputs(
    profile: &profile::ResolvedProfile,
    resolved_live_inputs: &[Option<ResolvedLiveInputIdentity>],
) -> Result<(Vec<ConfiguredInputIdentity>, u32)> {
    let configured_inputs = configured_identities(profile)?;
    let channel_count = u32::try_from(profile.ports.len())
        .map_err(|_| LambError::Capture("profile channel count exceeds u32".to_string()))?;
    if configured_inputs.len() != resolved_live_inputs.len()
        || configured_inputs.len() != profile.ports.len()
        || channel_count == 0
        || configured_inputs
            .iter()
            .zip(resolved_live_inputs)
            .any(|(configured, live)| !session_input_is_coherent(configured, live.as_ref()))
    {
        return Err(LambError::Capture(
            "configured and resolved session input ordering is incoherent".to_string(),
        ));
    }
    Ok((configured_inputs, channel_count))
}

fn session_input_is_coherent(
    configured: &ConfiguredInputIdentity,
    live: Option<&ResolvedLiveInputIdentity>,
) -> bool {
    use crate::calibration::{ConfiguredDeviceSelector, InputBackend, LiveDeviceKeyKind};

    match (&configured.backend, &configured.selector, live) {
        (
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget(_) | ConfiguredDeviceSelector::PipeWireAuto,
            None,
        ) => true,
        (
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget(_) | ConfiguredDeviceSelector::PipeWireAuto,
            Some(live),
        ) => {
            live.backend == InputBackend::PipeWire
                && live.key_kind != LiveDeviceKeyKind::JackSourceClient
                && live.resolved_source == configured.source
        }
        (
            InputBackend::Jack,
            ConfiguredDeviceSelector::JackSourceClient(configured_client),
            Some(live),
        ) => {
            live.backend == InputBackend::Jack
                && live.key_kind == LiveDeviceKeyKind::JackSourceClient
                && live.key_value == *configured_client
                && live.resolved_source == configured.source
                && configured
                    .source
                    .split_once(':')
                    .is_some_and(|(source_client, _)| source_client == configured_client)
        }
        _ => false,
    }
}

fn log_recovery_summary(summary: &crate::recovery::RecoveryScanSummary) {
    if summary.discovered == 0 {
        return;
    }
    eprintln!(
        "lamb: startup recovery: {} discovered, {} completed, {} rolled back, {} pending, {} failed",
        summary.discovered, summary.completed, summary.rolled_back, summary.pending, summary.failed
    );
    for issue in &summary.issues {
        eprintln!(
            "lamb: warning: recovery of {}: {}",
            issue.path.display(),
            issue.error
        );
    }
}

fn legacy_runtime_params(cfg: &LambConfig) -> CaptureRuntimeParams {
    CaptureRuntimeParams {
        seconds: cfg.seconds,
        chunk_frames_override: cfg.chunk_frames,
        memory_max: cfg.memory.max,
        headroom: cfg.memory.headroom,
        split_when_over_bytes: cfg.export.split_when_over_bytes,
        io_buffer_bytes_per_channel: DEFAULT_IO_BUFFER_BYTES_PER_CHANNEL,
        maximum_path_bytes: DEFAULT_MAXIMUM_PATH_BYTES,
        maximum_calibration_seconds: 0,
        capture_queue_slots: DEFAULT_CAPTURE_QUEUE_SLOTS,
        capture_worker_stack_bytes: DEFAULT_CAPTURE_WORKER_STACK_BYTES,
        control_queue_capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
        worker_stack_bytes: DEFAULT_WORKER_STACK_BYTES,
    }
}

#[derive(Debug, Clone)]
struct PreparedLegacyConfig {
    static_config: Arc<LambConfig>,
    session_export_policy: ResolvedExportPolicy,
    runtime_params: CaptureRuntimeParams,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCaptureState {
    channel_count: u32,
    sample_rate: u32,
    resolved_target: Option<String>,
}

#[expect(
    clippy::large_enum_variant,
    reason = "boxing the PipeWire resolution payload would complicate capture-start ownership"
)]
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
    CaptureRuntime::validate_plan(runtime_params, cfg.sample_rate, channel_count)?;
    let session_export_policy = cfg.resolved_session_export_policy()?;
    Ok(PreparedLegacyConfig {
        static_config: Arc::new(cfg),
        session_export_policy,
        runtime_params,
    })
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
                resolved_target: Some(cfg.backend.clone()),
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

#[allow(dead_code)]
fn resolve_legacy_capture(prepared: &PreparedLegacyConfig) -> Result<ResolvedLegacyCapture> {
    resolve_legacy_capture_with(prepared, crate::capture_pipewire::resolve_target)
}

#[cfg(test)]
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
        _fault_sink: RuntimeFaultSink,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError>;

    fn prepare_app(
        &self,
        _profile: &profile::ResolvedProfile,
        _params: CaptureRuntimeParams,
        _fault_sink: RuntimeFaultSink,
    ) -> std::result::Result<Option<PreparedAppCapture>, CaptureAttemptError> {
        Ok(None)
    }
}

struct PreparedAppCapture {
    backend: CaptureBackend,
    runtime: CaptureRuntime,
    health: Option<PipeWireHealth>,
    resolved_live_inputs: Vec<Option<ResolvedLiveInputIdentity>>,
    session_resource_probes: Vec<Box<dyn Send + Sync>>,
}

struct RealLegacyCaptureStarter;

impl LegacyCaptureStarter for RealLegacyCaptureStarter {
    fn start(
        &self,
        prepared: &PreparedLegacyConfig,
        fault_sink: RuntimeFaultSink,
    ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
        if std::env::var_os("LAMB_SKIP_RUNTIME_VALIDATION").is_none() {
            validate_runtime_environment(prepared.static_config.as_ref())
                .map_err(classify_legacy_attempt_error)?;
        }
        start_legacy_capture(prepared, fault_sink)
    }
}

struct ActiveCapture {
    backend: Option<CaptureBackend>,
    session: Option<Arc<CaptureSession>>,
    health: Option<PipeWireHealth>,
    resolved: ResolvedCaptureState,
    #[cfg(test)]
    backend_resource_probe: Option<Box<dyn Send + Sync>>,
}

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        drop(self.backend.take());
        #[cfg(test)]
        drop(self.backend_resource_probe.take());
        drop(self.session.take());
    }
}

fn start_legacy_capture(
    prepared: &PreparedLegacyConfig,
    fault_sink: RuntimeFaultSink,
) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
    if prepared.static_config.backend == "pipewire" {
        return start_legacy_capture_with_pipewire_start(
            prepared,
            fault_sink,
            crate::capture_pipewire::resolve_target_typed,
            PipeWireCapture::start_with_resolved,
        );
    }
    let resolved = resolve_legacy_capture_with(prepared, crate::capture_pipewire::resolve_target)
        .map_err(classify_legacy_attempt_error)?;
    start_resolved_legacy_capture(prepared, resolved, fault_sink)
        .map_err(classify_legacy_attempt_error)
}

fn start_legacy_capture_with_pipewire_start<R, S>(
    prepared: &PreparedLegacyConfig,
    fault_sink: RuntimeFaultSink,
    resolve: R,
    start: S,
) -> std::result::Result<ActiveCapture, CaptureAttemptError>
where
    R: FnOnce(&PipeWireCaptureConfig) -> std::result::Result<ResolvedTarget, TargetResolutionError>,
    S: FnOnce(
        PipeWireCaptureConfig,
        ResolvedTarget,
        CaptureRuntimeParams,
        RuntimeFaultSink,
    ) -> std::result::Result<(PipeWireCapture, CaptureRuntime), PipeWireStartupError>,
{
    let config = PipeWireCaptureConfig::from_lamb_config(prepared.static_config.as_ref())
        .map_err(classify_legacy_attempt_error)?;
    let channel_names = config.channel_names();
    let target = resolve(&config).map_err(classify_pipewire_resolution_error)?;
    let resolved_target = Some(match target.id {
        Some(id) => format!("{} ({id})", target.name),
        None => target.name.clone(),
    });
    eprintln!("lamb: {}", target.log_message());
    let (capture, runtime) = start(config, target.clone(), prepared.runtime_params, fault_sink)
        .map_err(|error| match error {
            PipeWireStartupError::Resolution(error) => classify_pipewire_resolution_error(error),
            PipeWireStartupError::Capture(error) => classify_legacy_attempt_error(error),
        })?;
    let health = Some(capture.health());
    let session = Arc::new(CaptureSession::from_legacy_runtime(
        runtime,
        prepared,
        &ResolvedCaptureState {
            channel_count: target.channels,
            sample_rate: target.sample_rate,
            resolved_target: resolved_target.clone(),
        },
        channel_names.clone(),
    ));
    Ok(ActiveCapture {
        backend: Some(CaptureBackend::PipeWire(capture, channel_names)),
        session: Some(session),
        health,
        resolved: ResolvedCaptureState {
            channel_count: target.channels,
            sample_rate: target.sample_rate,
            resolved_target,
        },
        #[cfg(test)]
        backend_resource_probe: None,
    })
}

fn start_resolved_legacy_capture(
    prepared: &PreparedLegacyConfig,
    resolved: ResolvedLegacyCapture,
    fault_sink: RuntimeFaultSink,
) -> Result<ActiveCapture> {
    let state = resolved.state.clone();
    let channel_names = resolved.channel_names;
    let (backend, runtime) = match resolved.backend {
        ResolvedLegacyBackend::Fake => {
            let (runtime, ingress) = CaptureRuntime::build(
                prepared.runtime_params,
                state.sample_rate,
                state.channel_count,
            )?;
            let capture = FakeCapture::start(
                ingress,
                state.channel_count,
                prepared.static_config.chunk_frames.unwrap_or(25),
            )?;
            (
                CaptureBackend::Fake(capture, channel_names.clone()),
                runtime,
            )
        }
        ResolvedLegacyBackend::PipeWire { config, target } => {
            eprintln!("lamb: {}", target.log_message());
            let (capture, runtime) = PipeWireCapture::start_with_resolved(
                config,
                target,
                prepared.runtime_params,
                fault_sink,
            )
            .map_err(PipeWireStartupError::into_lamb_error)?;
            (
                CaptureBackend::PipeWire(capture, channel_names.clone()),
                runtime,
            )
        }
    };
    let health = match &backend {
        CaptureBackend::PipeWire(capture, _) => Some(capture.health()),
        _ => None,
    };
    let session = Arc::new(CaptureSession::from_legacy_runtime(
        runtime,
        prepared,
        &state,
        channel_names,
    ));
    Ok(ActiveCapture {
        backend: Some(backend),
        session: Some(session),
        health,
        resolved: state,
        #[cfg(test)]
        backend_resource_probe: None,
    })
}

fn app_runtime_params(profile: &profile::ResolvedProfile) -> CaptureRuntimeParams {
    CaptureRuntimeParams {
        seconds: profile.buffer_seconds,
        chunk_frames_override: None,
        memory_max: None,
        headroom: 1.2,
        split_when_over_bytes: crate::math::WAV_SPLIT_DEFAULT_BYTES,
        io_buffer_bytes_per_channel: DEFAULT_IO_BUFFER_BYTES_PER_CHANNEL,
        maximum_path_bytes: DEFAULT_MAXIMUM_PATH_BYTES,
        maximum_calibration_seconds: 30,
        capture_queue_slots: DEFAULT_CAPTURE_QUEUE_SLOTS,
        capture_worker_stack_bytes: DEFAULT_CAPTURE_WORKER_STACK_BYTES,
        control_queue_capacity: DEFAULT_CONTROL_QUEUE_CAPACITY,
        worker_stack_bytes: DEFAULT_WORKER_STACK_BYTES,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigFamily {
    App,
    Legacy,
}

struct BootstrapConfig {
    config_path: PathBuf,
    text: Option<String>,
    family: ConfigFamily,
    control_socket_path: PathBuf,
}

#[cfg_attr(
    test,
    expect(
        clippy::large_enum_variant,
        reason = "boxing prepared lifecycle state would risk late transaction ownership changes"
    )
)]
enum PreparedBootstrap {
    App(app_config::LoadedAppConfig),
    Legacy(PreparedLegacyConfig),
    #[cfg(test)]
    TestAppCandidate {
        candidate: AppRuntimeState,
        entered: Option<Arc<std::sync::Barrier>>,
        release: Option<Arc<std::sync::Barrier>>,
    },
    Faulted {
        family: ConfigFamily,
        fallback_app_config: app_config::AppConfig,
        message: String,
    },
}

fn bootstrap_config(path: &Path) -> Result<BootstrapConfig> {
    let text = match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(LambError::non_restartable_bootstrap(io_error(path, source)));
        }
    };
    let parsed = text
        .as_deref()
        .and_then(|text| toml::from_str::<toml::Value>(text).ok());
    let family = if parsed
        .as_ref()
        .and_then(toml::Value::as_table)
        .is_some_and(|table| table.contains_key("configVersion"))
    {
        ConfigFamily::Legacy
    } else {
        ConfigFamily::App
    };
    let socket_template = match (family, text.as_deref()) {
        (ConfigFamily::Legacy, Some(_)) => parsed
            .as_ref()
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("controlSocketPath"))
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(app_config::default_control_socket_path),
        (ConfigFamily::App, Some(text)) => app_config::parse_config_text(path, text)
            .map(|cfg| cfg.daemon.control_socket_path)
            .unwrap_or_else(|_| app_config::default_control_socket_path()),
        (_, None) => app_config::default_control_socket_path(),
    };
    let control_socket_path = expand_control_socket_path(&socket_template)
        .map_err(LambError::non_restartable_bootstrap)?;
    Ok(BootstrapConfig {
        config_path: path.to_path_buf(),
        text,
        family,
        control_socket_path,
    })
}

fn prepare_bootstrap_config(bootstrap: &BootstrapConfig) -> PreparedBootstrap {
    match (bootstrap.family, bootstrap.text.as_deref()) {
        (ConfigFamily::Legacy, Some(text)) => {
            config::parse_config_text(&bootstrap.config_path, text)
                .and_then(expand_runtime_paths)
                .and_then(prepare_legacy_config)
                .map(PreparedBootstrap::Legacy)
                .unwrap_or_else(|error| PreparedBootstrap::Faulted {
                    family: ConfigFamily::Legacy,
                    fallback_app_config: app_config::AppConfig::default(),
                    message: error.to_string(),
                })
        }
        (ConfigFamily::App, Some(text)) => {
            let loaded = match app_config::parse_config_text(&bootstrap.config_path, text) {
                Ok(config) => app_config::LoadedAppConfig {
                    config,
                    state: ConfigLoadState::Loaded,
                    error: None,
                },
                Err(error) => app_config::LoadedAppConfig {
                    config: app_config::AppConfig::default(),
                    state: ConfigLoadState::Invalid,
                    error: Some(error.to_string()),
                },
            };
            PreparedBootstrap::App(loaded)
        }
        (ConfigFamily::App, None) => PreparedBootstrap::App(app_config::LoadedAppConfig {
            config: app_config::AppConfig::default(),
            state: ConfigLoadState::Missing,
            error: None,
        }),
        (ConfigFamily::Legacy, None) => PreparedBootstrap::Faulted {
            family: ConfigFamily::Legacy,
            fallback_app_config: app_config::AppConfig::default(),
            message: format!("config file not found: {}", bootstrap.config_path.display()),
        },
    }
}

pub fn run_from_config_path(path: &Path) -> Result<()> {
    let bootstrap = bootstrap_config(path)?;
    let prepared = prepare_bootstrap_config(&bootstrap);
    let socket = ControlSocketOwner::bind(bootstrap.control_socket_path.clone())
        .map_err(LambError::non_restartable_bootstrap)?;
    let ctx = build_idle_context(bootstrap, prepared)?;
    attempt_configured_start(&ctx, &RealLegacyCaptureStarter)?;
    run_idle_listener(ctx, socket)
}

fn reconcile_startup_calibrations(
    calibration_root: &Path,
    loaded: &app_config::LoadedAppConfig,
) -> Result<Vec<PathBuf>> {
    if loaded.state != ConfigLoadState::Loaded {
        return Ok(Vec::new());
    }
    let referenced = calibration_references(&loaded.config);
    crate::calibration::CalibrationStore::cleanup_root(calibration_root, &referenced)
}

#[cfg(test)]
fn run_idle_fallback_on_listener<F>(
    config_path: PathBuf,
    control_socket_path: PathBuf,
    reason: String,
    calibration_root: PathBuf,
    socket: ControlSocketOwner,
    before_operation: F,
) -> Result<()>
where
    F: Fn(&ControlRequest) + Send + 'static,
{
    let ctx = Arc::new(IdleDaemonContext {
        config_path,
        control_socket_path,
        calibration_root,
        runtime: Mutex::new(AppRuntimeState {
            config: app_config::AppConfig::default(),
            config_family: ConfigFamily::App,
            prepared_legacy: None,
            resolved_capture: None,
            lifecycle: LifecycleState::ready_stopped(None),
            state: "unconfigured".to_string(),
            last_error: Some(reason.clone()),
            config_load_error: Some(reason),
            active_profile: None,
            capture: None,
            capture_health: None,
            session: None,
            #[cfg(test)]
            test_capture_attached: false,
        }),
        clock: Arc::new(SystemRetryClock::default()),
        scheduler: RetrySchedulerHandle::new(),
        operation_authority: Mutex::new(()),
        operation_epoch: AtomicU64::new(0),
        #[cfg(test)]
        final_mutation_pause: Mutex::new(None),
        stop: AtomicBool::new(false),
        first_fatal: std::sync::OnceLock::new(),
    });

    run_idle_listener_with_hook(ctx, socket, before_operation)
}

#[cfg(test)]
fn finish_legacy_capture_startup_with<L, F, P, T>(
    active: ActiveCapture,
    prepared: &PreparedLegacyConfig,
    listener: L,
    final_listener_setup: F,
    publish: P,
) -> Result<T>
where
    F: FnOnce(&L) -> Result<()>,
    P: FnOnce(ActiveCapture, L) -> Result<T>,
{
    let session = active
        .session
        .as_ref()
        .expect("active capture always owns a session");
    debug_assert_eq!(session.channel_count, active.resolved.channel_count);
    debug_assert_eq!(session.sample_rate, active.resolved.sample_rate);
    debug_assert_eq!(prepared.static_config.backend, "fake");
    final_listener_setup(&listener)?;
    publish(active, listener)
}

fn expand_runtime_paths(mut cfg: LambConfig) -> Result<LambConfig> {
    let socket_path = cfg.control_socket_path.to_string_lossy();
    if socket_path.contains("%t") {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(|_| LambError::Validation("XDG_RUNTIME_DIR does not exist".to_string()))?;
        cfg.control_socket_path = PathBuf::from(socket_path.replace("%t", &runtime_dir));
    }
    Ok(cfg)
}

struct IdleDaemonContext {
    config_path: PathBuf,
    control_socket_path: PathBuf,
    calibration_root: PathBuf,
    runtime: Mutex<AppRuntimeState>,
    clock: Arc<dyn RetryClock>,
    scheduler: RetrySchedulerHandle,
    operation_authority: Mutex<()>,
    operation_epoch: AtomicU64,
    #[cfg(test)]
    final_mutation_pause: Mutex<Option<FinalMutationPause>>,
    stop: AtomicBool,
    first_fatal: std::sync::OnceLock<String>,
}

#[derive(Debug)]
enum OperationJob {
    Client {
        request: ControlRequest,
        stream: UnixStream,
    },
    Internal(ScheduledOperation),
}

#[cfg(test)]
fn pause_before_final_mutation(ctx: &IdleDaemonContext, kind: FinalMutationKind) {
    let pause = {
        let mut configured = ctx.final_mutation_pause.lock().unwrap();
        if configured.as_ref().is_some_and(|pause| pause.kind == kind) {
            configured.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.entered.wait();
        pause.release.wait();
    }
}

#[cfg(not(test))]
fn pause_before_final_mutation(_ctx: &IdleDaemonContext, _kind: FinalMutationKind) {}

fn build_idle_context(
    bootstrap: BootstrapConfig,
    prepared: PreparedBootstrap,
) -> Result<Arc<IdleDaemonContext>> {
    build_idle_context_with_dependencies(
        bootstrap,
        prepared,
        &RealLegacyCaptureStarter,
        &RealLegacyStartupRecovery,
        Arc::new(SystemRetryClock::default()),
    )
}

fn build_idle_context_with_dependencies(
    bootstrap: BootstrapConfig,
    prepared: PreparedBootstrap,
    app_starter: &dyn LegacyCaptureStarter,
    startup_recovery: &dyn LegacyStartupRecovery,
    clock: Arc<dyn RetryClock>,
) -> Result<Arc<IdleDaemonContext>> {
    let calibration_root = crate::calibration::default_state_root()?;
    let scheduler = RetrySchedulerHandle::new();
    let mut runtime = AppRuntimeState {
        config: app_config::AppConfig::default(),
        config_family: bootstrap.family,
        prepared_legacy: None,
        resolved_capture: None,
        lifecycle: LifecycleState::ready_stopped(None),
        state: "unconfigured".to_string(),
        last_error: None,
        config_load_error: None,
        active_profile: None,
        capture: None,
        capture_health: None,
        session: None,
        #[cfg(test)]
        test_capture_attached: false,
    };
    match prepared {
        PreparedBootstrap::App(loaded) => {
            if loaded.state == ConfigLoadState::Loaded {
                match reconcile_startup_calibrations(&calibration_root, &loaded) {
                    Ok(pending) => {
                        for path in pending {
                            eprintln!("lamb: calibration cleanup pending: {}", path.display());
                        }
                    }
                    Err(error) => eprintln!("lamb: calibration startup cleanup failed: {error}"),
                }
            }
            if let Err(error) = apply_loaded_app_config_inner(
                &mut runtime,
                loaded,
                &bootstrap.config_path,
                &calibration_root,
                app_starter,
                startup_recovery,
                clock.now(),
                scheduler.fault_sink(
                    1,
                    allocate_capture_attempt_id()
                        .map_err(|error| LambError::Capture(error.message))?,
                ),
            ) {
                let backend = runtime.capture.take();
                let session = runtime.session.take();
                runtime.capture_health = None;
                runtime.resolved_capture = None;
                let error = classify_legacy_attempt_error(error);
                if error.class == ErrorClass::Fatal {
                    drop_capture_then_session(backend, session);
                    return Err(LambError::Capture(error.message));
                }
                apply_capture_attempt_error_to_state(&mut runtime, error, clock.now())
                    .map_err(|error| LambError::DaemonFatal(error.message))?;
                drop_capture_then_session(backend, session);
            }
        }
        PreparedBootstrap::Legacy(prepared) => {
            runtime.config_family = ConfigFamily::Legacy;
            runtime.prepared_legacy = Some(prepared);
            runtime.state = "starting".to_string();
            runtime.lifecycle.mark_starting(None);
        }
        PreparedBootstrap::Faulted {
            family,
            fallback_app_config,
            message,
        } => {
            runtime.config = fallback_app_config;
            runtime.config_family = family;
            runtime.state = "faulted".to_string();
            runtime.last_error = Some(message.clone());
            runtime.config_load_error = Some(message.clone());
            runtime.lifecycle.mark_permanent(message);
        }
        #[cfg(test)]
        PreparedBootstrap::TestAppCandidate { .. } => {
            unreachable!("test app candidates are transaction-only")
        }
    }
    let ctx = Arc::new(IdleDaemonContext {
        config_path: bootstrap.config_path,
        control_socket_path: bootstrap.control_socket_path,
        calibration_root,
        runtime: Mutex::new(runtime),
        clock,
        scheduler,
        operation_authority: Mutex::new(()),
        operation_epoch: AtomicU64::new(0),
        #[cfg(test)]
        final_mutation_pause: Mutex::new(None),
        stop: AtomicBool::new(false),
        first_fatal: std::sync::OnceLock::new(),
    });
    let (published_health, published_generation) = {
        let runtime = ctx
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (runtime.capture_health.clone(), runtime.lifecycle.generation)
    };
    ctx.scheduler.invalidate(published_generation);
    if let Some(health) = published_health {
        health.arm();
    }
    let scheduled = ctx.runtime.lock().ok().and_then(|runtime| {
        runtime
            .lifecycle
            .next_retry_at
            .map(|due| (runtime.lifecycle.generation, due))
    });
    if let Some((generation, due)) = scheduled {
        ctx.scheduler.schedule_retry(generation, due);
    }
    Ok(ctx)
}

fn classify_legacy_attempt_error(error: LambError) -> CaptureAttemptError {
    let (class, capture_state) = match error {
        LambError::Config(_) | LambError::Validation(_) => {
            (ErrorClass::Permanent, CaptureState::Faulted)
        }
        LambError::DaemonFatal(_)
        | LambError::CaptureInvariant(_)
        | LambError::ControlInvariant(_)
        | LambError::ExportInvariant(_) => (ErrorClass::Fatal, CaptureState::Faulted),
        _ => (ErrorClass::Transient, CaptureState::Faulted),
    };
    CaptureAttemptError {
        class,
        capture_state,
        message: error.to_string(),
    }
}

fn classify_pipewire_resolution_error(error: TargetResolutionError) -> CaptureAttemptError {
    let (class, capture_state) = match error {
        TargetResolutionError::TargetMissing(_)
        | TargetResolutionError::PortMissing(_)
        | TargetResolutionError::TargetChanged(_) => {
            (ErrorClass::Transient, CaptureState::WaitingForDevice)
        }
        TargetResolutionError::BackendUnavailable(_) => {
            (ErrorClass::Transient, CaptureState::Faulted)
        }
        TargetResolutionError::InvalidSelector(_) => (ErrorClass::Permanent, CaptureState::Faulted),
    };
    CaptureAttemptError {
        class,
        capture_state,
        message: error.to_string(),
    }
}

fn runtime_fault_sink_for_attempt(
    ctx: &IdleDaemonContext,
    operation_generation: Option<u64>,
) -> std::result::Result<RuntimeFaultSink, CaptureAttemptError> {
    let generation = match operation_generation {
        Some(generation) => generation,
        None => ctx
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .lifecycle
            .generation
            .checked_add(1)
            .ok_or_else(|| CaptureAttemptError {
                class: ErrorClass::Fatal,
                capture_state: CaptureState::Faulted,
                message: "capture generation overflow".to_string(),
            })?,
    };
    Ok(ctx
        .scheduler
        .fault_sink(generation, allocate_capture_attempt_id()?))
}

fn start_app_pipewire_capture(
    config: PipeWireCaptureConfig,
    params: CaptureRuntimeParams,
    fault_sink: RuntimeFaultSink,
) -> std::result::Result<(PipeWireCapture, CaptureRuntime), CaptureAttemptError> {
    let resolved = crate::capture_pipewire::resolve_target_typed(&config)
        .map_err(classify_pipewire_resolution_error)?;
    eprintln!("lamb: {}", resolved.log_message());
    PipeWireCapture::start_with_resolved(config, resolved, params, fault_sink).map_err(|error| {
        match error {
            PipeWireStartupError::Resolution(error) => classify_pipewire_resolution_error(error),
            PipeWireStartupError::Capture(error) => classify_legacy_attempt_error(error),
        }
    })
}

fn apply_capture_attempt_error_to_state(
    runtime: &mut AppRuntimeState,
    error: CaptureAttemptError,
    now: RetryInstant,
) -> std::result::Result<(), CaptureAttemptError> {
    if error.class == ErrorClass::Fatal {
        return Err(error);
    }
    runtime.state = "faulted".to_string();
    runtime.last_error = Some(error.message.clone());
    runtime.config_load_error = Some(error.message.clone());
    match error.class {
        ErrorClass::Transient => {
            runtime
                .lifecycle
                .mark_transient(error.capture_state, error.message, now);
        }
        ErrorClass::Permanent => runtime.lifecycle.mark_permanent(error.message),
        ErrorClass::Fatal => unreachable!("fatal capture errors are rejected before mutation"),
    }
    Ok(())
}

fn publish_capture_attempt_error(ctx: &IdleDaemonContext, error: CaptureAttemptError) {
    if error.class == ErrorClass::Fatal {
        signal_fatal(ctx, error.message);
        return;
    }
    let authority = lock_operation_authority(ctx);
    let (scheduled, published_generation, active, fatal_message) = {
        let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
        let active = if recovered_poison {
            take_active_capture(&mut runtime)
        } else {
            (None, None)
        };
        let publication = match runtime.lifecycle.begin_operation() {
            Ok(generation) => {
                runtime.state = "faulted".to_string();
                runtime.last_error = Some(error.message.clone());
                runtime.config_load_error = Some(error.message.clone());
                let scheduled = match error.class {
                    ErrorClass::Transient => {
                        runtime.lifecycle.mark_transient(
                            error.capture_state,
                            error.message,
                            ctx.clock.now(),
                        );
                        runtime.lifecycle.next_retry_at.map(|due| (generation, due))
                    }
                    ErrorClass::Permanent => {
                        runtime.lifecycle.mark_permanent(error.message);
                        None
                    }
                    ErrorClass::Fatal => {
                        unreachable!("fatal capture error is handled before publication")
                    }
                };
                (scheduled, generation, None)
            }
            Err(generation_error) => {
                let message = generation_error.to_string();
                (None, runtime.lifecycle.generation, Some(message))
            }
        };
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        let (scheduled, generation, fatal_message) = publication;
        (scheduled, generation, active, fatal_message)
    };
    if fatal_message.is_none() {
        if let Some((generation, due)) = scheduled {
            ctx.scheduler.schedule_retry(generation, due);
        } else {
            ctx.scheduler.invalidate(published_generation);
        }
    }
    drop(authority);
    drop_capture_then_session(active.0, active.1);
    if let Some(message) = fatal_message {
        signal_fatal(ctx, message);
    }
}

trait LegacyStartupRecovery: Send + Sync {
    fn failed_count(&self, session: &CaptureSession) -> Result<usize>;
}

struct RealLegacyStartupRecovery;

impl LegacyStartupRecovery for RealLegacyStartupRecovery {
    fn failed_count(&self, session: &CaptureSession) -> Result<usize> {
        Ok(session.recover_startup(Path::new(APP_STAGING_ROOT)).failed)
    }
}

fn attempt_configured_start(
    ctx: &IdleDaemonContext,
    legacy_starter: &dyn LegacyCaptureStarter,
) -> Result<()> {
    attempt_configured_start_with_recovery(ctx, legacy_starter, &RealLegacyStartupRecovery)
}

struct PreparedCommandConfig {
    prepared: PreparedBootstrap,
    control_socket_path: PathBuf,
}

fn begin_command_operation(
    ctx: &IdleDaemonContext,
) -> std::result::Result<u64, CaptureAttemptError> {
    let authority = lock_operation_authority(ctx);
    if ctx.stop.load(Ordering::SeqCst) {
        return Err(shutdown_attempt_error());
    }
    let token = ctx
        .operation_epoch
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map_err(|_| CaptureAttemptError {
            class: ErrorClass::Fatal,
            capture_state: CaptureState::Faulted,
            message: "operation epoch overflow".to_string(),
        })?
        + 1;
    drop(authority);
    Ok(token)
}

#[derive(Debug, Clone, Copy)]
struct BegunOperation {
    token: u64,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperationEntry {
    Command,
    DirectStop,
}

fn begin_new_operation_generation(
    ctx: &IdleDaemonContext,
    entry: OperationEntry,
) -> std::result::Result<BegunOperation, CaptureAttemptError> {
    let _authority = lock_operation_authority(ctx);
    if ctx.stop.load(Ordering::SeqCst) {
        return Err(shutdown_attempt_error());
    }
    let token = ctx
        .operation_epoch
        .load(Ordering::SeqCst)
        .checked_add(1)
        .ok_or_else(|| CaptureAttemptError {
            class: ErrorClass::Fatal,
            capture_state: CaptureState::Faulted,
            message: "operation epoch overflow".to_string(),
        })?;
    let mut runtime = ctx
        .runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = runtime
        .lifecycle
        .begin_operation()
        .map_err(|error| CaptureAttemptError {
            class: ErrorClass::Fatal,
            capture_state: CaptureState::Faulted,
            message: error.to_string(),
        })?;
    let health_to_rebind = if entry == OperationEntry::Command {
        runtime.capture_health.clone()
    } else {
        None
    };
    ctx.operation_epoch.store(token, Ordering::SeqCst);
    ctx.scheduler.invalidate(generation);
    if entry == OperationEntry::DirectStop {
        ctx.stop.store(true, Ordering::SeqCst);
        runtime.lifecycle.mark_stopping();
        ctx.scheduler.stop();
    }
    ctx.runtime.clear_poison();
    drop(runtime);
    if let Some(health) = health_to_rebind {
        if let Some(attempt_id) = health.attempt_id() {
            health.rebind_and_arm(ctx.scheduler.fault_sink(generation, attempt_id));
        }
    }
    Ok(BegunOperation { token, generation })
}

fn lock_operation_authority(ctx: &IdleDaemonContext) -> std::sync::MutexGuard<'_, ()> {
    ctx.operation_authority
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_runtime_recovering_poison(
    ctx: &IdleDaemonContext,
) -> (std::sync::MutexGuard<'_, AppRuntimeState>, bool) {
    match ctx.runtime.lock() {
        Ok(runtime) => (runtime, false),
        Err(poisoned) => (poisoned.into_inner(), true),
    }
}

fn operation_is_current(ctx: &IdleDaemonContext, token: u64) -> bool {
    !ctx.stop.load(Ordering::SeqCst) && ctx.operation_epoch.load(Ordering::SeqCst) == token
}

fn shutdown_attempt_error() -> CaptureAttemptError {
    CaptureAttemptError {
        class: ErrorClass::Transient,
        capture_state: CaptureState::Stopped,
        message: "daemon is shutting down".to_string(),
    }
}

fn superseded_response(ctx: &IdleDaemonContext) -> ControlResponse {
    let status = idle_status_response(ctx);
    ControlResponse::failure(
        if ctx.stop.load(Ordering::SeqCst) {
            "daemon is shutting down"
        } else {
            "capture operation was superseded"
        },
        Some(status.clone()),
        Some(ErrorClass::Transient),
        status.lifecycle.daemon_state,
        status.lifecycle.capture_state,
    )
}

fn prepare_config_from_disk(
    path: &Path,
    requested_profile: Option<&str>,
) -> std::result::Result<PreparedCommandConfig, CaptureAttemptError> {
    let bootstrap = bootstrap_config(path).map_err(classify_legacy_attempt_error)?;
    let mut prepared = prepare_bootstrap_config(&bootstrap);
    if let PreparedBootstrap::App(loaded) = &mut prepared {
        if loaded.state != ConfigLoadState::Loaded {
            return Err(CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message: loaded
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("config file not found: {}", path.display())),
            });
        }
        if let Some(profile) = requested_profile
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
        {
            if !loaded.config.profiles.contains_key(profile) {
                return Err(CaptureAttemptError {
                    class: ErrorClass::Permanent,
                    capture_state: CaptureState::Faulted,
                    message: format!("profile {profile} does not exist"),
                });
            }
            loaded.config.daemon.active_profile = Some(profile.to_string());
        }
        profile::resolve_active_profile(&loaded.config).map_err(classify_legacy_attempt_error)?;
    }
    match prepared {
        PreparedBootstrap::Faulted { message, .. } => Err(CaptureAttemptError {
            class: ErrorClass::Permanent,
            capture_state: CaptureState::Faulted,
            message,
        }),
        prepared => Ok(PreparedCommandConfig {
            prepared,
            control_socket_path: bootstrap.control_socket_path,
        }),
    }
}

fn validate_candidate_socket(
    ctx: &IdleDaemonContext,
    candidate: &PreparedCommandConfig,
) -> std::result::Result<(), CaptureAttemptError> {
    if candidate.control_socket_path == ctx.control_socket_path {
        Ok(())
    } else {
        Err(CaptureAttemptError {
            class: ErrorClass::Permanent,
            capture_state: CaptureState::Faulted,
            message: format!(
                "candidate control socket {} differs from bound socket {}",
                candidate.control_socket_path.display(),
                ctx.control_socket_path.display()
            ),
        })
    }
}

fn current_capture_is_running(ctx: &IdleDaemonContext) -> bool {
    ctx.runtime
        .lock()
        .map(|runtime| {
            runtime.capture.is_some()
                && runtime.session.is_some()
                && runtime.lifecycle.capture_state == CaptureState::Running
        })
        .unwrap_or(false)
}

fn command_failure_without_state_mutation(
    ctx: &IdleDaemonContext,
    error: CaptureAttemptError,
) -> ControlResponse {
    let lifecycle = ctx
        .runtime
        .lock()
        .map(|runtime| runtime.lifecycle.clone())
        .unwrap_or_else(|_| {
            let mut lifecycle = LifecycleState::ready_stopped(None);
            lifecycle.mark_permanent("runtime state lock poisoned".to_string());
            lifecycle
        });
    ControlResponse::failure(
        error.message,
        Some(idle_status_response(ctx)),
        Some(error.class),
        lifecycle.daemon_state,
        lifecycle.capture_state,
    )
}

fn fatal_failure_response(ctx: &IdleDaemonContext, message: String) -> ControlResponse {
    let status = idle_status_response(ctx);
    ControlResponse::failure(
        message,
        Some(status.clone()),
        Some(ErrorClass::Fatal),
        status.lifecycle.daemon_state,
        status.lifecycle.capture_state,
    )
}

fn command_attempt_failure(
    ctx: &IdleDaemonContext,
    token: u64,
    error: CaptureAttemptError,
    preserve_running: bool,
) -> ControlResponse {
    command_attempt_failure_with_generation(ctx, token, None, error, preserve_running)
}

fn command_attempt_failure_with_generation(
    ctx: &IdleDaemonContext,
    token: u64,
    operation_generation: Option<u64>,
    error: CaptureAttemptError,
    preserve_running: bool,
) -> ControlResponse {
    let authority = lock_operation_authority(ctx);
    if !operation_is_current(ctx, token) {
        drop(authority);
        return superseded_response(ctx);
    }
    if error.class == ErrorClass::Fatal {
        let message = error.message;
        drop(authority);
        signal_fatal(ctx, message.clone());
        return fatal_failure_response(ctx, message);
    }
    if preserve_running && current_capture_is_running(ctx) {
        let response = command_failure_without_state_mutation(ctx, error);
        drop(authority);
        return response;
    }
    let (scheduled, published_generation, active, fatal_message) = {
        let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
        if !operation_is_current(ctx, token) {
            if recovered_poison {
                ctx.runtime.clear_poison();
            }
            drop(runtime);
            drop(authority);
            return superseded_response(ctx);
        }
        pause_before_final_mutation(ctx, FinalMutationKind::Failure);
        let generation = match operation_generation {
            Some(generation) if runtime.lifecycle.generation == generation => Some(generation),
            Some(_) => {
                if recovered_poison {
                    ctx.runtime.clear_poison();
                }
                drop(runtime);
                drop(authority);
                return superseded_response(ctx);
            }
            None => match runtime.lifecycle.begin_operation() {
                Ok(generation) => Some(generation),
                Err(_) if recovered_poison => None,
                Err(_) => {
                    drop(runtime);
                    drop(authority);
                    return superseded_response(ctx);
                }
            },
        };
        let active = if recovered_poison {
            take_active_capture(&mut runtime)
        } else {
            (None, None)
        };
        let publication = if let Some(generation) = generation {
            runtime.state = "faulted".to_string();
            runtime.last_error = Some(error.message.clone());
            runtime.config_load_error = Some(error.message.clone());
            let scheduled = match error.class {
                ErrorClass::Transient => {
                    runtime.lifecycle.mark_transient(
                        error.capture_state,
                        error.message,
                        ctx.clock.now(),
                    );
                    runtime.lifecycle.next_retry_at.map(|due| (generation, due))
                }
                ErrorClass::Permanent => {
                    runtime.lifecycle.mark_permanent(error.message);
                    None
                }
                ErrorClass::Fatal => {
                    unreachable!("fatal command error is returned before publication")
                }
            };
            (scheduled, generation, None)
        } else {
            let message = "operation generation overflow while recovering poisoned runtime";
            (
                None,
                runtime.lifecycle.generation,
                Some(message.to_string()),
            )
        };
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        (publication.0, publication.1, active, publication.2)
    };
    if fatal_message.is_none() {
        if let Some((generation, due)) = scheduled {
            ctx.scheduler.schedule_retry(generation, due);
        } else {
            ctx.scheduler.invalidate(published_generation);
        }
    }
    drop(authority);
    drop_capture_then_session(active.0, active.1);
    if let Some(message) = fatal_message {
        signal_fatal(ctx, message.clone());
        fatal_failure_response(ctx, message)
    } else {
        current_failure_response(ctx)
    }
}

fn current_failure_response(ctx: &IdleDaemonContext) -> ControlResponse {
    let (message, class, daemon_state, capture_state) = ctx
        .runtime
        .lock()
        .map(|runtime| {
            (
                runtime
                    .lifecycle
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "capture command failed".to_string()),
                runtime.lifecycle.error_class,
                runtime.lifecycle.daemon_state,
                runtime.lifecycle.capture_state,
            )
        })
        .unwrap_or_else(|_| {
            (
                "runtime state lock poisoned".to_string(),
                Some(ErrorClass::Permanent),
                DaemonState::Degraded,
                CaptureState::Faulted,
            )
        });
    ControlResponse::failure(
        message,
        Some(idle_status_response(ctx)),
        class,
        daemon_state,
        capture_state,
    )
}

fn take_active_capture(
    runtime: &mut AppRuntimeState,
) -> (Option<CaptureBackend>, Option<Arc<CaptureSession>>) {
    let backend = runtime.capture.take();
    let session = runtime.session.take();
    runtime.capture_health = None;
    runtime.resolved_capture = None;
    #[cfg(test)]
    {
        runtime.test_capture_attached = false;
    }
    (backend, session)
}

fn empty_runtime(family: ConfigFamily) -> AppRuntimeState {
    AppRuntimeState {
        config: app_config::AppConfig::default(),
        config_family: family,
        prepared_legacy: None,
        resolved_capture: None,
        lifecycle: LifecycleState::ready_stopped(None),
        state: "unconfigured".to_string(),
        last_error: None,
        config_load_error: None,
        active_profile: None,
        capture: None,
        capture_health: None,
        session: None,
        #[cfg(test)]
        test_capture_attached: false,
    }
}

fn publication_generation(
    lifecycle: &mut LifecycleState,
    operation_generation: Option<u64>,
) -> Result<u64> {
    match operation_generation {
        Some(generation) if lifecycle.generation == generation => Ok(generation),
        Some(_) => Err(LambError::Control(
            "capture operation superseded".to_string(),
        )),
        None => lifecycle.begin_operation(),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "arguments mirror the atomic prepared-install transaction and its policy flags"
)]
fn install_prepared_then_follow_start_mode(
    ctx: &IdleDaemonContext,
    token: u64,
    prepared: PreparedBootstrap,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
    old_already_detached: bool,
    force_start: bool,
    activate: bool,
) -> ControlResponse {
    install_prepared_then_follow_start_mode_with_generation(
        ctx,
        token,
        None,
        prepared,
        legacy_starter,
        legacy_recovery,
        old_already_detached,
        force_start,
        activate,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "generation-aware installation must carry the same atomic transaction inputs"
)]
fn install_prepared_then_follow_start_mode_with_generation(
    ctx: &IdleDaemonContext,
    token: u64,
    operation_generation: Option<u64>,
    prepared: PreparedBootstrap,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
    old_already_detached: bool,
    force_start: bool,
    activate: bool,
) -> ControlResponse {
    if !operation_is_current(ctx, token) {
        return superseded_response(ctx);
    }
    match prepared {
        PreparedBootstrap::Legacy(prepared) => {
            if !operation_is_current(ctx, token) {
                return superseded_response(ctx);
            }
            let fault_sink = match runtime_fault_sink_for_attempt(ctx, operation_generation) {
                Ok(sink) => sink,
                Err(error) => {
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        error,
                        !old_already_detached,
                    );
                }
            };
            let mut active = match legacy_starter.start(&prepared, fault_sink) {
                Ok(active) => active,
                Err(error) => {
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        error,
                        !old_already_detached,
                    );
                }
            };
            if !operation_is_current(ctx, token) {
                drop(active);
                return superseded_response(ctx);
            }
            if let Some(session) = active.session.as_ref() {
                let recovery = legacy_recovery.failed_count(session).and_then(|failed| {
                    if failed == 0 {
                        Ok(())
                    } else {
                        Err(LambError::Control(format!(
                            "legacy startup recovery failed for {failed} transaction(s)"
                        )))
                    }
                });
                if let Err(error) = recovery {
                    drop(active);
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        classify_legacy_attempt_error(error),
                        !old_already_detached,
                    );
                }
            }
            if !operation_is_current(ctx, token) {
                drop(active);
                return superseded_response(ctx);
            }
            let authority = lock_operation_authority(ctx);
            if !operation_is_current(ctx, token) {
                drop(authority);
                drop(active);
                return superseded_response(ctx);
            }
            pause_before_final_mutation(ctx, FinalMutationKind::LegacySuccess);
            let published_health = active.health.take();
            let old = {
                let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
                let Ok(generation) =
                    publication_generation(&mut runtime.lifecycle, operation_generation)
                else {
                    if recovered_poison {
                        ctx.runtime.clear_poison();
                    }
                    drop(runtime);
                    drop(authority);
                    drop(active);
                    return superseded_response(ctx);
                };
                let old = if old_already_detached && !recovered_poison {
                    (None, None)
                } else {
                    take_active_capture(&mut runtime)
                };
                let resolved = active.resolved.clone();
                let session = active
                    .session
                    .take()
                    .expect("successful capture owns its session");
                let backend = active
                    .backend
                    .take()
                    .expect("successful capture owns its backend");
                runtime.capture_health = published_health.clone();
                runtime.config_family = ConfigFamily::Legacy;
                runtime.prepared_legacy = Some(prepared);
                runtime.active_profile = None;
                runtime.capture = Some(backend);
                runtime.session = Some(session);
                runtime.resolved_capture = Some(resolved.clone());
                runtime.state = "capturing".to_string();
                runtime.last_error = None;
                runtime.config_load_error = None;
                runtime
                    .lifecycle
                    .mark_running(None, resolved.resolved_target);
                if recovered_poison {
                    ctx.runtime.clear_poison();
                }
                (old, generation)
            };
            ctx.scheduler.invalidate(old.1);
            if let Some(health) = published_health {
                health.arm();
            }
            let response =
                ControlResponse::success("config reloaded", Some(idle_status_response(ctx)));
            drop(authority);
            drop(active);
            drop_capture_then_session((old.0).0, (old.0).1);
            response
        }
        PreparedBootstrap::App(mut loaded) => {
            let configured_start_mode = loaded.config.daemon.start_mode.clone();
            if force_start {
                loaded.config.daemon.start_mode = "auto".to_string();
            }
            if !operation_is_current(ctx, token) {
                return superseded_response(ctx);
            }
            let mut candidate = empty_runtime(ConfigFamily::App);
            let fault_sink = match runtime_fault_sink_for_attempt(ctx, operation_generation) {
                Ok(sink) => sink,
                Err(error) => {
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        error,
                        !old_already_detached,
                    );
                }
            };
            if let Err(error) = apply_loaded_app_config_inner_with_checkpoint(
                &mut candidate,
                loaded,
                &ctx.config_path,
                &ctx.calibration_root,
                legacy_starter,
                legacy_recovery,
                ctx.clock.now(),
                false,
                fault_sink,
                || {
                    if operation_is_current(ctx, token) {
                        Ok(())
                    } else {
                        Err(LambError::Control(
                            "capture operation superseded".to_string(),
                        ))
                    }
                },
            ) {
                if !operation_is_current(ctx, token) {
                    return superseded_response(ctx);
                }
                return command_attempt_failure(
                    ctx,
                    token,
                    classify_legacy_attempt_error(error),
                    !old_already_detached,
                );
            }
            candidate.config.daemon.start_mode = configured_start_mode;
            if !operation_is_current(ctx, token) {
                drop(candidate);
                return superseded_response(ctx);
            }
            if !matches!(
                candidate.lifecycle.capture_state,
                CaptureState::Running | CaptureState::Stopped
            ) {
                let error = CaptureAttemptError {
                    class: candidate
                        .lifecycle
                        .error_class
                        .unwrap_or(ErrorClass::Transient),
                    capture_state: candidate.lifecycle.capture_state,
                    message: candidate
                        .lifecycle
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "capture start failed".to_string()),
                };
                drop(candidate);
                return command_attempt_failure_with_generation(
                    ctx,
                    token,
                    operation_generation,
                    error,
                    !old_already_detached,
                );
            }
            if activate {
                let save_result = {
                    let authority = lock_operation_authority(ctx);
                    if !operation_is_current(ctx, token) {
                        drop(authority);
                        None
                    } else {
                        pause_before_final_mutation(ctx, FinalMutationKind::PersistenceAdmission);
                        let result = profile::save_config(&ctx.config_path, &candidate.config);
                        drop(authority);
                        Some(result)
                    }
                };
                let Some(save_result) = save_result else {
                    drop(candidate);
                    return superseded_response(ctx);
                };
                if let Err(error) = save_result {
                    drop(candidate);
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        classify_legacy_attempt_error(error),
                        !old_already_detached,
                    );
                }
                #[cfg(test)]
                if let Some(hook) = AFTER_START_CONFIG_PERSIST_HOOK.lock().unwrap().take() {
                    hook();
                }
            }
            let authority = lock_operation_authority(ctx);
            if !operation_is_current(ctx, token) {
                drop(authority);
                drop(candidate);
                return superseded_response(ctx);
            }
            pause_before_final_mutation(ctx, FinalMutationKind::AppSuccess);
            let published_health = candidate.capture_health.clone();
            let old = {
                let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
                let Ok(generation) =
                    publication_generation(&mut runtime.lifecycle, operation_generation)
                else {
                    if recovered_poison {
                        ctx.runtime.clear_poison();
                    }
                    drop(runtime);
                    drop(authority);
                    drop(candidate);
                    return superseded_response(ctx);
                };
                let old = if old_already_detached && !recovered_poison {
                    (None, None)
                } else {
                    take_active_capture(&mut runtime)
                };
                candidate.lifecycle.generation = generation;
                *runtime = candidate;
                if recovered_poison {
                    ctx.runtime.clear_poison();
                }
                (old, generation)
            };
            ctx.scheduler.invalidate(old.1);
            if let Some(health) = published_health {
                health.arm();
            }
            let response =
                ControlResponse::success("config reloaded", Some(idle_status_response(ctx)));
            drop(authority);
            drop_capture_then_session((old.0).0, (old.0).1);
            response
        }
        #[cfg(test)]
        PreparedBootstrap::TestAppCandidate {
            mut candidate,
            entered,
            release,
        } => {
            if let Some(entered) = entered {
                entered.wait();
            }
            if let Some(release) = release {
                release.wait();
            }
            if !operation_is_current(ctx, token) {
                drop(candidate);
                return superseded_response(ctx);
            }
            if activate {
                let save_result = {
                    let authority = lock_operation_authority(ctx);
                    if !operation_is_current(ctx, token) {
                        drop(authority);
                        None
                    } else {
                        pause_before_final_mutation(ctx, FinalMutationKind::PersistenceAdmission);
                        let result = profile::save_config(&ctx.config_path, &candidate.config);
                        drop(authority);
                        Some(result)
                    }
                };
                let Some(save_result) = save_result else {
                    drop(candidate);
                    return superseded_response(ctx);
                };
                if let Err(error) = save_result {
                    drop(candidate);
                    return command_attempt_failure_with_generation(
                        ctx,
                        token,
                        operation_generation,
                        classify_legacy_attempt_error(error),
                        !old_already_detached,
                    );
                }
                if let Some(hook) = AFTER_START_CONFIG_PERSIST_HOOK.lock().unwrap().take() {
                    hook();
                }
            }
            let authority = lock_operation_authority(ctx);
            if !operation_is_current(ctx, token) {
                drop(authority);
                drop(candidate);
                return superseded_response(ctx);
            }
            pause_before_final_mutation(ctx, FinalMutationKind::AppSuccess);
            let old = {
                let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
                let Ok(generation) =
                    publication_generation(&mut runtime.lifecycle, operation_generation)
                else {
                    if recovered_poison {
                        ctx.runtime.clear_poison();
                    }
                    drop(runtime);
                    drop(authority);
                    drop(candidate);
                    return superseded_response(ctx);
                };
                let old = if old_already_detached && !recovered_poison {
                    (None, None)
                } else {
                    take_active_capture(&mut runtime)
                };
                candidate.lifecycle.generation = generation;
                *runtime = candidate;
                if recovered_poison {
                    ctx.runtime.clear_poison();
                }
                (old, generation)
            };
            let response =
                ControlResponse::success("config reloaded", Some(idle_status_response(ctx)));
            ctx.scheduler.invalidate(old.1);
            drop(authority);
            drop_capture_then_session((old.0).0, (old.0).1);
            response
        }
        PreparedBootstrap::Faulted { message, .. } => command_attempt_failure_with_generation(
            ctx,
            token,
            operation_generation,
            CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message,
            },
            !old_already_detached,
        ),
    }
}

fn reload_daemon_config_with_recovery(
    ctx: &IdleDaemonContext,
    requested_profile: Option<&str>,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
) -> ControlResponse {
    reload_daemon_config_with_recovery_and_entry_hook(
        ctx,
        requested_profile,
        legacy_starter,
        legacy_recovery,
        || {},
    )
}

fn reload_daemon_config_with_recovery_and_entry_hook(
    ctx: &IdleDaemonContext,
    requested_profile: Option<&str>,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
    after_entry: impl FnOnce(),
) -> ControlResponse {
    let operation = match begin_new_operation_generation(ctx, OperationEntry::Command) {
        Ok(operation) => operation,
        Err(error) => return command_failure_without_state_mutation(ctx, error),
    };
    let token = operation.token;
    after_entry();
    let candidate = match prepare_config_from_disk(&ctx.config_path, requested_profile) {
        Ok(candidate) => candidate,
        Err(error) => {
            return command_attempt_failure_with_generation(
                ctx,
                token,
                Some(operation.generation),
                error,
                true,
            );
        }
    };
    if !operation_is_current(ctx, token) {
        return superseded_response(ctx);
    }
    if let Err(error) = validate_candidate_socket(ctx, &candidate) {
        return command_attempt_failure_with_generation(
            ctx,
            token,
            Some(operation.generation),
            error,
            true,
        );
    }
    install_prepared_then_follow_start_mode_with_generation(
        ctx,
        token,
        Some(operation.generation),
        candidate.prepared,
        legacy_starter,
        legacy_recovery,
        false,
        false,
        false,
    )
}

fn start_capture_transaction_with_recovery(
    ctx: &IdleDaemonContext,
    requested_profile: Option<&str>,
    activate: bool,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
) -> ControlResponse {
    let operation = match begin_new_operation_generation(ctx, OperationEntry::Command) {
        Ok(operation) => operation,
        Err(error) => return command_failure_without_state_mutation(ctx, error),
    };
    let token = operation.token;
    let authority = lock_operation_authority(ctx);
    if !operation_is_current(ctx, token) {
        drop(authority);
        return superseded_response(ctx);
    }
    let running = current_capture_is_running(ctx);
    if running {
        if !operation_is_current(ctx, token) {
            drop(authority);
            return superseded_response(ctx);
        }
        pause_before_final_mutation(ctx, FinalMutationKind::RunningStartNoop);
        let response =
            ControlResponse::success("capture already running", Some(idle_status_response(ctx)));
        drop(authority);
        return response;
    }
    drop(authority);
    let candidate = match prepare_config_from_disk(&ctx.config_path, requested_profile) {
        Ok(candidate) => candidate,
        Err(error) => {
            return command_attempt_failure_with_generation(
                ctx,
                token,
                Some(operation.generation),
                error,
                false,
            );
        }
    };
    if !operation_is_current(ctx, token) {
        return superseded_response(ctx);
    }
    if let Err(error) = validate_candidate_socket(ctx, &candidate) {
        return command_attempt_failure_with_generation(
            ctx,
            token,
            Some(operation.generation),
            error,
            false,
        );
    }
    let authority = lock_operation_authority(ctx);
    if !operation_is_current(ctx, token) {
        drop(authority);
        return superseded_response(ctx);
    }
    let old = {
        let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
        if runtime.lifecycle.generation != operation.generation {
            if recovered_poison {
                ctx.runtime.clear_poison();
            }
            drop(runtime);
            drop(authority);
            return superseded_response(ctx);
        }
        let old = take_active_capture(&mut runtime);
        let active_profile = requested_profile
            .map(str::to_string)
            .or_else(|| runtime.lifecycle.active_profile.clone());
        runtime.lifecycle.mark_starting(active_profile);
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        old
    };
    drop(authority);
    drop_capture_then_session(old.0, old.1);
    if !operation_is_current(ctx, token) {
        return superseded_response(ctx);
    }
    let mut response = install_prepared_then_follow_start_mode_with_generation(
        ctx,
        token,
        Some(operation.generation),
        candidate.prepared,
        legacy_starter,
        legacy_recovery,
        true,
        true,
        activate,
    );
    if response.ok {
        response.message = requested_profile
            .map(|profile| format!("capturing {profile}"))
            .unwrap_or_else(|| "capture started".to_string());
    }
    response
}

fn stop_capture_transaction(ctx: &IdleDaemonContext) -> ControlResponse {
    let operation = match begin_new_operation_generation(ctx, OperationEntry::Command) {
        Ok(operation) => operation,
        Err(error) => return command_failure_without_state_mutation(ctx, error),
    };
    let token = operation.token;
    let authority = lock_operation_authority(ctx);
    if !operation_is_current(ctx, token) {
        drop(authority);
        return superseded_response(ctx);
    }
    pause_before_final_mutation(ctx, FinalMutationKind::StopCapture);
    cancel_active_calibration(ctx);
    let active = {
        let (mut runtime, recovered_poison) = match ctx.runtime.lock() {
            Ok(runtime) => (runtime, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        if runtime.lifecycle.generation != operation.generation {
            drop(runtime);
            drop(authority);
            return superseded_response(ctx);
        }
        let active = take_active_capture(&mut runtime);
        runtime.state = if runtime.config_family == ConfigFamily::Legacy {
            "stopped".to_string()
        } else if runtime.active_profile.is_some() {
            "idle".to_string()
        } else {
            "unconfigured".to_string()
        };
        runtime.last_error = None;
        runtime.config_load_error = None;
        let active_profile = runtime
            .active_profile
            .as_ref()
            .map(|profile| profile.name.clone());
        runtime.lifecycle.mark_stopped(active_profile);
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        active
    };
    let response = ControlResponse::success("capture stopped", Some(idle_status_response(ctx)));
    drop(authority);
    drop_capture_then_session(active.0, active.1);
    response
}

fn release_capture_for_shutdown(ctx: &IdleDaemonContext) {
    let active = {
        let mut runtime = ctx
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active = take_active_capture(&mut runtime);
        runtime.state = "stopping".to_string();
        runtime.last_error = None;
        runtime.config_load_error = None;
        runtime.lifecycle.mark_stopping();
        ctx.runtime.clear_poison();
        active
    };
    drop_capture_then_session(active.0, active.1);
}

fn attempt_configured_start_with_recovery(
    ctx: &IdleDaemonContext,
    legacy_starter: &dyn LegacyCaptureStarter,
    recovery: &dyn LegacyStartupRecovery,
) -> Result<()> {
    let prepared = ctx
        .runtime
        .lock()
        .ok()
        .and_then(|runtime| runtime.prepared_legacy.clone());
    let Some(prepared) = prepared else {
        return Ok(());
    };
    let fault_sink = match runtime_fault_sink_for_attempt(ctx, None) {
        Ok(sink) => sink,
        Err(error) => {
            if error.class == ErrorClass::Fatal {
                return Err(LambError::Capture(error.message));
            }
            publish_capture_attempt_error(ctx, error);
            return Ok(());
        }
    };
    match legacy_starter.start(&prepared, fault_sink) {
        Ok(mut active) => {
            let recovery_result = active
                .session
                .as_ref()
                .ok_or(LambError::ControlInvariant(
                    "successful capture is missing its session",
                ))
                .and_then(|session| recovery.failed_count(session))
                .and_then(|failed| {
                    if failed == 0 {
                        Ok(())
                    } else {
                        Err(LambError::Control(format!(
                            "legacy startup recovery failed for {failed} transaction(s)"
                        )))
                    }
                });
            if let Err(error) = recovery_result {
                drop(active);
                let error = classify_legacy_attempt_error(error);
                if error.class == ErrorClass::Fatal {
                    return Err(LambError::Capture(error.message));
                }
                publish_capture_attempt_error(ctx, error);
                return Ok(());
            }
            let resolved = active.resolved.clone();
            let session = active
                .session
                .take()
                .expect("successful capture owns its session");
            let backend = active
                .backend
                .take()
                .expect("successful capture owns its backend");
            let capture_health = active.health.take();
            let published_health = capture_health.clone();
            let authority = lock_operation_authority(ctx);
            let mut runtime = ctx.runtime.lock().unwrap();
            let generation = match runtime.lifecycle.begin_operation() {
                Ok(generation) => generation,
                Err(_) => {
                    drop(runtime);
                    drop(authority);
                    drop_capture_then_session(Some(backend), Some(session));
                    return Err(LambError::ControlInvariant(
                        "initial capture publication generation overflow",
                    ));
                }
            };
            runtime.capture = Some(backend);
            runtime.capture_health = capture_health;
            runtime.session = Some(session);
            runtime.resolved_capture = Some(resolved.clone());
            runtime.state = "capturing".to_string();
            runtime.last_error = None;
            runtime.config_load_error = None;
            runtime
                .lifecycle
                .mark_running(None, resolved.resolved_target);
            drop(runtime);
            ctx.scheduler.invalidate(generation);
            if let Some(health) = published_health {
                health.arm();
            }
            drop(authority);
            Ok(())
        }
        Err(error) => {
            if error.class == ErrorClass::Fatal {
                Err(LambError::Capture(error.message))
            } else {
                publish_capture_attempt_error(ctx, error);
                Ok(())
            }
        }
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

impl AppRuntimeState {
    fn capture_matches_target(&self, target: &SessionCalibrationTarget) -> bool {
        match self.capture.as_ref() {
            Some(CaptureBackend::Fake(_, _)) => false,
            Some(CaptureBackend::Jack(capture, _)) => {
                target.active_profile.backend == "jack" && capture.sample_rate == target.sample_rate
            }
            Some(CaptureBackend::PipeWire(capture, _)) => {
                target.active_profile.backend == "pipewire"
                    && capture.sample_rate == target.sample_rate
            }
            #[cfg(test)]
            Some(CaptureBackend::TestProbe(_)) => false,
            #[cfg(test)]
            Some(CaptureBackend::TestAppProbe { .. }) => true,
            None => {
                #[cfg(test)]
                {
                    self.test_capture_attached
                }
                #[cfg(not(test))]
                {
                    false
                }
            }
        }
    }
}

#[allow(dead_code)]
enum CaptureBackend {
    Fake(FakeCapture, Vec<String>),
    Jack(JackCapture, Vec<String>),
    PipeWire(PipeWireCapture, Vec<String>),
    #[cfg(test)]
    TestProbe(Box<dyn Send + Sync>),
    #[cfg(test)]
    TestAppProbe {
        resource: Box<dyn Send + Sync>,
        sample_rate: u32,
    },
}

struct PrivateAppCaptureAttempt {
    backend: Option<CaptureBackend>,
    runtime: Option<CaptureRuntime>,
    session: Option<Arc<CaptureSession>>,
}

impl PrivateAppCaptureAttempt {
    fn new(backend: CaptureBackend, runtime: CaptureRuntime) -> Self {
        Self {
            backend: Some(backend),
            runtime: Some(runtime),
            session: None,
        }
    }

    fn backend(&self) -> &CaptureBackend {
        self.backend.as_ref().expect("private attempt has backend")
    }

    fn take_runtime(&mut self) -> CaptureRuntime {
        self.runtime
            .take()
            .expect("private attempt has capture runtime")
    }

    fn set_session(&mut self, session: Arc<CaptureSession>) {
        self.session = Some(session);
    }

    fn session(&self) -> &Arc<CaptureSession> {
        self.session.as_ref().expect("private attempt has session")
    }

    fn run_post_backend_step<F>(&mut self, step: F) -> Result<()>
    where
        F: FnOnce(&Arc<CaptureSession>) -> Result<()>,
    {
        step(self.session())
    }

    fn publish(mut self) -> (CaptureBackend, Arc<CaptureSession>) {
        let backend = self.backend.take().expect("private attempt has backend");
        let session = self.session.take().expect("private attempt has session");
        debug_assert!(self.runtime.is_none());
        (backend, session)
    }

    #[cfg(test)]
    fn for_test(backend: CaptureBackend, session: Arc<CaptureSession>) -> Self {
        Self {
            backend: Some(backend),
            runtime: None,
            session: Some(session),
        }
    }
}

impl Drop for PrivateAppCaptureAttempt {
    fn drop(&mut self) {
        drop(self.backend.take());
        drop(self.session.take());
        drop(self.runtime.take());
    }
}

impl CaptureBackend {
    fn runtime_error(&self) -> Option<String> {
        match self {
            Self::Fake(_, _) => None,
            Self::Jack(_, _) => None,
            Self::PipeWire(capture, _) => capture.runtime_error(),
            #[cfg(test)]
            Self::TestProbe(_) => None,
            #[cfg(test)]
            Self::TestAppProbe { .. } => None,
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            CaptureBackend::Fake(_, _) => {
                unreachable!("fake capture is not used by app-profile runtime")
            }
            CaptureBackend::Jack(c, _) => c.sample_rate,
            CaptureBackend::PipeWire(c, _) => c.sample_rate,
            #[cfg(test)]
            CaptureBackend::TestProbe(_) => 0,
            #[cfg(test)]
            CaptureBackend::TestAppProbe { sample_rate, .. } => *sample_rate,
        }
    }
}

fn run_idle_listener(ctx: Arc<IdleDaemonContext>, socket: ControlSocketOwner) -> Result<()> {
    run_idle_listener_with_hook(ctx, socket, |_| {})
}

fn cancel_operation_job(ctx: &IdleDaemonContext, job: OperationJob) {
    if let OperationJob::Client { stream, .. } = job {
        let response = ControlResponse {
            ok: false,
            message: "shutting down".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
        let _ = write_response(stream, &response);
    }
}

fn finish_idle_listener_with_probe<F>(
    ctx: Arc<IdleDaemonContext>,
    mut socket: ControlSocketOwner,
    lane: Arc<OperationLane<OperationJob>>,
    scheduler_worker: std::thread::JoinHandle<()>,
    worker: std::thread::JoinHandle<()>,
    mut probe: F,
) -> Result<()>
where
    F: FnMut(&'static str),
{
    ctx.scheduler.stop();
    let mut first_error = scheduler_worker
        .join()
        .err()
        .map(|_| LambError::Control("retry scheduler panicked".to_string()));
    probe("scheduler-joined");

    lane.close();
    probe("lane-closed");
    if worker.join().is_err() {
        while let Some(job) = lane.pop() {
            cancel_operation_job(&ctx, job);
        }
        if first_error.is_none() {
            first_error = Some(LambError::Control("operation worker panicked".to_string()));
        }
    }
    probe("worker-joined");

    release_capture_for_shutdown(&ctx);
    probe("capture-released");
    let cleanup_result = socket.cleanup();
    probe("socket-cleaned");
    if let Some(error) = first_error {
        return Err(error);
    }
    cleanup_result
}

fn finish_idle_listener(
    ctx: Arc<IdleDaemonContext>,
    socket: ControlSocketOwner,
    lane: Arc<OperationLane<OperationJob>>,
    scheduler_worker: std::thread::JoinHandle<()>,
    worker: std::thread::JoinHandle<()>,
) -> Result<()> {
    finish_idle_listener_with_probe(ctx, socket, lane, scheduler_worker, worker, |_| {})
}

fn run_idle_listener_with_hook<F>(
    ctx: Arc<IdleDaemonContext>,
    socket: ControlSocketOwner,
    before_operation: F,
) -> Result<()>
where
    F: Fn(&ControlRequest) + Send + 'static,
{
    run_idle_listener_with_dependencies(
        ctx,
        socket,
        Arc::new(RealLegacyCaptureStarter),
        Arc::new(RealLegacyStartupRecovery),
        || {},
        move |job| {
            if let OperationJob::Client { request, .. } = job {
                before_operation(request);
            }
        },
        || {},
    )
}

fn run_idle_listener_with_dependencies<C, B, A>(
    ctx: Arc<IdleDaemonContext>,
    socket: ControlSocketOwner,
    legacy_starter: Arc<dyn LegacyCaptureStarter>,
    legacy_recovery: Arc<dyn LegacyStartupRecovery>,
    after_accept: C,
    before_operation: B,
    after_operation: A,
) -> Result<()>
where
    C: Fn(),
    B: Fn(&OperationJob) + Send + 'static,
    A: Fn() + Send + 'static,
{
    debug_assert_eq!(ctx.control_socket_path, socket.path);
    let lane = Arc::new(OperationLane::new(DEFAULT_CONTROL_QUEUE_CAPACITY as usize)?);
    let scheduler_worker = spawn_retry_scheduler(ctx.scheduler.clone(), Arc::clone(&ctx.clock), {
        let lane = Arc::clone(&lane);
        move |operation| {
            lane.try_enqueue(OperationJob::Internal(operation))
                .map_err(|(error, _)| error)
        }
    })
    .map_err(|error| io_error("retry scheduler", error))?;
    let worker = spawn_operation_worker(
        Arc::clone(&lane),
        DEFAULT_WORKER_STACK_BYTES as usize,
        {
            let ctx = Arc::clone(&ctx);
            let legacy_starter = Arc::clone(&legacy_starter);
            let legacy_recovery = Arc::clone(&legacy_recovery);
            move |job| {
                ctx.scheduler.notify_lane_available();
                before_operation(&job);
                execute_operation_job_with_recovery(
                    &ctx,
                    job,
                    legacy_starter.as_ref(),
                    legacy_recovery.as_ref(),
                );
                after_operation();
            }
        },
        {
            let ctx = Arc::clone(&ctx);
            move |job| cancel_operation_job(&ctx, job)
        },
    );
    for stream in socket.listener().incoming() {
        match stream {
            Ok(stream) => {
                after_accept();
                let _ = route_idle_stream(&ctx, &lane, stream);
            }
            Err(err) => {
                eprintln!("lamb: connection error: {err}");
            }
        }
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
    }
    let fatal = ctx.first_fatal.get().cloned();
    let result = finish_idle_listener(ctx, socket, lane, scheduler_worker, worker);
    match (fatal, result) {
        (Some(message), _) => Err(LambError::Control(message)),
        (None, result) => result,
    }
}

fn expand_control_socket_path(socket_path: &str) -> Result<PathBuf> {
    if socket_path.contains("%t") {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(|_| LambError::Validation("XDG_RUNTIME_DIR does not exist".to_string()))?;
        return Ok(PathBuf::from(socket_path.replace("%t", &runtime_dir)));
    }
    Ok(PathBuf::from(socket_path))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.file_type().is_socket() && metadata.dev() == self.dev && metadata.ino() == self.ino
    }
}

#[derive(Debug)]
struct ControlSocketOwner {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    cleaned: bool,
}

#[derive(Debug)]
struct StagingSocketGuard {
    listener: Option<UnixListener>,
    path: PathBuf,
    identity: Option<SocketIdentity>,
    armed: bool,
}

impl StagingSocketGuard {
    fn new(listener: UnixListener, path: PathBuf) -> Self {
        Self {
            listener: Some(listener),
            path,
            identity: None,
            armed: true,
        }
    }

    fn set_identity(&mut self, identity: SocketIdentity) {
        self.identity = Some(identity);
    }

    fn into_owner(mut self) -> ControlSocketOwner {
        let listener = self
            .listener
            .take()
            .expect("armed staging guard owns its listener");
        let identity = self
            .identity
            .expect("staging identity is acquired before ownership transfer");
        self.armed = false;
        ControlSocketOwner {
            listener,
            path: self.path.clone(),
            identity,
            cleaned: false,
        }
    }
}

impl Drop for StagingSocketGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.identity {
            Some(identity) => {
                let _ = cleanup_socket_path(&self.path, identity);
            }
            None => {
                if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                    if metadata.file_type().is_socket() {
                        let _ = fs::remove_file(&self.path);
                    }
                }
            }
        }
    }
}

impl ControlSocketOwner {
    fn bind(path: PathBuf) -> Result<Self> {
        Self::bind_with_setup(path, |_| Ok(()))
    }

    fn bind_with_setup<F>(path: PathBuf, setup: F) -> Result<Self>
    where
        F: FnOnce(&UnixListener) -> Result<()>,
    {
        Self::bind_with_hooks(path, || {}, setup)
    }

    fn bind_with_hooks<H, F>(path: PathBuf, after_pin: H, setup: F) -> Result<Self>
    where
        H: FnOnce(),
        F: FnOnce(&UnixListener) -> Result<()>,
    {
        Self::bind_with_io(
            path,
            after_pin,
            pin_socket_path,
            |pinned| {
                pinned
                    .metadata()
                    .map_err(|source| io_error("pinned socket", source))
            },
            setup,
        )
    }

    fn bind_with_io<H, P, M, F>(
        path: PathBuf,
        after_pin: H,
        pin: P,
        pinned_metadata: M,
        setup: F,
    ) -> Result<Self>
    where
        H: FnOnce(),
        P: FnOnce(&Path) -> Result<fs::File>,
        M: FnOnce(&fs::File) -> Result<fs::Metadata>,
        F: FnOnce(&UnixListener) -> Result<()>,
    {
        validate_unix_socket_path(&path)?;
        let staging_path = private_socket_path(&path)?;
        validate_unix_socket_path(&staging_path)?;
        remove_stale_control_socket(&path)?;
        let parent = path
            .parent()
            .ok_or_else(|| LambError::Control("control socket path has no parent".to_string()))?;
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;

        let listener =
            UnixListener::bind(&staging_path).map_err(|source| io_error(&staging_path, source))?;
        let mut staging = StagingSocketGuard::new(listener, staging_path.clone());
        let path_metadata = fs::symlink_metadata(&staging_path)
            .map_err(|source| io_error(&staging_path, source))?;
        if !path_metadata.file_type().is_socket() {
            return Err(LambError::Control(format!(
                "private control path is not a socket at {}",
                staging_path.display()
            )));
        }
        let identity = SocketIdentity::from_metadata(&path_metadata);
        staging.set_identity(identity);

        let pinned = pin(&staging_path)?;
        let metadata = pinned_metadata(&pinned)?;
        if !identity.matches(&metadata) {
            return Err(LambError::Control(format!(
                "private control socket changed before pinning at {}",
                staging_path.display()
            )));
        }
        let mut owner = staging.into_owner();
        after_pin();

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let pinned_path = PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd()));
        fs::set_permissions(&pinned_path, permissions)
            .map_err(|source| io_error(&staging_path, source))?;

        match rename_noreplace(&staging_path, &path) {
            Ok(()) => {}
            Err(source) if source.raw_os_error() == Some(libc::EEXIST) => {
                return Err(LambError::Control(format!(
                    "control path changed during socket publication at {}",
                    path.display()
                )));
            }
            Err(source) => return Err(io_error(&path, source)),
        }
        owner.path = path;

        setup(owner.listener())?;
        Ok(owner)
    }

    fn listener(&self) -> &UnixListener {
        &self.listener
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        cleanup_socket_path(&self.path, self.identity)?;
        self.cleaned = true;
        Ok(())
    }
}

const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;

fn validate_unix_socket_path(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > UNIX_SOCKET_PATH_MAX_BYTES {
        return Err(LambError::Control(format!(
            "control socket path exceeds Linux sockaddr_un.sun_path capacity ({} > {} bytes): {}",
            bytes.len(),
            UNIX_SOCKET_PATH_MAX_BYTES,
            path.display()
        )));
    }
    if bytes.contains(&0) {
        return Err(LambError::Control(
            "control socket path contains NUL".to_string(),
        ));
    }
    Ok(())
}

fn private_socket_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| LambError::Control("control socket path has no parent".to_string()))?;
    if path.file_name().is_none() {
        return Err(LambError::Control(
            "control socket path has no file name".to_string(),
        ));
    }
    let mut random = 0_u64;
    let read = unsafe {
        libc::getrandom(
            (&mut random as *mut u64).cast::<libc::c_void>(),
            std::mem::size_of::<u64>(),
            0,
        )
    };
    if read != std::mem::size_of::<u64>() as isize {
        return Err(io_error(parent, std::io::Error::last_os_error()));
    }
    Ok(parent.join(format!(".lamb-{:016x}", random)))
}

fn pin_socket_path(path: &Path) -> Result<fs::File> {
    let nul_terminated = nul_terminated_path(path)?;
    let fd = unsafe {
        libc::open(
            nul_terminated.as_ptr().cast::<libc::c_char>(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from = nul_terminated_path(from).map_err(lamb_error_to_io)?;
    let to = nul_terminated_path(to).map_err(lamb_error_to_io)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr().cast::<libc::c_char>(),
            libc::AT_FDCWD,
            to.as_ptr().cast::<libc::c_char>(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn nul_terminated_path(path: &Path) -> Result<Vec<u8>> {
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.contains(&0) {
        return Err(LambError::Control(
            "control socket path contains NUL".to_string(),
        ));
    }
    let mut nul_terminated = Vec::with_capacity(path_bytes.len() + 1);
    nul_terminated.extend_from_slice(path_bytes);
    nul_terminated.push(0);
    Ok(nul_terminated)
}

fn lamb_error_to_io(error: LambError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
}

fn cleanup_socket_path(path: &Path, identity: SocketIdentity) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if identity.matches(&metadata) => {
            fs::remove_file(path).map_err(|source| io_error(path, source))
        }
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

impl Drop for ControlSocketOwner {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn remove_stale_control_socket(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(path, source)),
        Ok(metadata) => metadata,
    };
    if !metadata.file_type().is_socket() {
        return Err(LambError::Control(format!(
            "refusing to replace non-socket control path {}",
            path.display()
        )));
    }

    let identity = SocketIdentity::from_metadata(&metadata);
    let probe = UnixDatagram::unbound().map_err(|source| io_error(path, source))?;
    match probe.connect(path) {
        Err(source) if source.raw_os_error() == Some(libc::EPROTOTYPE) => {
            Err(LambError::Control(format!(
                "control socket already has a live listener at {}",
                path.display()
            )))
        }
        Err(source) if source.raw_os_error() == Some(libc::ECONNREFUSED) => {
            match fs::symlink_metadata(path) {
                Ok(current) if identity.matches(&current) => {
                    fs::remove_file(path).map_err(|source| io_error(path, source))
                }
                Ok(_) => Err(LambError::Control(format!(
                    "control socket changed while checking stale path {}",
                    path.display()
                ))),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(io_error(path, source)),
            }
        }
        Ok(()) => Err(LambError::Control(format!(
            "refusing control socket with unexpected probe response at {}",
            path.display()
        ))),
        Err(source) => Err(io_error(path, source)),
    }
}

fn read_request(stream: UnixStream) -> Result<(ControlRequest, UnixStream)> {
    stream
        .set_read_timeout(Some(CONTROL_REQUEST_TIMEOUT))
        .map_err(|source| LambError::Control(source.to_string()))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| LambError::Control(source.to_string()))?;
    let request: ControlRequest = serde_json::from_str(&line)
        .map_err(|err| LambError::Control(format!("invalid control request: {err}")))?;
    Ok((request, reader.into_inner()))
}

fn write_response(stream: UnixStream, response: &ControlResponse) -> Result<()> {
    let body =
        serde_json::to_string(response).map_err(|err| LambError::Control(err.to_string()))?;
    let mut stream = stream;
    writeln!(stream, "{body}").map_err(|source| LambError::Control(source.to_string()))?;
    Ok(())
}

fn begin_shutdown(ctx: &IdleDaemonContext) {
    if begin_new_operation_generation(ctx, OperationEntry::DirectStop).is_err() {
        let _authority = lock_operation_authority(ctx);
        ctx.stop.store(true, Ordering::SeqCst);
        ctx.scheduler.stop();
        let mut runtime = ctx
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.lifecycle.mark_stopping();
        ctx.runtime.clear_poison();
    }
}

fn signal_fatal(ctx: &IdleDaemonContext, message: String) {
    let _ = ctx.first_fatal.set(message);
    ctx.stop.store(true, Ordering::SeqCst);
    ctx.scheduler.stop();
    let _ = nonblocking_control_wake(&ctx.control_socket_path);
}

fn nonblocking_control_wake(path: &Path) -> Result<()> {
    validate_unix_socket_path(path)?;
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let path_bytes = path.as_os_str().as_bytes();
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path_bytes.len(),
        );
    }
    let address_len = std::mem::offset_of!(libc::sockaddr_un, sun_path)
        .checked_add(path_bytes.len())
        .and_then(|length| length.checked_add(1))
        .and_then(|length| libc::socklen_t::try_from(length).ok())
        .ok_or_else(|| LambError::Control("control socket address length overflow".to_string()))?;
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    };
    if connected == 0 {
        return Ok(());
    }

    let source = std::io::Error::last_os_error();
    let raw = source.raw_os_error();
    // A successful/in-progress connect makes accept eligible. EAGAIN means the
    // backlog is already full, so accept is also eligible; once accepted, the
    // independently bounded request read makes the stop flag visible promptly.
    if raw == Some(libc::EINPROGRESS)
        || raw == Some(libc::EALREADY)
        || raw == Some(libc::EAGAIN)
        || raw == Some(libc::ECONNREFUSED)
    {
        Ok(())
    } else {
        Err(io_error(path, source))
    }
}

fn route_idle_stream(
    ctx: &IdleDaemonContext,
    lane: &OperationLane<OperationJob>,
    stream: UnixStream,
) -> Result<()> {
    let (request, stream) = read_request(stream)?;
    match request {
        ControlRequest::Status => {
            let response = ControlResponse {
                ok: true,
                message: "status".to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        ControlRequest::Stop => {
            begin_shutdown(ctx);
            cancel_active_calibration(ctx);
            let response = ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        request => match lane.try_enqueue(OperationJob::Client { request, stream }) {
            Ok(()) => Ok(()),
            Err((
                EnqueueError::Full | EnqueueError::Closed,
                OperationJob::Client { stream, .. },
            )) => {
                let response = ControlResponse {
                    ok: false,
                    message: "operation queue is busy or shutting down".to_string(),
                    status: Some(idle_status_response(ctx)),
                    error_context: crate::control::ControlErrorContext::default(),
                    persistence_outcome: None,
                    threshold_report: None,
                };
                write_response(stream, &response)
            }
            Err((_, OperationJob::Internal(_))) => {
                unreachable!("client admission returned internal job")
            }
        },
    }
}

#[cfg(test)]
fn handle_idle_request(ctx: &IdleDaemonContext, request: ControlRequest) -> ControlResponse {
    handle_idle_request_with_recovery(
        ctx,
        request,
        &RealLegacyCaptureStarter,
        &RealLegacyStartupRecovery,
    )
}

fn handle_idle_request_with_recovery(
    ctx: &IdleDaemonContext,
    request: ControlRequest,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
) -> ControlResponse {
    let legacy = ctx
        .runtime
        .lock()
        .map(|runtime| runtime.config_family == ConfigFamily::Legacy)
        .unwrap_or(false);
    if legacy && matches!(&request, ControlRequest::Threshold { .. }) {
        return ControlResponse {
            ok: false,
            message: "profile threshold commands are unsupported for legacy configuration"
                .to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
    }
    match request {
        ControlRequest::Status => ControlResponse {
            ok: true,
            message: "status".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        },
        ControlRequest::Stop => {
            begin_shutdown(ctx);
            cancel_active_calibration(ctx);
            ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
        ControlRequest::StartCapture { profile, activate } => {
            start_capture_transaction_with_recovery(
                ctx,
                profile.as_deref(),
                activate,
                legacy_starter,
                legacy_recovery,
            )
        }
        ControlRequest::StopCapture => stop_capture_transaction(ctx),
        ControlRequest::Recall => persistence_delivery_only_response(idle_status_response(ctx)),
        ControlRequest::Clear => handle_app_clear(ctx),
        ControlRequest::Dump => persistence_delivery_only_response(idle_status_response(ctx)),
        ControlRequest::Reload => {
            reload_daemon_config_with_recovery(ctx, None, legacy_starter, legacy_recovery)
        }
        ControlRequest::Threshold { request } => handle_app_threshold(ctx, request),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScheduledGenerationDecision {
    Current,
    Stale,
    Terminalized,
}

fn scheduled_generation_decision(
    ctx: &IdleDaemonContext,
    generation: u64,
) -> ScheduledGenerationDecision {
    let authority = lock_operation_authority(ctx);
    let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
    let generation_is_current = runtime.lifecycle.generation == generation;
    let scheduler_stopped = ctx.scheduler.is_stopped();
    if !recovered_poison {
        return if scheduler_stopped {
            ScheduledGenerationDecision::Terminalized
        } else if generation_is_current {
            ScheduledGenerationDecision::Current
        } else {
            ScheduledGenerationDecision::Stale
        };
    }
    if !generation_is_current {
        ctx.runtime.clear_poison();
        return ScheduledGenerationDecision::Stale;
    }

    let no_active_ownership = runtime.capture.is_none()
        && runtime.session.is_none()
        && runtime.capture_health.is_none()
        && runtime.resolved_capture.is_none();
    #[cfg(test)]
    let no_active_ownership = no_active_ownership && !runtime.test_capture_attached;
    let retry_is_coherent = runtime.lifecycle.error_class == Some(ErrorClass::Transient)
        && runtime.lifecycle.retry_policy == RetryPolicy::BoundedBackoff
        && runtime.lifecycle.next_retry_at.is_some();
    if !scheduler_stopped && no_active_ownership && retry_is_coherent {
        ctx.runtime.clear_poison();
        return ScheduledGenerationDecision::Current;
    }

    let active = take_active_capture(&mut runtime);
    let message = "runtime state lock poison violated scheduled retry invariants".to_string();
    ctx.runtime.clear_poison();
    drop(runtime);
    drop(authority);
    drop_capture_then_session(active.0, active.1);
    signal_fatal(ctx, message);
    ScheduledGenerationDecision::Terminalized
}

fn execute_operation_job_with_recovery(
    ctx: &IdleDaemonContext,
    job: OperationJob,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
) {
    if ctx.stop.load(Ordering::Acquire) {
        cancel_operation_job(ctx, job);
        return;
    }
    match job {
        OperationJob::Client { request, stream } => match request {
            ControlRequest::Recall => app_persistence_delivery(ctx, ExportCommand::Recall, stream),
            ControlRequest::Dump => app_persistence_delivery(ctx, ExportCommand::Dump, stream),
            request => {
                let response = handle_idle_request_with_recovery(
                    ctx,
                    request,
                    legacy_starter,
                    legacy_recovery,
                );
                let _ = write_response(stream, &response);
                if response.error_context.error_class == Some(ErrorClass::Fatal) {
                    signal_fatal(ctx, response.message);
                }
            }
        },
        OperationJob::Internal(ScheduledOperation::Retry { generation }) => {
            execute_retry_operation(ctx, generation, legacy_starter, legacy_recovery)
        }
        OperationJob::Internal(ScheduledOperation::RuntimeFault {
            generation,
            attempt_id,
            fault,
        }) => publish_runtime_fault(ctx, generation, attempt_id, fault),
    }
}

fn execute_retry_operation(
    ctx: &IdleDaemonContext,
    generation: u64,
    legacy_starter: &dyn LegacyCaptureStarter,
    legacy_recovery: &dyn LegacyStartupRecovery,
) {
    if scheduled_generation_decision(ctx, generation) != ScheduledGenerationDecision::Current {
        return;
    }
    let token = match begin_command_operation(ctx) {
        Ok(token) => token,
        Err(error) => {
            if error.class == ErrorClass::Fatal {
                signal_fatal(ctx, error.message);
            }
            return;
        }
    };
    if scheduled_generation_decision(ctx, generation) != ScheduledGenerationDecision::Current
        || !operation_is_current(ctx, token)
    {
        return;
    }
    let candidate = match prepare_config_from_disk(&ctx.config_path, None) {
        Ok(candidate) => candidate,
        Err(error) => {
            let response = command_attempt_failure(ctx, token, error, false);
            signal_fatal_response(ctx, &response);
            return;
        }
    };
    if scheduled_generation_decision(ctx, generation) != ScheduledGenerationDecision::Current
        || !operation_is_current(ctx, token)
    {
        return;
    }
    if let Err(error) = validate_candidate_socket(ctx, &candidate) {
        let response = command_attempt_failure(ctx, token, error, false);
        signal_fatal_response(ctx, &response);
        return;
    }
    if scheduled_generation_decision(ctx, generation) != ScheduledGenerationDecision::Current
        || !operation_is_current(ctx, token)
    {
        return;
    }
    let response = install_prepared_then_follow_start_mode(
        ctx,
        token,
        candidate.prepared,
        legacy_starter,
        legacy_recovery,
        false,
        true,
        false,
    );
    signal_fatal_response(ctx, &response);
}

fn signal_fatal_response(ctx: &IdleDaemonContext, response: &ControlResponse) {
    if response.error_context.error_class == Some(ErrorClass::Fatal) {
        signal_fatal(ctx, response.message.clone());
    }
}

fn publish_runtime_fault(
    ctx: &IdleDaemonContext,
    generation: u64,
    attempt_id: CaptureAttemptId,
    fault: RuntimeCaptureFault,
) {
    if scheduled_generation_decision(ctx, generation) != ScheduledGenerationDecision::Current {
        return;
    }
    let authority = lock_operation_authority(ctx);
    if ctx.stop.load(Ordering::SeqCst) {
        return;
    }
    let (mut runtime, recovered_poison) = lock_runtime_recovering_poison(ctx);
    if runtime.lifecycle.generation != generation
        || runtime
            .capture_health
            .as_ref()
            .and_then(PipeWireHealth::attempt_id)
            != Some(attempt_id)
    {
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        return;
    }
    let has_matching_capture = runtime.capture.is_some()
        || runtime.session.is_some()
        || runtime.capture_health.is_some()
        || runtime.resolved_capture.is_some();
    #[cfg(test)]
    let has_matching_capture = has_matching_capture || runtime.test_capture_attached;
    if !has_matching_capture {
        if recovered_poison {
            ctx.runtime.clear_poison();
        }
        return;
    }
    let (capture_state, message) = match fault {
        RuntimeCaptureFault::DeviceDisconnected(message) => {
            (CaptureState::WaitingForDevice, message)
        }
        RuntimeCaptureFault::BackendFault(message) => (CaptureState::Faulted, message),
    };
    let active = take_active_capture(&mut runtime);
    runtime.state = "faulted".to_string();
    runtime.last_error = Some(message.clone());
    runtime.config_load_error = Some(message.clone());
    runtime
        .lifecycle
        .mark_transient(capture_state, message, ctx.clock.now());
    let due = runtime.lifecycle.next_retry_at;
    if recovered_poison {
        ctx.runtime.clear_poison();
    }
    drop(runtime);
    drop_capture_then_session(active.0, active.1);
    if let Some(due) = due {
        ctx.scheduler.schedule_retry(generation, due);
    }
    drop(authority);
}

fn cancel_active_calibration(ctx: &IdleDaemonContext) {
    let session = ctx
        .runtime
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .session
        .clone();
    if let Some(session) = session {
        session.arena.cancel_calibration();
    }
}

fn idle_status_response(ctx: &IdleDaemonContext) -> DaemonStatus {
    let runtime = ctx.runtime.lock();
    let stopping = ctx.stop.load(Ordering::Acquire);
    let snapshot = runtime.ok().map(|runtime| {
        let capture_fault = runtime
            .capture_health
            .as_ref()
            .and_then(PipeWireHealth::fault)
            .or_else(|| {
                runtime
                    .capture
                    .as_ref()
                    .and_then(CaptureBackend::runtime_error)
            });
        let legacy_target = runtime
            .prepared_legacy
            .as_ref()
            .and_then(|prepared| prepared.static_config.target.clone());
        let legacy_format = runtime
            .prepared_legacy
            .as_ref()
            .map(|prepared| prepared.static_config.sample_format.clone());
        (
            stopping,
            runtime.config_family,
            runtime.state.clone(),
            runtime.last_error.clone(),
            runtime.config_load_error.clone(),
            capture_fault,
            runtime
                .resolved_capture
                .as_ref()
                .and_then(|resolved| resolved.resolved_target.clone()),
            runtime
                .active_profile
                .as_ref()
                .map(|profile| profile.name.clone()),
            legacy_target,
            legacy_format,
            runtime.session.clone(),
            runtime.lifecycle.clone(),
        )
    });
    let Some((
        stopping,
        config_family,
        runtime_state,
        runtime_last_error,
        config_load_error,
        capture_fault,
        resolved_capture_target,
        app_resolved_target,
        legacy_target,
        legacy_format,
        session,
        mut lifecycle_state,
    )) = snapshot
    else {
        let mut lifecycle = LifecycleState::ready_stopped(None);
        lifecycle.mark_permanent("runtime state lock poisoned".to_string());
        return DaemonStatus {
            state: "poisoned".to_string(),
            active_export_count: 0,
            pending_recall_count: 0,
            buffer_capacity_seconds: 0.0,
            retained_seconds: 0.0,
            dropped_frames: 0,
            target: Some(ctx.config_path.display().to_string()),
            resolved_target: None,
            sample_rate: 0,
            channel_count: 0,
            format: "".to_string(),
            last_error: None,
            lifecycle: lifecycle.status(ctx.clock.as_ref()),
        };
    };

    let state = if stopping {
        "stopping".to_string()
    } else if capture_fault.is_some() {
        "faulted".to_string()
    } else {
        runtime_state
    };
    let last_error = capture_fault
        .clone()
        .or(config_load_error)
        .or(runtime_last_error);
    if stopping {
        lifecycle_state.mark_stopping();
    } else if let Some(error) = capture_fault {
        lifecycle_state.daemon_state = DaemonState::Degraded;
        lifecycle_state.capture_state = CaptureState::Faulted;
        lifecycle_state.error_class = Some(ErrorClass::Transient);
        lifecycle_state.last_error = Some(error);
        lifecycle_state.retry_policy = RetryPolicy::Manual;
        lifecycle_state.retry_attempt = 0;
        lifecycle_state.next_retry_at = None;
    }

    let (sample_rate, channel_count, session_format, capacity, retained, dropped, frozen_pending) =
        if let Some(session) = session {
            let (capacity, retained, dropped, frozen_pending) = match session.status() {
                Ok(status) => (
                    status.capacity_frames,
                    status.retained_frames,
                    status.dropped_frames,
                    status.frozen_pending,
                ),
                Err(_) => (0, 0, 0, false),
            };
            (
                session.sample_rate,
                session.channel_count,
                "F32LE".to_string(),
                capacity as f64 / f64::from(session.sample_rate),
                retained as f64 / f64::from(session.sample_rate),
                dropped,
                frozen_pending,
            )
        } else {
            (0, 0, "".to_string(), 0.0, 0.0, 0, false)
        };
    let (target, resolved_target, format) = match config_family {
        ConfigFamily::Legacy => (
            legacy_target,
            resolved_capture_target,
            legacy_format.unwrap_or_default(),
        ),
        ConfigFamily::App => (
            Some(ctx.config_path.display().to_string()),
            app_resolved_target,
            session_format,
        ),
    };

    DaemonStatus {
        state,
        active_export_count: u32::from(frozen_pending),
        pending_recall_count: 0,
        buffer_capacity_seconds: capacity,
        retained_seconds: retained,
        dropped_frames: dropped,
        target,
        resolved_target,
        sample_rate,
        channel_count,
        format,
        last_error,
        lifecycle: lifecycle_state.status(ctx.clock.as_ref()),
    }
}

fn validate_runtime_environment(cfg: &LambConfig) -> Result<()> {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| LambError::Validation("XDG_RUNTIME_DIR does not exist".to_string()))?;
    let socket_path = cfg.control_socket_path.to_string_lossy();
    if !socket_path.starts_with(&runtime_dir) && socket_path.contains("%t") {
        return Err(LambError::Validation(
            "control socket path resolves outside runtime directory".to_string(),
        ));
    }
    if cfg.backend == "pipewire" {
        let pipewire_socket = Path::new(&runtime_dir).join("pipewire-0");
        if !pipewire_socket.exists() {
            return Err(LambError::Validation(format!(
                "PipeWire socket not reachable at {}",
                pipewire_socket.display()
            )));
        }
    }
    Ok(())
}

fn persistence_delivery_only_response(status: DaemonStatus) -> ControlResponse {
    ControlResponse {
        ok: false,
        message: "persistence commands require operation-worker delivery".to_string(),
        status: Some(status),
        error_context: crate::control::ControlErrorContext::default(),
        persistence_outcome: None,
        threshold_report: None,
    }
}

fn persistence_message_from_committed(outcome: &CommittedPersistenceRef<'_>) -> String {
    match outcome {
        CommittedPersistenceRef::Written { frames, losses, .. } => {
            persistence_message("written", *frames, losses.lost_frames())
        }
        CommittedPersistenceRef::SkippedSilent { frames, losses, .. } => {
            persistence_message("skipped exact-zero audio", *frames, losses.lost_frames())
        }
        CommittedPersistenceRef::SkippedByPolicy { frames, losses, .. } => {
            persistence_message("skipped by policy", *frames, losses.lost_frames())
        }
        CommittedPersistenceRef::NoNewAudio { losses } if losses.lost_frames() == 0 => {
            "no new audio".to_string()
        }
        CommittedPersistenceRef::NoNewAudio { losses } => format!(
            "no new audio; warning: {} frames lost before persistence",
            losses.lost_frames()
        ),
    }
}

fn drop_capture_then_session<B, S>(backend: Option<B>, session: Option<S>) {
    drop(backend);
    drop(session);
}

fn app_lock_error_response(ctx: &IdleDaemonContext) -> ControlResponse {
    ControlResponse {
        ok: false,
        message: "runtime state lock poisoned".to_string(),
        status: Some(idle_status_response(ctx)),
        error_context: crate::control::ControlErrorContext::default(),
        persistence_outcome: None,
        threshold_report: None,
    }
}

fn config_load_failure_response(ctx: &IdleDaemonContext) -> Option<ControlResponse> {
    ctx.runtime
        .lock()
        .ok()
        .and_then(|runtime| runtime.config_load_error.clone())
        .map(|error| ControlResponse {
            ok: false,
            message: error,
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        })
}

/// Profile-threshold mutations deliberately run only on the operation lane.
/// The runtime lock and any exact-session policy lock are deliberately acquired
/// before and held through the bounded atomic save. This can make direct Status
/// wait briefly, but leaves post-publication installation as infallible
/// assignments/moves with no lock reacquisition.
fn handle_app_threshold(ctx: &IdleDaemonContext, request: ThresholdRequest) -> ControlResponse {
    if let Some(response) = config_load_failure_response(ctx) {
        return response;
    }
    let result = match request {
        ThresholdRequest::Show { profile } => show_threshold(ctx, &profile),
        ThresholdRequest::Set {
            profile,
            channel,
            dbfs,
        } => set_threshold(ctx, &profile, &channel, dbfs),
        ThresholdRequest::Reset { profile, channel } => reset_threshold(ctx, &profile, &channel),
        ThresholdRequest::Calibrate {
            profile,
            channel,
            seconds,
        } => {
            if !(1..=30).contains(&seconds) {
                Err(LambError::Validation(
                    "calibration seconds must be within 1..=30".to_string(),
                ))
            } else {
                calibrate_threshold(ctx, &profile, &channel, seconds)
            }
        }
    };
    match result {
        Ok((message, report)) => ControlResponse {
            ok: true,
            message,
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: Some(report),
        },
        Err(error) => {
            set_app_last_error(ctx, error.to_string());
            ControlResponse {
                ok: false,
                message: error.to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
    }
}

fn session_calibration_target(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel_name: &str,
) -> Result<SessionCalibrationTarget> {
    let runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    if !runtime.config.profiles.contains_key(profile_name) {
        return Err(LambError::Config(format!(
            "profile {profile_name} does not exist"
        )));
    }
    let active = runtime
        .active_profile
        .as_ref()
        .ok_or_else(|| LambError::Control(format!("profile {profile_name} is not active")))?;
    if active.name != profile_name {
        return Err(LambError::Control(format!(
            "profile {profile_name} is not active"
        )));
    }
    let session = runtime
        .session
        .clone()
        .ok_or_else(|| LambError::Control(format!("profile {profile_name} is not capturing")))?;
    if session.profile_name != profile_name {
        return Err(LambError::Control(
            "active capture session identity changed".to_string(),
        ));
    }
    let index = session
        .configured_inputs
        .iter()
        .position(|input| input.name == channel_name)
        .ok_or_else(|| {
            LambError::Config(format!(
                "profile {profile_name} has no channel {channel_name}"
            ))
        })?;
    let live = session
        .resolved_live_inputs
        .get(index)
        .and_then(Clone::clone)
        .ok_or_else(|| {
            LambError::Control(format!(
                "profile {profile_name} channel {channel_name} has no durable live key"
            ))
        })?;
    let configured = session.configured_inputs[index].clone();
    let sample_rate = session.sample_rate;
    let capacity = session.calibration_sample_frames;
    let target = SessionCalibrationTarget {
        session,
        active_profile: active.clone(),
        channel: u32::try_from(index)
            .map_err(|_| LambError::Capture("channel index exceeds u32".to_string()))?,
        configured,
        live,
        sample_rate,
        capacity,
    };
    if ctx.stop.load(Ordering::Acquire) || !calibration_target_matches_runtime(&runtime, &target) {
        return Err(LambError::CaptureInvariant(
            "calibration target is not currently capturing and healthy",
        ));
    }
    Ok(target)
}

fn calibration_target_is_current(
    ctx: &IdleDaemonContext,
    target: &SessionCalibrationTarget,
) -> bool {
    if ctx.stop.load(Ordering::Acquire) {
        return false;
    }
    let Ok(runtime) = ctx.runtime.lock() else {
        return false;
    };
    calibration_target_matches_runtime(&runtime, target)
}

fn calibration_target_matches_runtime(
    runtime: &AppRuntimeState,
    target: &SessionCalibrationTarget,
) -> bool {
    let Some(session) = runtime.session.as_ref() else {
        return false;
    };
    let Ok(index) = usize::try_from(target.channel) else {
        return false;
    };
    let configured_matches_profile = target
        .active_profile
        .ports
        .get(index)
        .filter(|port| port.name == target.configured.name)
        .and_then(|port| configured_identity(&target.active_profile, &port.name).ok())
        .as_ref()
        == Some(&target.configured);
    runtime.state == "capturing"
        && runtime.capture_matches_target(target)
        && Arc::ptr_eq(session, &target.session)
        && runtime.active_profile.as_ref() == Some(&target.active_profile)
        && target.active_profile.name == session.profile_name
        && usize::try_from(session.channel_count).ok() == Some(session.configured_inputs.len())
        && session.configured_inputs.len() == session.resolved_live_inputs.len()
        && session.configured_inputs.len() == target.active_profile.ports.len()
        && configured_matches_profile
        && session.sample_rate == target.sample_rate
        && session.calibration_sample_frames == target.capacity
        && session.configured_inputs.get(index) == Some(&target.configured)
        && session.resolved_live_inputs.get(index) == Some(&Some(target.live.clone()))
        && runtime
            .capture_health
            .as_ref()
            .and_then(PipeWireHealth::fault)
            .is_none()
        && runtime
            .capture
            .as_ref()
            .and_then(CaptureBackend::runtime_error)
            .is_none()
}

#[cfg(test)]
fn observe_app_calibration(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel_name: &str,
    seconds: u32,
) -> Result<()> {
    let target = session_calibration_target(ctx, profile_name, channel_name)?;
    let frames = u64::from(target.sample_rate)
        .checked_mul(u64::from(seconds))
        .ok_or_else(|| LambError::Validation("calibration frame count overflow".to_string()))?;
    if frames == 0 || frames > target.capacity {
        return Err(LambError::Validation(
            "calibration frame count is outside startup capacity".to_string(),
        ));
    }
    let deadline = Duration::from_secs(u64::from(seconds) + 5);
    let lease = target.session.arena.calibrate_channel_until(
        crate::capture_arena::CalibrationCaptureRequest {
            channel: target.channel,
            frames,
        },
        deadline,
        || !calibration_target_is_current(ctx, &target),
    )?;
    if !calibration_target_is_current(ctx, &target) || !lease.metadata().usable {
        return Err(LambError::CaptureInvariant(
            "calibration observation was invalidated",
        ));
    }
    Ok(())
}

fn configured_identity(
    profile: &profile::ResolvedProfile,
    channel: &str,
) -> Result<crate::calibration::ConfiguredInputIdentity> {
    use crate::calibration::{ConfiguredDeviceSelector, ConfiguredInputIdentity, InputBackend};
    let port = profile
        .ports
        .iter()
        .find(|port| port.name == channel)
        .ok_or_else(|| {
            LambError::Config(format!("profile {} has no channel {channel}", profile.name))
        })?;
    match profile.backend.as_str() {
        "pipewire" => {
            let target = profile
                .pipewire_config
                .as_ref()
                .and_then(|config| config.target.as_deref())
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .map(|target| ConfiguredDeviceSelector::PipeWireTarget(target.to_string()))
                .unwrap_or(ConfiguredDeviceSelector::PipeWireAuto);
            ConfiguredInputIdentity::new(InputBackend::PipeWire, target, &port.name, &port.source)
        }
        "jack" => {
            let client = port
                .source
                .split_once(':')
                .map(|(client, _)| client)
                .ok_or_else(|| {
                    LambError::Validation("JACK source must be a complete client:port".to_string())
                })?;
            ConfiguredInputIdentity::new(
                InputBackend::Jack,
                ConfiguredDeviceSelector::JackSourceClient(client.to_string()),
                &port.name,
                &port.source,
            )
        }
        backend => Err(LambError::Validation(format!(
            "unsupported backend {backend}"
        ))),
    }
}

fn configured_identities(
    profile: &profile::ResolvedProfile,
) -> Result<Vec<crate::calibration::ConfiguredInputIdentity>> {
    profile
        .ports
        .iter()
        .map(|port| configured_identity(profile, &port.name))
        .collect()
}

fn pipewire_live_identities(
    profile: &profile::ResolvedProfile,
    target: &ResolvedTarget,
) -> Result<Vec<Option<crate::calibration::ResolvedLiveInputIdentity>>> {
    use crate::calibration::{InputBackend, ResolvedLiveInputIdentity};

    if profile.backend != "pipewire"
        || profile.ports.len() != target.source_ports.len()
        || u32::try_from(profile.ports.len()).ok() != Some(target.channels)
    {
        return Err(LambError::Capture(
            "configured PipeWire inputs do not match resolved capture sources".to_string(),
        ));
    }
    let Some((key_kind, key_value)) = target.durable_live_key() else {
        return Ok(vec![None; profile.ports.len()]);
    };
    target
        .source_ports
        .iter()
        .map(|port| {
            ResolvedLiveInputIdentity::new(InputBackend::PipeWire, key_kind, &key_value, &port.name)
                .map(Some)
        })
        .collect()
}

fn jack_live_identities(
    profile: &profile::ResolvedProfile,
) -> Result<Vec<Option<crate::calibration::ResolvedLiveInputIdentity>>> {
    use crate::calibration::{InputBackend, LiveDeviceKeyKind, ResolvedLiveInputIdentity};

    if profile.backend != "jack" {
        return Err(LambError::Capture(
            "JACK live identities require a JACK profile".to_string(),
        ));
    }
    profile
        .ports
        .iter()
        .map(|port| {
            let (client, _) = port.source.split_once(':').ok_or_else(|| {
                LambError::Validation("JACK source must be a complete client:port".to_string())
            })?;
            ResolvedLiveInputIdentity::new(
                InputBackend::Jack,
                LiveDeviceKeyKind::JackSourceClient,
                client,
                &port.source,
            )
            .map(Some)
        })
        .collect()
}

fn threshold_report(
    config: &app_config::AppConfig,
    profile_name: &str,
    active_profile: Option<&profile::ResolvedProfile>,
    session: Option<&CaptureSession>,
    calibration_root: &Path,
    now: u64,
) -> Result<ThresholdReport> {
    let profile_config = config
        .profiles
        .get(profile_name)
        .ok_or_else(|| LambError::Config(format!("profile {profile_name} does not exist")))?;
    let resolved = profile::validate_profile(profile_name, profile_config)?;
    let requested_active = active_profile.is_some_and(|active| active.name == profile_name);
    let capturing_session =
        session.filter(|session| session.profile_name == profile_name && requested_active);
    let (detector_name, detector_version) = match resolved.export_policy.activity.detector {
        crate::activity::ActivityDetectorKind::ExactZero => {
            ("exact-zero", crate::activity::EXACT_ZERO_DETECTOR_VERSION)
        }
        crate::activity::ActivityDetectorKind::WindowedRmsPeak => (
            "windowed-rms-peak",
            crate::activity::WINDOWED_RMS_PEAK_DETECTOR_VERSION,
        ),
        crate::activity::ActivityDetectorKind::FixedLevel
        | crate::activity::ActivityDetectorKind::CalibratedNoiseFloor => {
            return Err(LambError::Validation(
                "resolved profile contains an unsupported activity detector".to_string(),
            ));
        }
    };
    let channels = resolved
        .ports
        .iter()
        .enumerate()
        .map(|(index, port)| {
            let input = configured_identity(&resolved, &port.name)?;
            let threshold = profile_config
                .channels
                .get(&port.name)
                .and_then(|channel| channel.activity.as_ref());
            let stored = threshold.map(|threshold| StoredThresholdReport {
                threshold_dbfs: threshold.threshold_dbfs,
                source: threshold.threshold_source,
                updated_at_unix_seconds: threshold.updated_at_unix_seconds,
                age_seconds: now.checked_sub(threshold.updated_at_unix_seconds),
                calibration_id: threshold.calibration_id.clone(),
            });
            let inspection = threshold
                .filter(|threshold| {
                    threshold.threshold_source == crate::activity::ThresholdSource::Calibrated
                })
                .map(|threshold| {
                    crate::calibration::CalibrationStore::inspect_root(
                        calibration_root,
                        threshold,
                        &input,
                        now,
                    )
                })
                .transpose()?;
            let artifact_status = match (threshold, inspection.as_ref()) {
                (None, _) => CalibrationReportStatus::NotConfigured,
                (Some(threshold), _)
                    if threshold.threshold_source == crate::activity::ThresholdSource::Manual =>
                {
                    CalibrationReportStatus::NotApplicable
                }
                (_, Some(inspection)) => match &inspection.status {
                    crate::calibration::CalibrationArtifactStatus::Complete => {
                        CalibrationReportStatus::Complete
                    }
                    crate::calibration::CalibrationArtifactStatus::Stale(reason) => {
                        CalibrationReportStatus::Stale {
                            reason: reason.clone(),
                        }
                    }
                },
                _ => CalibrationReportStatus::NotConfigured,
            };
            let session_configured =
                capturing_session.and_then(|session| session.configured_inputs.get(index));
            let configured_identity_matches =
                capturing_session.map(|_| session_configured == Some(&input));
            let current_live = capturing_session
                .and_then(|session| session.resolved_live_inputs.get(index))
                .and_then(Option::as_ref);
            let current_live_identity = current_live.map(|live| crate::control::LiveInputReport {
                backend: live.backend,
                key_kind: live.key_kind,
                key_value: live.key_value.clone(),
                resolved_source: live.resolved_source.clone(),
            });
            let (calibration_evaluation, effective_threshold_dbfs) = match threshold {
                None => (CalibrationEvaluation::NotResolved, None),
                Some(threshold)
                    if threshold.threshold_source == crate::activity::ThresholdSource::Manual =>
                {
                    if threshold.input_id == input.input_id() {
                        (CalibrationEvaluation::Valid, Some(threshold.threshold_dbfs))
                    } else {
                        (
                            CalibrationEvaluation::Stale {
                                reason: crate::calibration::StaleReason::InputMismatch,
                            },
                            None,
                        )
                    }
                }
                Some(_) if capturing_session.is_none() => {
                    (CalibrationEvaluation::NotResolved, None)
                }
                Some(_) if configured_identity_matches == Some(false) => (
                    CalibrationEvaluation::Stale {
                        reason: crate::calibration::StaleReason::InputMismatch,
                    },
                    None,
                ),
                Some(threshold) => {
                    let session = capturing_session.expect("checked above");
                    match crate::calibration::CalibrationStore::validate_root(
                        calibration_root,
                        session.calibration_sample_frames,
                        threshold,
                        &input,
                        current_live,
                        session.sample_rate,
                        now,
                    )? {
                        crate::calibration::CalibrationValidity::Valid => {
                            (CalibrationEvaluation::Valid, Some(threshold.threshold_dbfs))
                        }
                        crate::calibration::CalibrationValidity::Stale(reason) => {
                            (CalibrationEvaluation::Stale { reason }, None)
                        }
                    }
                }
            };
            Ok(ThresholdChannelReport {
                channel: port.name.clone(),
                detector: detector_name.to_string(),
                detector_version: detector_version.to_string(),
                configured_input: ConfiguredInputReport {
                    backend: input.backend,
                    selector: input.selector.clone(),
                    source: input.source.clone(),
                    input_id: input.input_id().to_string(),
                },
                stored,
                artifact_status,
                current_live_identity,
                configured_identity_matches,
                calibration_evaluation,
                effective_threshold_dbfs,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ThresholdReport {
        profile: profile_name.to_string(),
        active_profile: requested_active,
        capturing: capturing_session.is_some(),
        channels,
        message: "threshold report".to_string(),
    })
}

fn show_threshold(
    ctx: &IdleDaemonContext,
    profile_name: &str,
) -> Result<(String, ThresholdReport)> {
    let runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    let report = threshold_report(
        &runtime.config,
        profile_name,
        runtime.active_profile.as_ref(),
        runtime.session.as_deref(),
        &ctx.calibration_root,
        unix_now(),
    )?;
    Ok(("threshold report".to_string(), report))
}

fn install_effective_session_activity_policy(
    config: &app_config::AppConfig,
    resolved: &profile::ResolvedProfile,
    session: &CaptureSession,
    calibration_root: &Path,
    now: u64,
) -> Result<()> {
    let report = threshold_report(
        config,
        &resolved.name,
        Some(resolved),
        Some(session),
        calibration_root,
        now,
    )?;
    let mut activity = resolved.export_policy.activity.clone();
    if activity.channels.len() != report.channels.len() {
        return Err(LambError::ControlInvariant(
            "session activity policy/report channel count changed",
        ));
    }
    for (policy_channel, report_channel) in activity.channels.iter_mut().zip(&report.channels) {
        if policy_channel.name != report_channel.channel {
            return Err(LambError::ControlInvariant(
                "session activity policy/report channel order changed",
            ));
        }
        if report_channel.effective_threshold_dbfs.is_none() {
            policy_channel.threshold = None;
        }
    }
    session
        .policy
        .lock()
        .map_err(|_| LambError::Control("session export policy lock poisoned".to_string()))?
        .activity = activity;
    Ok(())
}

fn set_threshold(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel: &str,
    dbfs: f64,
) -> Result<(String, ThresholdReport)> {
    if !dbfs.is_finite() || !(-120.0..=0.0).contains(&dbfs) {
        return Err(LambError::Validation(
            "threshold dBFS must be finite and within [-120.0, 0.0]".to_string(),
        ));
    }
    mutate_threshold(ctx, profile_name, channel, Some(dbfs))
}

fn calibrate_threshold(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel: &str,
    seconds: u32,
) -> Result<(String, ThresholdReport)> {
    calibrate_threshold_with(
        ctx,
        profile_name,
        channel,
        seconds,
        unix_now,
        &mut |_| Ok(()),
        &mut |_, _| Ok(()),
    )
}

fn calibrate_threshold_with<N>(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel_name: &str,
    seconds: u32,
    now: N,
    durability_hook: &mut crate::calibration::DurabilityHook<'_>,
    cleanup_hook: &mut crate::calibration::CleanupHook<'_>,
) -> Result<(String, ThresholdReport)>
where
    N: FnOnce() -> u64,
{
    if !(1..=30).contains(&seconds) {
        return Err(LambError::Validation(
            "calibration seconds must be within 1..=30".to_string(),
        ));
    }
    let target = session_calibration_target(ctx, profile_name, channel_name)?;
    let frames = u64::from(target.sample_rate)
        .checked_mul(u64::from(seconds))
        .ok_or_else(|| LambError::Validation("calibration frame count overflow".to_string()))?;
    if frames == 0 || frames > target.capacity {
        return Err(LambError::Validation(
            "calibration frame count is outside startup capacity".to_string(),
        ));
    }
    let mut lease = target.session.arena.calibrate_channel_until(
        crate::capture_arena::CalibrationCaptureRequest {
            channel: target.channel,
            frames,
        },
        Duration::from_secs(u64::from(seconds) + 5),
        || !calibration_target_is_current(ctx, &target),
    )?;
    if !calibration_target_is_current(ctx, &target) || !lease.metadata().usable {
        return Err(LambError::CaptureInvariant(
            "calibration observation was invalidated",
        ));
    }

    // This mutates only the arena-owned statistics; it neither clones the lease
    // nor creates a candidate until capture, target, and derivation are all valid.
    let threshold_dbfs = crate::calibration::derive_calibrated_threshold(lease.rms_mut())?;
    let created_at = now();
    let store = crate::calibration::CalibrationStore::new(&ctx.calibration_root, target.capacity)?;
    let mut prepared = store.prepare_generated_lease_with_hook(
        &target.configured,
        &target.live,
        &mut lease,
        threshold_dbfs,
        created_at,
        durability_hook,
    )?;
    drop(lease);

    if !calibration_target_is_current(ctx, &target) {
        return Err(LambError::CaptureInvariant(
            "calibration target changed before commit",
        ));
    }
    let mut runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    if ctx.stop.load(Ordering::Acquire) || !calibration_target_matches_runtime(&runtime, &target) {
        return Err(LambError::CaptureInvariant(
            "calibration target changed before commit",
        ));
    }
    let previous_activity = runtime
        .config
        .profiles
        .get(profile_name)
        .and_then(|profile| profile.channels.get(channel_name))
        .and_then(|channel| channel.activity.as_ref());
    let previous = match previous_activity.and_then(|activity| {
        activity
            .calibration_id
            .as_ref()
            .map(|id| (activity.input_id.as_str(), id.as_str()))
    }) {
        None => None,
        Some((input_id, id)) => match crate::calibration::RecordedGeneration::capture(
            ctx.calibration_root.join(input_id).join(id),
        ) {
            Ok(recorded) => Some(recorded),
            Err(LambError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                None
            }
            Err(error) => return Err(error),
        },
    };
    let mut candidate = runtime.config.clone();
    let channel = candidate
        .profiles
        .get_mut(profile_name)
        .ok_or_else(|| LambError::Config(format!("profile {profile_name} does not exist")))?
        .channels
        .entry(channel_name.to_string())
        .or_default();
    channel.activity = Some(crate::app_config::ActivityThresholdConfig {
        threshold_dbfs: f64::from(threshold_dbfs),
        threshold_source: crate::activity::ThresholdSource::Calibrated,
        updated_at_unix_seconds: created_at,
        input_id: target.configured.input_id().to_string(),
        calibration_id: Some(prepared.calibration_id().to_string()),
    });
    app_config::validate_persisted_config(&candidate)?;
    let candidate_resolved = profile::validate_profile(
        profile_name,
        candidate
            .profiles
            .get(profile_name)
            .expect("candidate profile exists"),
    )?;
    let report = threshold_report(
        &candidate,
        profile_name,
        Some(&candidate_resolved),
        Some(&target.session),
        &ctx.calibration_root,
        created_at,
    )?;
    let mut effective_activity = candidate_resolved.export_policy.activity.clone();
    if effective_activity.channels.len() != report.channels.len() {
        return Err(LambError::ControlInvariant(
            "candidate activity policy/report channel count changed",
        ));
    }
    for (policy_channel, report_channel) in
        effective_activity.channels.iter_mut().zip(&report.channels)
    {
        if policy_channel.name != report_channel.channel {
            return Err(LambError::ControlInvariant(
                "candidate activity policy/report channel order changed",
            ));
        }
        if report_channel.effective_threshold_dbfs.is_none() {
            policy_channel.threshold = None;
        }
    }
    let mut policy_guard = target
        .session
        .policy
        .lock()
        .map_err(|_| LambError::Control("session export policy lock poisoned".to_string()))?;
    let installed_config = candidate.clone();
    let installed_profile = candidate_resolved.clone();
    let cleanup = crate::calibration::commit_prepared_generation_with_hooks(
        &ctx.config_path,
        &candidate,
        &mut prepared,
        previous,
        || {
            policy_guard.activity = effective_activity;
            runtime.config = installed_config;
            runtime.active_profile = Some(installed_profile);
        },
        durability_hook,
        cleanup_hook,
    )?;
    drop(policy_guard);
    drop(runtime);
    let message = match cleanup {
        crate::calibration::OldGenerationCleanup::NotRequested
        | crate::calibration::OldGenerationCleanup::Removed => "threshold calibrated".to_string(),
        crate::calibration::OldGenerationCleanup::Pending(path) => {
            format!(
                "threshold calibrated; old calibration cleanup pending: {}",
                path.display()
            )
        }
    };
    Ok((message, report))
}

fn reset_threshold(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel: &str,
) -> Result<(String, ThresholdReport)> {
    mutate_threshold(ctx, profile_name, channel, None)
}

fn mutate_threshold(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel: &str,
    dbfs: Option<f64>,
) -> Result<(String, ThresholdReport)> {
    mutate_threshold_with(
        ctx,
        profile_name,
        channel,
        dbfs,
        unix_now,
        profile::save_config,
        |root, referenced| crate::calibration::CalibrationStore::cleanup_root(root, referenced),
    )
}

fn mutate_threshold_with<N, S, C>(
    ctx: &IdleDaemonContext,
    profile_name: &str,
    channel: &str,
    dbfs: Option<f64>,
    now: N,
    save_config: S,
    cleanup_root: C,
) -> Result<(String, ThresholdReport)>
where
    N: FnOnce() -> u64,
    S: FnOnce(&Path, &app_config::AppConfig) -> Result<()>,
    C: FnOnce(&Path, &std::collections::BTreeSet<(String, String)>) -> Result<Vec<PathBuf>>,
{
    let mut runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    let mut candidate = runtime.config.clone();
    let resolved = profile::validate_profile(
        profile_name,
        candidate
            .profiles
            .get(profile_name)
            .ok_or_else(|| LambError::Config(format!("profile {profile_name} does not exist")))?,
    )?;
    let identity = configured_identity(&resolved, channel)?;
    let now = now();
    {
        let profile_config = candidate
            .profiles
            .get_mut(profile_name)
            .expect("validated above");
        let channel_config = profile_config
            .channels
            .entry(channel.to_string())
            .or_default();
        channel_config.activity =
            dbfs.map(
                |threshold_dbfs| crate::app_config::ActivityThresholdConfig {
                    threshold_dbfs,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: now,
                    input_id: identity.input_id().to_string(),
                    calibration_id: channel_config
                        .activity
                        .as_ref()
                        .and_then(|previous| {
                            (previous.input_id == identity.input_id())
                                .then(|| previous.calibration_id.clone())
                        })
                        .flatten(),
                },
            );
    }
    app_config::validate_persisted_config(&candidate)?;
    let candidate_resolved = profile::validate_profile(
        profile_name,
        candidate
            .profiles
            .get(profile_name)
            .expect("validated above"),
    )?;
    let requested_active = runtime
        .active_profile
        .as_ref()
        .is_some_and(|profile| profile.name == profile_name);
    let matching_session = runtime
        .session
        .as_ref()
        .filter(|session| requested_active && session.profile_name == profile_name)
        .cloned();
    let report_active_profile = if requested_active {
        Some(&candidate_resolved)
    } else {
        runtime.active_profile.as_ref()
    };
    let report = threshold_report(
        &candidate,
        profile_name,
        report_active_profile,
        matching_session.as_deref(),
        &ctx.calibration_root,
        now,
    )?;
    let mut effective_activity = candidate_resolved.export_policy.activity.clone();
    if effective_activity.channels.len() != report.channels.len() {
        return Err(LambError::ControlInvariant(
            "candidate activity policy/report channel count changed",
        ));
    }
    for (policy_channel, report_channel) in
        effective_activity.channels.iter_mut().zip(&report.channels)
    {
        if policy_channel.name != report_channel.channel {
            return Err(LambError::ControlInvariant(
                "candidate activity policy/report channel order changed",
            ));
        }
        if report_channel.effective_threshold_dbfs.is_none() {
            policy_channel.threshold = None;
        }
    }
    let mut policy_guard = matching_session
        .as_ref()
        .map(|session| {
            session
                .policy
                .lock()
                .map_err(|_| LambError::Control("session export policy lock poisoned".to_string()))
        })
        .transpose()?;
    let referenced = dbfs.is_none().then(|| calibration_references(&candidate));
    let success_message = if dbfs.is_some() {
        "threshold updated".to_string()
    } else {
        "threshold reset".to_string()
    };

    save_config(&ctx.config_path, &candidate)?;
    if let Some(policy) = policy_guard.as_deref_mut() {
        policy.activity = effective_activity;
    }
    runtime.config = candidate;
    if requested_active {
        runtime.active_profile = Some(candidate_resolved);
    }
    drop(policy_guard);
    drop(runtime);

    let message = match referenced {
        None => success_message,
        Some(referenced) => match cleanup_root(&ctx.calibration_root, &referenced) {
            Ok(pending) if pending.is_empty() => success_message,
            Ok(pending) => format!(
                "{success_message}; calibration cleanup pending for {} path(s)",
                pending.len()
            ),
            Err(error) => format!("{success_message}; calibration cleanup warning: {error}"),
        },
    };
    Ok((message, report))
}

fn calibration_references(
    config: &app_config::AppConfig,
) -> std::collections::BTreeSet<(String, String)> {
    config
        .profiles
        .values()
        .flat_map(|profile| profile.channels.values())
        .filter_map(|channel| channel.activity.as_ref())
        .filter_map(|threshold| {
            threshold
                .calibration_id
                .as_ref()
                .map(|calibration_id| (threshold.input_id.clone(), calibration_id.clone()))
        })
        .collect()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn app_persistence_delivery(
    ctx: &IdleDaemonContext,
    command: ExportCommand,
    mut stream: UnixStream,
) {
    let session = match ctx.runtime.lock() {
        Ok(runtime) => runtime.session.clone(),
        Err(error) => {
            drop(error.into_inner());
            let response = app_lock_error_response(ctx);
            let _ = write_response(stream, &response);
            return;
        }
    };
    let Some(session) = session else {
        let response = ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
        let _ = write_response(stream, &response);
        return;
    };
    let timestamp = iso8601_compact_label();
    let mut entered = false;
    let result = session.persist_with_delivery(command, &timestamp, |outcome| {
        entered = true;
        let status = idle_status_response(ctx);
        let message = persistence_message_from_committed(&outcome);
        write_persistence_response(
            &mut stream,
            true,
            &message,
            &status,
            session.sample_rate,
            outcome,
        )
    });
    if let Err(error) = result {
        set_app_last_error(ctx, error.to_string());
        if !entered {
            let response = ControlResponse {
                ok: false,
                message: error.to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            };
            let _ = write_response(stream, &response);
        }
    }
}

#[cfg(test)]
fn handle_app_recall(ctx: &IdleDaemonContext) -> ControlResponse {
    let session = match ctx.runtime.lock() {
        Ok(runtime) => runtime.session.clone(),
        Err(error) => {
            drop(error.into_inner());
            return app_lock_error_response(ctx);
        }
    };
    let Some(session) = session else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
    };

    let timestamp = iso8601_compact_label();
    let result = session.persist(ExportCommand::Recall, &timestamp);
    app_persistence_response(ctx, result, session.sample_rate)
}

fn handle_app_clear(ctx: &IdleDaemonContext) -> ControlResponse {
    let session = match ctx.runtime.lock() {
        Ok(runtime) => runtime.session.clone(),
        Err(error) => {
            drop(error.into_inner());
            return app_lock_error_response(ctx);
        }
    };
    let Some(session) = session else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
    };
    match session.clear() {
        Ok(()) => ControlResponse {
            ok: true,
            message: "cleared".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        },
        Err(err) => {
            set_app_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
    }
}

#[cfg(test)]
fn handle_app_dump(ctx: &IdleDaemonContext) -> ControlResponse {
    let session = match ctx.runtime.lock() {
        Ok(runtime) => runtime.session.clone(),
        Err(error) => {
            drop(error.into_inner());
            return app_lock_error_response(ctx);
        }
    };
    let Some(session) = session else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            error_context: crate::control::ControlErrorContext::default(),
            persistence_outcome: None,
            threshold_report: None,
        };
    };

    let timestamp = iso8601_compact_label();
    let result = session.persist(ExportCommand::Dump, &timestamp);
    app_persistence_response(ctx, result, session.sample_rate)
}

#[cfg(test)]
fn app_persistence_response(
    ctx: &IdleDaemonContext,
    result: Result<DumpOutcome>,
    sample_rate: u32,
) -> ControlResponse {
    match result {
        Ok(outcome) => persistence_response(outcome, sample_rate, idle_status_response(ctx)),
        Err(err) => {
            set_app_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(idle_status_response(ctx)),
                error_context: crate::control::ControlErrorContext::default(),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
    }
}

#[cfg(test)]
fn persistence_response(
    outcome: DumpOutcome,
    sample_rate: u32,
    status: DaemonStatus,
) -> ControlResponse {
    let (message, persistence_outcome) = match outcome {
        DumpOutcome::Written {
            range,
            frames,
            export_start_frame,
            export_frames,
            losses,
            output_directory,
            files,
        } => {
            let lost_frames = losses.lost_frames();
            let message = persistence_message("written", frames, lost_frames);
            (
                message,
                PersistenceOutcomeResponse::Written {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    export_start_frame,
                    export_frames,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                    output_directory,
                    files,
                },
            )
        }
        DumpOutcome::SkippedSilent {
            range,
            frames,
            losses,
        } => {
            let lost_frames = losses.lost_frames();
            let message = persistence_message("skipped exact-zero audio", frames, lost_frames);
            (
                message,
                PersistenceOutcomeResponse::SkippedSilent {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                },
            )
        }
        DumpOutcome::SkippedByPolicy {
            range,
            frames,
            losses,
        } => {
            let lost_frames = losses.lost_frames();
            let message = persistence_message("skipped by policy", frames, lost_frames);
            (
                message,
                PersistenceOutcomeResponse::SkippedByPolicy {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                },
            )
        }
        DumpOutcome::NoNewAudio { losses } => {
            let lost_frames = losses.lost_frames();
            let message = if lost_frames == 0 {
                "no new audio".to_string()
            } else {
                format!("no new audio; warning: {lost_frames} frames lost before persistence")
            };
            (
                message,
                PersistenceOutcomeResponse::NoNewAudio {
                    lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                },
            )
        }
    };
    ControlResponse {
        ok: true,
        message,
        status: Some(status),
        error_context: crate::control::ControlErrorContext::default(),
        persistence_outcome: Some(persistence_outcome),
        threshold_report: None,
    }
}

fn persistence_message(kind: &str, frames: u64, lost_frames: u64) -> String {
    if lost_frames == 0 {
        format!("{kind}: {frames} frames")
    } else {
        format!("{kind}: {frames} frames; warning: {lost_frames} frames lost before persistence")
    }
}

fn set_app_last_error(ctx: &IdleDaemonContext, error: String) {
    if let Ok(mut runtime) = ctx.runtime.lock() {
        runtime.last_error = Some(error);
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_loaded_app_config_inner(
    state: &mut AppRuntimeState,
    loaded: app_config::LoadedAppConfig,
    path: &Path,
    calibration_root: &Path,
    app_starter: &dyn LegacyCaptureStarter,
    startup_recovery: &dyn LegacyStartupRecovery,
    now: RetryInstant,
    fault_sink: RuntimeFaultSink,
) -> Result<()> {
    apply_loaded_app_config_inner_with_checkpoint(
        state,
        loaded,
        path,
        calibration_root,
        app_starter,
        startup_recovery,
        now,
        true,
        fault_sink,
        || Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_loaded_app_config_inner_with_checkpoint<F>(
    state: &mut AppRuntimeState,
    loaded: app_config::LoadedAppConfig,
    path: &Path,
    calibration_root: &Path,
    app_starter: &dyn LegacyCaptureStarter,
    startup_recovery: &dyn LegacyStartupRecovery,
    now: RetryInstant,
    begin_attempt_generation: bool,
    fault_sink: RuntimeFaultSink,
    mut checkpoint: F,
) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    match loaded.state {
        ConfigLoadState::Loaded => {
            state.config = loaded.config.clone();
            let active_profile = profile::resolve_active_profile(&loaded.config)?;
            if let Some(profile) = active_profile {
                state.active_profile = Some(profile.clone());
                if state.config.daemon.start_mode == "auto" {
                    if begin_attempt_generation {
                        state.lifecycle.begin_operation()?;
                    }
                    checkpoint()?;
                    state.capture.take();
                    state.capture_health = None;
                    state.session = None;
                    let channel_names: Vec<String> =
                        profile.ports.iter().map(|p| p.name.clone()).collect();
                    let params = app_runtime_params(&profile);
                    let injected =
                        match app_starter.prepare_app(&profile, params, fault_sink.clone()) {
                            Ok(injected) => injected,
                            Err(error) => {
                                apply_capture_attempt_error_to_state(state, error, now)
                                    .map_err(|error| LambError::DaemonFatal(error.message))?;
                                return Ok(());
                            }
                        };
                    let build = match injected {
                        Some(prepared) => Ok((
                            prepared.backend,
                            prepared.runtime,
                            prepared.health,
                            Some((
                                prepared.resolved_live_inputs,
                                prepared.session_resource_probes,
                            )),
                        )),
                        None => match profile.backend.as_str() {
                            "jack" => JackCapture::start(
                                JackCaptureConfig::from_profile(&profile),
                                params,
                            )
                            .map(|(capture, runtime)| {
                                (
                                    CaptureBackend::Jack(capture, channel_names),
                                    runtime,
                                    None,
                                    None,
                                )
                            })
                            .map_err(classify_legacy_attempt_error),
                            "pipewire" => {
                                if let Some(pw_cfg) = profile.pipewire_config.clone() {
                                    start_app_pipewire_capture(pw_cfg, params, fault_sink).map(
                                        |(capture, runtime)| {
                                            (
                                                CaptureBackend::PipeWire(capture, channel_names),
                                                runtime,
                                                None,
                                                None,
                                            )
                                        },
                                    )
                                } else {
                                    Err(classify_legacy_attempt_error(LambError::Validation(
                                        "pipewire profile has no pipewire config".to_string(),
                                    )))
                                }
                            }
                            other => Err(classify_legacy_attempt_error(LambError::Validation(
                                format!("unknown backend: {other}"),
                            ))),
                        },
                    };
                    checkpoint()?;
                    match build {
                        Ok((backend, runtime, injected_health, injected_inputs)) => {
                            checkpoint()?;
                            let mut attempt = PrivateAppCaptureAttempt::new(backend, runtime);
                            let sample_rate = attempt.backend().sample_rate();
                            let resolved_live_inputs = if let Some((resolved, ..)) =
                                &injected_inputs
                            {
                                resolved.clone()
                            } else {
                                match attempt.backend() {
                                    CaptureBackend::Fake(_, _) => {
                                        unreachable!(
                                            "fake capture is not used by app-profile runtime"
                                        )
                                    }
                                    CaptureBackend::Jack(_, _) => jack_live_identities(&profile)?,
                                    CaptureBackend::PipeWire(capture, _) => {
                                        pipewire_live_identities(
                                            &profile,
                                            capture.resolved_target(),
                                        )?
                                    }
                                    #[cfg(test)]
                                    CaptureBackend::TestProbe(_) => {
                                        unreachable!("test-only capture backend")
                                    }
                                    #[cfg(test)]
                                    CaptureBackend::TestAppProbe { .. } => {
                                        unreachable!(
                                            "test app backend requires injected identities"
                                        )
                                    }
                                }
                            };
                            let (configured_inputs, channel_count) =
                                validate_app_session_inputs(&profile, &resolved_live_inputs)?;
                            let capture_runtime = attempt.take_runtime();
                            let session = CaptureSession::from_validated_app_runtime(
                                capture_runtime,
                                &profile,
                                sample_rate,
                                configured_inputs,
                                resolved_live_inputs,
                                channel_count,
                            );
                            #[cfg(test)]
                            let mut session = session;
                            #[cfg(test)]
                            if let Some((_, probes)) = injected_inputs {
                                session.attempt_resource_probes = probes;
                            }
                            attempt.set_session(Arc::new(session));
                            attempt.run_post_backend_step(|session| {
                                checkpoint()?;
                                install_effective_session_activity_policy(
                                    &loaded.config,
                                    &profile,
                                    session,
                                    calibration_root,
                                    unix_now(),
                                )
                            })?;
                            checkpoint()?;
                            let recovery_failed =
                                startup_recovery.failed_count(attempt.session())?;
                            if recovery_failed != 0 {
                                return Err(LambError::Control(format!(
                                    "app startup recovery failed for {recovery_failed} transaction(s)"
                                )));
                            }
                            checkpoint()?;
                            state.state = "capturing".to_string();
                            state.last_error = None;
                            state.capture_health =
                                injected_health.or_else(|| match attempt.backend() {
                                    CaptureBackend::PipeWire(capture, _) => Some(capture.health()),
                                    CaptureBackend::Fake(_, _) => {
                                        unreachable!(
                                            "fake capture is not used by app-profile runtime"
                                        )
                                    }
                                    CaptureBackend::Jack(_, _) => None,
                                    #[cfg(test)]
                                    CaptureBackend::TestProbe(_) => None,
                                    #[cfg(test)]
                                    CaptureBackend::TestAppProbe { .. } => None,
                                });
                            let (backend, session) = attempt.publish();
                            state.session = Some(session);
                            state.capture = Some(backend);
                            state.lifecycle.mark_running(
                                Some(profile.name.clone()),
                                Some(profile.name.clone()),
                            );
                        }
                        Err(error) => {
                            state.capture_health = None;
                            apply_capture_attempt_error_to_state(state, error, now)
                                .map_err(|error| LambError::DaemonFatal(error.message))?;
                        }
                    }
                } else {
                    state.state = "idle".to_string();
                    state.last_error = None;
                    state.capture = None;
                    state.capture_health = None;
                    state.session = None;
                    state.lifecycle.mark_stopped(Some(profile.name.clone()));
                }
            } else {
                state.state = "unconfigured".to_string();
                state.last_error = Some("no active profile configured".to_string());
                state.active_profile = None;
                state.capture = None;
                state.capture_health = None;
                state.session = None;
                state.lifecycle.mark_stopped(None);
            }
            state.config_load_error = None;
            Ok(())
        }
        ConfigLoadState::Missing => {
            let error = format!("config file not found: {}", path.display());
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = Some(error.clone());
            state.config_load_error = Some(error.clone());
            state.active_profile = None;
            state.capture = None;
            state.capture_health = None;
            state.session = None;
            state.lifecycle.mark_permanent(error);
            Ok(())
        }
        ConfigLoadState::Invalid => {
            let error = loaded
                .error
                .unwrap_or_else(|| format!("invalid config file: {}", path.display()));
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = Some(error.clone());
            state.config_load_error = Some(error.clone());
            state.active_profile = None;
            state.capture = None;
            state.capture_health = None;
            state.session = None;
            state.lifecycle.mark_permanent(error);
            Ok(())
        }
    }
}

fn iso8601_compact_label() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs_in_day: u64 = 86400;
    let days = seconds / secs_in_day;
    let day_secs = seconds % secs_in_day;

    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    format!("{y:04}{m:02}{d:02}{hour:02}{minute:02}{sec:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon_lifecycle::{RetryInstant, ScheduledOperation};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::sync::Condvar;

    const ROUTE_TEST_TIMEOUT: Duration = Duration::from_secs(2);

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
    fn socket_owner_does_not_adopt_or_chmod_replacement_during_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let error = ControlSocketOwner::bind_with_hooks(
            path.clone(),
            || {
                std::fs::write(&path, b"foreign replacement").unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("changed during socket publication"));
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn socket_owner_cleans_private_path_when_pin_fails_after_bind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let error = ControlSocketOwner::bind_with_io(
            path.clone(),
            || {},
            |_| Err(LambError::Control("injected pin failure".to_string())),
            |pinned| pinned.metadata().map_err(|source| io_error(&path, source)),
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected pin failure"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn socket_owner_cleans_private_path_when_pinned_metadata_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let error = ControlSocketOwner::bind_with_io(
            path.clone(),
            || {},
            pin_socket_path,
            |_| {
                Err(LambError::Control(
                    "injected pinned metadata failure".to_string(),
                ))
            },
            |_| Ok(()),
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("injected pinned metadata failure"));
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
    fn socket_owner_preserves_foreign_live_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let foreign = UnixListener::bind(&path).unwrap();
        let identity = std::fs::symlink_metadata(&path).unwrap();

        assert!(ControlSocketOwner::bind(path.clone()).is_err());

        let preserved = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(preserved.dev(), identity.dev());
        assert_eq!(preserved.ino(), identity.ino());
        drop(foreign);
    }

    #[test]
    fn socket_owner_live_probe_queues_no_stream_connection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let foreign = UnixListener::bind(&path).unwrap();
        foreign.set_nonblocking(true).unwrap();

        assert!(ControlSocketOwner::bind(path.clone()).is_err());

        let accept_error = foreign.accept().unwrap_err();
        assert_eq!(accept_error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(path.exists());
    }

    #[test]
    fn socket_owner_preserves_socket_on_unexpected_probe_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let foreign = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        let identity = std::fs::symlink_metadata(&path).unwrap();

        assert!(ControlSocketOwner::bind(path.clone()).is_err());

        let preserved = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(preserved.dev(), identity.dev());
        assert_eq!(preserved.ino(), identity.ino());
        drop(foreign);
    }

    #[test]
    fn socket_owner_replaces_stale_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);

        let owner = ControlSocketOwner::bind(path.clone()).unwrap();
        let replacement_connection = UnixStream::connect(&path).unwrap();
        drop(replacement_connection);
        drop(owner);
        assert!(!path.exists());
    }

    #[test]
    fn nonblocking_wake_tolerates_refused_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);

        nonblocking_control_wake(&path).unwrap();

        assert!(path.exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn socket_owner_removes_owned_path_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let owner = ControlSocketOwner::bind(path.clone()).unwrap();
        assert!(path.exists());
        drop(owner);
        assert!(!path.exists());
    }

    #[test]
    fn fatal_listener_failure_exits_1_and_removes_socket() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fatal-listener.sock");
        let result: Result<()> = (|| {
            let _owner = ControlSocketOwner::bind(path.clone())?;
            assert!(path.exists());
            Err(LambError::ControlInvariant(
                "injected fatal listener failure",
            ))
        })();

        let error = result.unwrap_err();
        assert_eq!(error.process_exit_code(), 1);
        assert_eq!(
            error.to_string(),
            "control error: injected fatal listener failure"
        );
        assert!(!path.exists());
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

    #[test]
    fn resolving_four_pipewire_ports_preserves_static_config() {
        let prepared = prepare_legacy_config(test_legacy_pipewire_config()).unwrap();
        let before = prepared.static_config.as_ref().clone();
        let resolved =
            resolve_legacy_capture_with(&prepared, |_| Ok(test_resolved_target(4, 44_100)))
                .unwrap();

        assert_eq!(resolved.state.channel_count, 4);
        assert_eq!(resolved.state.sample_rate, 44_100);
        assert_eq!(prepared.static_config.as_ref(), &before);
        assert_eq!(prepared.static_config.channels, None);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        dropped: Mutex<Vec<ProbeResource>>,
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

        fn dropped(&self) -> Vec<ProbeResource> {
            self.dropped.lock().unwrap().clone()
        }
    }

    struct AttemptResourceLease {
        counters: Arc<AttemptResourceCounters>,
        resource: ProbeResource,
    }

    impl Drop for AttemptResourceLease {
        fn drop(&mut self) {
            self.counters.live[self.resource as usize].fetch_sub(1, Ordering::SeqCst);
            self.counters.dropped.lock().unwrap().push(self.resource);
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SupervisorSnapshot {
        prepared_fingerprint: String,
        backend_identity: Option<usize>,
        session_identity: Option<usize>,
        lifecycle: LifecycleState,
        config_family: ConfigFamily,
        status_json: String,
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

    struct BlockingLegacyCaptureStarter {
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
        active: Mutex<Option<ActiveCapture>>,
    }

    struct ProductionPathAppStarter {
        resources: Arc<AttemptResourceCounters>,
    }

    struct TransientAppStarter {
        calls: AtomicUsize,
    }

    struct TransientThenSuccessfulAppStarter {
        calls: AtomicUsize,
        resources: Arc<AttemptResourceCounters>,
    }

    struct CountingSuccessfulRecovery {
        calls: AtomicUsize,
    }

    #[derive(Default)]
    struct DaemonManualClock {
        now_millis: AtomicU64,
    }

    impl RetryClock for DaemonManualClock {
        fn now(&self) -> RetryInstant {
            RetryInstant::from_millis(self.now_millis.load(Ordering::SeqCst))
        }

        fn unix_seconds(&self, instant: RetryInstant) -> u64 {
            instant.as_millis() / 1_000
        }
    }

    struct SuccessfulLegacyRecovery;

    struct ScriptedLegacyRecovery(Result<()>);

    struct NonzeroAppRecovery;

    impl LegacyStartupRecovery for SuccessfulLegacyRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            Ok(0)
        }
    }

    impl LegacyStartupRecovery for CountingSuccessfulRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    impl LegacyStartupRecovery for ScriptedLegacyRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            match &self.0 {
                Ok(()) => Ok(0),
                Err(error) => Err(LambError::Control(error.to_string())),
            }
        }
    }

    impl LegacyStartupRecovery for NonzeroAppRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            Ok(1)
        }
    }

    impl LegacyCaptureStarter for ScriptedLegacyCaptureStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted capture outcome")
        }
    }

    impl LegacyCaptureStarter for BlockingLegacyCaptureStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            self.entered.wait();
            self.release.wait();
            Ok(self.active.lock().unwrap().take().unwrap())
        }
    }

    impl LegacyCaptureStarter for ProductionPathAppStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            unreachable!("app production-path starter does not start legacy capture")
        }

        fn prepare_app(
            &self,
            profile: &profile::ResolvedProfile,
            params: CaptureRuntimeParams,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<Option<PreparedAppCapture>, CaptureAttemptError> {
            let (runtime, _ingress) =
                CaptureRuntime::build(params, 100, u32::try_from(profile.ports.len()).unwrap())
                    .map_err(classify_legacy_attempt_error)?;
            Ok(Some(PreparedAppCapture {
                backend: CaptureBackend::TestAppProbe {
                    resource: Box::new(self.resources.lease(ProbeResource::Backend)),
                    sample_rate: 100,
                },
                runtime,
                health: None,
                resolved_live_inputs: jack_live_identities(profile)
                    .map_err(classify_legacy_attempt_error)?,
                session_resource_probes: vec![
                    Box::new(self.resources.lease(ProbeResource::Session)),
                    Box::new(self.resources.lease(ProbeResource::Arena)),
                    Box::new(self.resources.lease(ProbeResource::Workspace)),
                    Box::new(self.resources.lease(ProbeResource::Descriptor)),
                ],
            }))
        }
    }

    impl LegacyCaptureStarter for TransientAppStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            unreachable!("app starter does not start legacy capture")
        }

        fn prepare_app(
            &self,
            _profile: &profile::ResolvedProfile,
            _params: CaptureRuntimeParams,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<Option<PreparedAppCapture>, CaptureAttemptError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "app device unavailable".to_string(),
            })
        }
    }

    impl LegacyCaptureStarter for TransientThenSuccessfulAppStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            unreachable!("app starter does not start legacy capture")
        }

        fn prepare_app(
            &self,
            profile: &profile::ResolvedProfile,
            params: CaptureRuntimeParams,
            fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<Option<PreparedAppCapture>, CaptureAttemptError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(CaptureAttemptError {
                    class: ErrorClass::Transient,
                    capture_state: CaptureState::WaitingForDevice,
                    message: "app device unavailable".to_string(),
                });
            }
            ProductionPathAppStarter {
                resources: Arc::clone(&self.resources),
            }
            .prepare_app(profile, params, fault_sink)
        }
    }

    impl SupervisorHarness {
        fn running() -> Self {
            let harness = Self::stopped();
            let active = test_active_capture_with_resources(&harness.resources);
            let resolved = active.resolved.clone();
            let mut active = active;
            let session = active.session.take().unwrap();
            let backend = active.backend.take().unwrap();
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            runtime.capture = Some(backend);
            runtime.session = Some(session);
            runtime.resolved_capture = Some(resolved.clone());
            runtime.state = "capturing".to_string();
            runtime
                .lifecycle
                .mark_running(None, resolved.resolved_target);
            drop(runtime);
            harness
        }

        fn stopped() -> Self {
            Self::stopped_with_outcomes(VecDeque::new())
        }

        fn stopped_with_outcomes(
            outcomes: VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let config_path = temp.path().join("lamb.toml");
            fs::write(
                &config_path,
                toml::to_string(&test_legacy_fake_config()).unwrap(),
            )
            .unwrap();
            let bootstrap = bootstrap_config(&config_path).unwrap();
            let prepared = prepare_bootstrap_config(&bootstrap);
            let ctx = build_idle_context(bootstrap, prepared).unwrap();
            {
                let mut runtime = ctx.runtime.lock().unwrap();
                runtime.state = "stopped".to_string();
                runtime.lifecycle.mark_stopped(None);
            }
            Self {
                _temp: temp,
                config_path,
                ctx,
                starter: ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(outcomes),
                },
                resources: Arc::new(AttemptResourceCounters::default()),
            }
        }

        fn reload_with_text(&self, text: &str) -> ControlResponse {
            let replacement = self.config_path.with_extension("replacement");
            fs::write(&replacement, text).unwrap();
            fs::rename(replacement, &self.config_path).unwrap();
            reload_daemon_config_with_recovery(
                &self.ctx,
                None,
                &self.starter,
                &SuccessfulLegacyRecovery,
            )
        }

        fn start_capture(&self) -> ControlResponse {
            start_capture_transaction_with_recovery(
                &self.ctx,
                None,
                false,
                &self.starter,
                &SuccessfulLegacyRecovery,
            )
        }

        fn stop_capture(&self) -> ControlResponse {
            stop_capture_transaction(&self.ctx)
        }

        fn snapshot(&self) -> SupervisorSnapshot {
            let runtime = self.ctx.runtime.lock().unwrap();
            let mut snapshot = SupervisorSnapshot {
                prepared_fingerprint: toml::to_string(
                    runtime
                        .prepared_legacy
                        .as_ref()
                        .expect("prepared legacy config")
                        .static_config
                        .as_ref(),
                )
                .unwrap(),
                backend_identity: runtime
                    .capture
                    .as_ref()
                    .map(|backend| backend as *const CaptureBackend as usize),
                session_identity: runtime
                    .session
                    .as_ref()
                    .map(|session| Arc::as_ptr(session) as usize),
                lifecycle: runtime.lifecycle.clone(),
                config_family: runtime.config_family,
                status_json: String::new(),
            };
            drop(runtime);
            snapshot.status_json = serde_json::to_string(&idle_status_response(&self.ctx)).unwrap();
            snapshot
        }

        fn lifecycle(&self) -> LifecycleState {
            self.ctx.runtime.lock().unwrap().lifecycle.clone()
        }

        fn stop_flag(&self) -> bool {
            self.ctx.stop.load(Ordering::Acquire)
        }

        fn live_capture_resources(&self) -> usize {
            [
                ProbeResource::Backend,
                ProbeResource::Session,
                ProbeResource::Arena,
                ProbeResource::Workspace,
                ProbeResource::Descriptor,
            ]
            .into_iter()
            .map(|resource| self.resources.live(resource))
            .sum()
        }
    }

    fn assert_visible_authority_preserved_with_one_generation_advance(
        before: &SupervisorSnapshot,
        after: &SupervisorSnapshot,
    ) {
        assert_eq!(
            after.lifecycle.generation,
            before.lifecycle.generation.checked_add(1).unwrap()
        );
        let mut normalized_after = after.clone();
        normalized_after.lifecycle.generation = before.lifecycle.generation;
        assert_eq!(&normalized_after, before);
    }

    fn test_active_capture() -> ActiveCapture {
        let prepared = prepare_legacy_config(test_legacy_fake_config()).unwrap();
        start_legacy_capture(&prepared, RuntimeFaultSink::default()).unwrap()
    }

    struct CountingRetryStarter {
        outcomes: Mutex<VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>>,
        calls: Arc<AtomicUsize>,
    }

    impl LegacyCaptureStarter for CountingRetryStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted retry capture outcome");
            outcome.map(|mut active| {
                active.health = Some(PipeWireHealth::unarmed_with_fault_sink(fault_sink));
                active
            })
        }
    }

    struct ProbeRetryStarter {
        calls: Arc<AtomicUsize>,
        constructors: Arc<AtomicUsize>,
        resources: Arc<AttemptResourceCounters>,
        pre_resolution_missing: bool,
    }

    impl LegacyCaptureStarter for ProbeRetryStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.pre_resolution_missing {
                return missing_target_attempt();
            }
            self.constructors.fetch_add(1, Ordering::SeqCst);
            let mut active = test_active_capture_with_resources(&self.resources);
            active.health = Some(PipeWireHealth::unarmed_with_fault_sink(fault_sink));
            Ok(active)
        }
    }

    struct FailFirstRetryRecovery {
        calls: AtomicUsize,
    }

    impl LegacyStartupRecovery for FailFirstRetryRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(LambError::Capture(
                    "injected post-construction recovery failure".to_string(),
                ))
            } else {
                Ok(0)
            }
        }
    }

    type RetryObservations = Arc<(Mutex<(Vec<&'static str>, usize)>, Condvar)>;

    struct RetryOperationHarness {
        _temp: tempfile::TempDir,
        ctx: Arc<IdleDaemonContext>,
        socket_path: PathBuf,
        listener_thread: Option<std::thread::JoinHandle<Result<()>>>,
        starter_calls: Arc<AtomicUsize>,
        constructor_calls: Arc<AtomicUsize>,
        resources: Arc<AttemptResourceCounters>,
        observations: RetryObservations,
        clock: Arc<DaemonManualClock>,
    }

    impl RetryOperationHarness {
        fn new(
            outcomes: VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>,
        ) -> Self {
            Self::new_with_scheduler(outcomes)
        }

        fn new_with_scheduler(
            outcomes: VecDeque<std::result::Result<ActiveCapture, CaptureAttemptError>>,
        ) -> Self {
            let calls = Arc::new(AtomicUsize::new(0));
            let starter = Arc::new(CountingRetryStarter {
                outcomes: Mutex::new(outcomes),
                calls: Arc::clone(&calls),
            });
            Self::build(
                starter,
                Arc::new(SuccessfulLegacyRecovery),
                calls,
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AttemptResourceCounters::default()),
            )
        }

        fn with_successful_probe_attempts(_attempts: usize) -> Self {
            let calls = Arc::new(AtomicUsize::new(0));
            let constructors = Arc::new(AtomicUsize::new(0));
            let resources = Arc::new(AttemptResourceCounters::default());
            let starter = Arc::new(ProbeRetryStarter {
                calls: Arc::clone(&calls),
                constructors: Arc::clone(&constructors),
                resources: Arc::clone(&resources),
                pre_resolution_missing: false,
            });
            Self::build(
                starter,
                Arc::new(SuccessfulLegacyRecovery),
                calls,
                constructors,
                resources,
            )
        }

        fn with_pre_resolution_missing_target() -> Self {
            let calls = Arc::new(AtomicUsize::new(0));
            let constructors = Arc::new(AtomicUsize::new(0));
            let resources = Arc::new(AttemptResourceCounters::default());
            let starter = Arc::new(ProbeRetryStarter {
                calls: Arc::clone(&calls),
                constructors: Arc::clone(&constructors),
                resources: Arc::clone(&resources),
                pre_resolution_missing: true,
            });
            Self::build(
                starter,
                Arc::new(SuccessfulLegacyRecovery),
                calls,
                constructors,
                resources,
            )
        }

        fn with_post_construction_recovery_failure() -> Self {
            let calls = Arc::new(AtomicUsize::new(0));
            let constructors = Arc::new(AtomicUsize::new(0));
            let resources = Arc::new(AttemptResourceCounters::default());
            let starter = Arc::new(ProbeRetryStarter {
                calls: Arc::clone(&calls),
                constructors: Arc::clone(&constructors),
                resources: Arc::clone(&resources),
                pre_resolution_missing: false,
            });
            Self::build(
                starter,
                Arc::new(FailFirstRetryRecovery {
                    calls: AtomicUsize::new(0),
                }),
                calls,
                constructors,
                resources,
            )
        }

        fn build(
            starter: Arc<dyn LegacyCaptureStarter>,
            recovery: Arc<dyn LegacyStartupRecovery>,
            starter_calls: Arc<AtomicUsize>,
            constructor_calls: Arc<AtomicUsize>,
            resources: Arc<AttemptResourceCounters>,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let config_path = temp.path().join("lamb.toml");
            let socket_path = temp.path().join("control.sock");
            let output_path = temp.path().join("output");
            fs::create_dir(&output_path).unwrap();
            let socket_toml = toml::Value::String(socket_path.to_string_lossy().into_owned());
            let output_toml = toml::Value::String(output_path.to_string_lossy().into_owned());
            fs::write(
                &config_path,
                format!(
                    r#"configVersion = 1
user = "test"
backend = "fake"
channels = 2
channelMap = ["left", "right"]
seconds = 5
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = {output_toml}
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = {socket_toml}
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
                ),
            )
            .unwrap();
            let bootstrap = bootstrap_config(&config_path).unwrap();
            let prepared = prepare_bootstrap_config(&bootstrap);
            let clock = Arc::new(DaemonManualClock::default());
            let ctx = build_idle_context_with_dependencies(
                bootstrap,
                prepared,
                starter.as_ref(),
                recovery.as_ref(),
                clock.clone(),
            )
            .unwrap();
            {
                let mut runtime = ctx.runtime.lock().unwrap();
                runtime.state = "stopped".to_string();
                runtime.lifecycle.mark_stopped(None);
                assert_eq!(runtime.lifecycle.begin_operation().unwrap(), 1);
            }
            let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
            let observations = Arc::new((Mutex::new((Vec::new(), 0)), Condvar::new()));
            let before_observations = Arc::clone(&observations);
            let after_observations = Arc::clone(&observations);
            let listener_ctx = Arc::clone(&ctx);
            let listener_thread = std::thread::spawn(move || {
                run_idle_listener_with_dependencies(
                    listener_ctx,
                    socket,
                    starter,
                    recovery,
                    || {},
                    move |job| {
                        let label = match job {
                            OperationJob::Internal(ScheduledOperation::Retry { .. }) => "retry",
                            OperationJob::Internal(ScheduledOperation::RuntimeFault { .. }) => {
                                "runtime-fault"
                            }
                            OperationJob::Client {
                                request: ControlRequest::StartCapture { .. },
                                ..
                            } => "start-capture",
                            OperationJob::Client {
                                request: ControlRequest::StopCapture,
                                ..
                            } => "stop-capture",
                            OperationJob::Client { .. } => "client",
                        };
                        before_observations.0.lock().unwrap().0.push(label);
                    },
                    move || {
                        let mut state = after_observations.0.lock().unwrap();
                        state.1 += 1;
                        after_observations.1.notify_all();
                    },
                )
            });
            let harness = Self {
                _temp: temp,
                ctx,
                socket_path,
                listener_thread: Some(listener_thread),
                starter_calls,
                constructor_calls,
                resources,
                observations,
                clock,
            };
            assert!(harness.status_via_socket().ok);
            harness.assert_listener_live();
            {
                let mut state = harness.observations.0.lock().unwrap();
                state.0.clear();
                state.1 = 0;
            }
            harness
        }

        fn enqueue_internal(&self, operation: ScheduledOperation) {
            match operation {
                ScheduledOperation::Retry { generation } => {
                    self.ctx
                        .scheduler
                        .schedule_retry(generation, self.clock_now());
                }
                ScheduledOperation::RuntimeFault {
                    generation,
                    attempt_id,
                    fault,
                } => self
                    .ctx
                    .scheduler
                    .notify_fault(generation, attempt_id, fault),
            }
            self.scheduler_barrier();
        }

        fn enqueue_request(&self, request: ControlRequest) {
            let response = self.request(request);
            assert!(response.ok, "{}", response.message);
        }

        fn request(&self, request: ControlRequest) -> ControlResponse {
            crate::control::send_request(&self.socket_path, &request).unwrap()
        }

        fn status_via_socket(&self) -> ControlResponse {
            self.request(ControlRequest::Status)
        }

        fn wait_for_operations(&self, count: usize) {
            let mut state = self.observations.0.lock().unwrap();
            let deadline = std::time::Instant::now() + ROUTE_TEST_TIMEOUT;
            while state.1 < count {
                let remaining = deadline
                    .checked_duration_since(std::time::Instant::now())
                    .expect("timed out waiting for retry operation");
                let (next, timeout) = self.observations.1.wait_timeout(state, remaining).unwrap();
                assert!(
                    !timeout.timed_out(),
                    "timed out waiting for retry operation"
                );
                state = next;
            }
        }

        fn completed_operations(&self) -> usize {
            self.observations.0.lock().unwrap().1
        }

        fn observed_order(&self) -> Vec<&'static str> {
            self.observations.0.lock().unwrap().0.clone()
        }

        fn stop_capture(&self) {
            let before = self.completed_operations();
            let response = self.request(ControlRequest::StopCapture);
            assert!(response.ok, "{}", response.message);
            self.wait_for_operations(before + 1);
            let mut state = self.observations.0.lock().unwrap();
            state.0.clear();
            state.1 = 0;
        }

        fn starter_call_count(&self) -> usize {
            self.starter_calls.load(Ordering::SeqCst)
        }

        fn constructor_count(&self) -> usize {
            self.constructor_calls.load(Ordering::SeqCst)
        }

        fn pid(&self) -> u32 {
            std::process::id()
        }

        fn socket_identity(&self) -> (u64, u64) {
            let metadata = fs::symlink_metadata(&self.socket_path).unwrap();
            (metadata.dev(), metadata.ino())
        }

        fn assert_listener_live(&self) {
            assert!(self.socket_path.exists());
            assert!(UnixStream::connect(&self.socket_path).is_ok());
            assert!(!self.listener_thread.as_ref().unwrap().is_finished());
        }

        fn attempt_count(&self) -> usize {
            self.starter_call_count()
        }

        fn resource_counts(&self) -> [usize; 5] {
            std::array::from_fn(|index| self.resources.live[index].load(Ordering::SeqCst))
        }

        fn lifecycle(&self) -> LifecycleState {
            self.ctx.runtime.lock().unwrap().lifecycle.clone()
        }

        fn attempt_initial_start(&self) {
            let before = self.completed_operations();
            let response = self.request(ControlRequest::StartCapture {
                profile: None,
                activate: false,
            });
            assert!(
                response.ok || response.status.is_some(),
                "start response must preserve control-plane status: {}",
                response.message
            );
            self.wait_for_operations(before + 1);
            let mut state = self.observations.0.lock().unwrap();
            state.0.clear();
            state.1 = 0;
        }

        fn now_millis(&self) -> u64 {
            self.clock.now_millis.load(Ordering::SeqCst)
        }

        fn clock_now(&self) -> RetryInstant {
            self.clock.now()
        }

        fn advance_to_next_retry(&self) {
            let due = self
                .lifecycle()
                .next_retry_at
                .expect("transient lifecycle has a retry deadline");
            self.clock
                .now_millis
                .store(due.as_millis(), Ordering::SeqCst);
            self.scheduler_barrier();
        }

        fn scheduler_barrier(&self) {
            self.ctx.scheduler.wake_for_test();
        }

        fn scheduler_thread_start_count(&self) -> u64 {
            self.ctx.scheduler.thread_start_count_for_test()
        }

        fn scheduler_timed_wait_count(&self) -> u64 {
            self.ctx.scheduler.timed_wait_count_for_test()
        }

        fn disconnect_published_pipewire_through_callback(&self) -> bool {
            let health = self
                .ctx
                .runtime
                .lock()
                .unwrap()
                .capture_health
                .clone()
                .expect("published capture has PipeWire health");
            crate::capture_pipewire::observe_stream_unconnected_for_test(&health)
        }
    }

    impl Drop for RetryOperationHarness {
        fn drop(&mut self) {
            if !self.ctx.stop.load(Ordering::Acquire) {
                let response =
                    crate::control::send_request(&self.socket_path, &ControlRequest::Stop)
                        .expect("production listener accepts Stop during harness shutdown");
                assert!(response.ok, "{}", response.message);
            }
            if let Some(listener) = self.listener_thread.take() {
                listener.join().unwrap().unwrap();
            }
            assert!(!self.socket_path.exists());
            assert_eq!(self.resource_counts(), [0; 5]);
        }
    }

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
        harness.scheduler_barrier();
        assert_eq!(harness.starter_call_count(), calls_before);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
    }

    #[test]
    fn integrated_scheduler_and_worker_reject_stale_fault_notification_and_publication() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        let old_generation = harness.lifecycle().generation;
        let attempt_id = CaptureAttemptId::from_raw_for_test(1);
        let sink = harness.ctx.scheduler.fault_sink(old_generation, attempt_id);
        harness.stop_capture();

        sink.notify(RuntimeCaptureFault::BackendFault(
            "stale notification".to_string(),
        ));
        harness.ctx.scheduler.wake_for_test();
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);

        harness.enqueue_internal(ScheduledOperation::RuntimeFault {
            generation: old_generation,
            attempt_id,
            fault: RuntimeCaptureFault::BackendFault("stale publication".to_string()),
        });
        harness.scheduler_barrier();
        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Stopped);
        assert_eq!(lifecycle.error_class, None);
        assert_eq!(lifecycle.retry_policy, RetryPolicy::None);
    }

    #[test]
    fn pipewire_resolution_errors_map_without_rendered_string_classification() {
        let cases = [
            (
                TargetResolutionError::TargetMissing("missing target".to_string()),
                ErrorClass::Transient,
                CaptureState::WaitingForDevice,
            ),
            (
                TargetResolutionError::PortMissing("missing port".to_string()),
                ErrorClass::Transient,
                CaptureState::WaitingForDevice,
            ),
            (
                TargetResolutionError::BackendUnavailable("daemon absent".to_string()),
                ErrorClass::Transient,
                CaptureState::Faulted,
            ),
            (
                TargetResolutionError::TargetChanged("startup race".to_string()),
                ErrorClass::Transient,
                CaptureState::WaitingForDevice,
            ),
            (
                TargetResolutionError::InvalidSelector("bad selector".to_string()),
                ErrorClass::Permanent,
                CaptureState::Faulted,
            ),
        ];

        for (error, class, capture_state) in cases {
            let classified = classify_pipewire_resolution_error(error);
            assert_eq!(classified.class, class);
            assert_eq!(classified.capture_state, capture_state);
        }
    }

    #[test]
    fn legacy_second_handshake_resolution_variants_reach_supervisor_as_waiting_transient() {
        for startup_error in [
            TargetResolutionError::TargetMissing("vanished target".to_string()),
            TargetResolutionError::PortMissing("vanished port".to_string()),
            TargetResolutionError::TargetChanged("changed identity".to_string()),
        ] {
            let harness = SupervisorHarness::stopped();
            let prepared = prepare_legacy_config(test_legacy_pipewire_config()).unwrap();
            let Err(attempt_error) = start_legacy_capture_with_pipewire_start(
                &prepared,
                RuntimeFaultSink::default(),
                |_| Ok(test_resolved_target(1, 48_000)),
                move |_, _, _, _| Err(PipeWireStartupError::Resolution(startup_error)),
            ) else {
                panic!("typed second-resolution failure unexpectedly started capture");
            };

            publish_capture_attempt_error(&harness.ctx, attempt_error);

            let lifecycle = harness.lifecycle();
            assert_eq!(lifecycle.capture_state, CaptureState::WaitingForDevice);
            assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
            assert_eq!(lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        }

        let harness = SupervisorHarness::stopped();
        let prepared = prepare_legacy_config(test_legacy_pipewire_config()).unwrap();
        let Err(attempt_error) = start_legacy_capture_with_pipewire_start(
            &prepared,
            RuntimeFaultSink::default(),
            |_| Ok(test_resolved_target(1, 48_000)),
            |_, _, _, _| {
                Err(PipeWireStartupError::Capture(LambError::Capture(
                    "stream open failed".to_string(),
                )))
            },
        ) else {
            panic!("generic PipeWire startup failure unexpectedly started capture");
        };
        publish_capture_attempt_error(&harness.ctx, attempt_error);
        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Faulted);
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
    }

    #[test]
    fn pipewire_disconnect_enqueues_recovery_without_status_request() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        let resources = Arc::new(AttemptResourceCounters::default());
        {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            let mut active = test_active_capture_with_resources(&resources);
            runtime.capture = active.backend.take();
            runtime.session = active.session.take();
            runtime.resolved_capture = Some(active.resolved.clone());
            runtime.state = "capturing".to_string();
            runtime
                .lifecycle
                .mark_running(None, active.resolved.resolved_target.clone());
        }
        let generation = harness.lifecycle().generation;
        let attempt_id = CaptureAttemptId::from_raw_for_test(2);
        let health = PipeWireHealth::with_fault_sink(
            harness.ctx.scheduler.fault_sink(generation, attempt_id),
        );
        harness.ctx.runtime.lock().unwrap().capture_health = Some(health.clone());

        assert!(health.record_fatal(RuntimeCaptureFault::DeviceDisconnected(
            "selected PipeWire device disconnected".to_string(),
        )));
        harness.ctx.scheduler.wake_for_test();
        harness.wait_for_operations(1);

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.generation, generation);
        assert_eq!(lifecycle.capture_state, CaptureState::WaitingForDevice);
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(lifecycle.retry_attempt, 1);
        assert_eq!(
            lifecycle.next_retry_at,
            Some(RetryInstant::from_millis(1_000))
        );
        assert_eq!(harness.starter_call_count(), 0);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        assert!(runtime.resolved_capture.is_none());
    }

    #[test]
    fn old_generation_pipewire_fault_is_ignored() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        let old_generation = harness.lifecycle().generation;
        let health = PipeWireHealth::with_fault_sink(
            harness
                .ctx
                .scheduler
                .fault_sink(old_generation, CaptureAttemptId::from_raw_for_test(3)),
        );
        harness.stop_capture();

        assert!(health.record_fatal(RuntimeCaptureFault::BackendFault(
            "old backend failed".to_string(),
        )));
        harness.ctx.scheduler.wake_for_test();

        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Stopped);
        assert_eq!(lifecycle.error_class, None);
        assert_eq!(lifecycle.retry_attempt, 0);
    }

    #[test]
    fn duplicate_pipewire_fault_does_not_double_advance_backoff() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            let mut active = test_active_capture();
            runtime.capture = active.backend.take();
            runtime.session = active.session.take();
            runtime.resolved_capture = Some(active.resolved.clone());
            runtime.state = "capturing".to_string();
            runtime
                .lifecycle
                .mark_running(None, active.resolved.resolved_target.clone());
        }
        let generation = harness.lifecycle().generation;
        let attempt_id = CaptureAttemptId::from_raw_for_test(4);
        let health = PipeWireHealth::with_fault_sink(
            harness.ctx.scheduler.fault_sink(generation, attempt_id),
        );
        harness.ctx.runtime.lock().unwrap().capture_health = Some(health.clone());

        assert!(health.record_fatal(RuntimeCaptureFault::BackendFault("first fault".to_string(),)));
        assert!(!health.record_fatal(RuntimeCaptureFault::BackendFault(
            "duplicate fault".to_string(),
        )));
        harness.ctx.scheduler.wake_for_test();
        harness.wait_for_operations(1);

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Faulted);
        assert_eq!(lifecycle.retry_attempt, 1);
        assert_eq!(
            lifecycle.next_retry_at,
            Some(RetryInstant::from_millis(1_000))
        );
    }

    #[test]
    fn same_generation_fault_from_different_attempt_cannot_detach_active_capture() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        let resources = Arc::new(AttemptResourceCounters::default());
        let generation = harness.lifecycle().generation;
        let stale_attempt = CaptureAttemptId::from_raw_for_test(40);
        let active_attempt = CaptureAttemptId::from_raw_for_test(41);
        let health = PipeWireHealth::with_fault_sink(
            harness.ctx.scheduler.fault_sink(generation, active_attempt),
        );
        {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            let mut active = test_active_capture_with_resources(&resources);
            runtime.capture = active.backend.take();
            runtime.session = active.session.take();
            runtime.capture_health = Some(health);
            runtime.resolved_capture = Some(active.resolved.clone());
            runtime.state = "capturing".to_string();
            runtime
                .lifecycle
                .mark_running(None, active.resolved.resolved_target.clone());
        }

        publish_runtime_fault(
            &harness.ctx,
            generation,
            stale_attempt,
            RuntimeCaptureFault::BackendFault("stale attempt".to_string()),
        );
        assert_eq!(resources.live(ProbeResource::Backend), 1);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);

        publish_runtime_fault(
            &harness.ctx,
            generation,
            active_attempt,
            RuntimeCaptureFault::BackendFault("active attempt".to_string()),
        );
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(harness.lifecycle().retry_attempt, 1);
    }

    #[test]
    fn duplicate_already_admitted_fault_jobs_advance_backoff_once() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::new());
        let generation = harness.lifecycle().generation;
        let attempt_id = CaptureAttemptId::from_raw_for_test(42);
        let health = PipeWireHealth::with_fault_sink(
            harness.ctx.scheduler.fault_sink(generation, attempt_id),
        );
        {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            let mut active = test_active_capture();
            runtime.capture = active.backend.take();
            runtime.session = active.session.take();
            runtime.capture_health = Some(health);
            runtime.resolved_capture = Some(active.resolved.clone());
            runtime.state = "capturing".to_string();
            runtime
                .lifecycle
                .mark_running(None, active.resolved.resolved_target.clone());
        }
        let fault = RuntimeCaptureFault::BackendFault("duplicate admitted fault".to_string());

        publish_runtime_fault(&harness.ctx, generation, attempt_id, fault.clone());
        publish_runtime_fault(&harness.ctx, generation, attempt_id, fault);

        assert_eq!(harness.lifecycle().retry_attempt, 1);
    }

    #[test]
    fn scheduler_lane_worker_fifo_enforces_attempt_identity_and_duplicate_noop() {
        let harness = SupervisorHarness::running();
        let generation = harness.lifecycle().generation;
        let matching_attempt = CaptureAttemptId::from_raw_for_test(43);
        let mismatched_attempt = CaptureAttemptId::from_raw_for_test(44);
        let health = PipeWireHealth::unarmed_with_fault_sink(
            harness
                .ctx
                .scheduler
                .fault_sink(generation, matching_attempt),
        );
        health.arm();
        harness.ctx.runtime.lock().unwrap().capture_health = Some(health);

        let lane = Arc::new(OperationLane::new(4).unwrap());
        let admitted = Arc::new((Mutex::new(0_usize), Condvar::new()));
        let scheduler =
            spawn_retry_scheduler(harness.ctx.scheduler.clone(), harness.ctx.clock.clone(), {
                let lane = Arc::clone(&lane);
                let admitted = Arc::clone(&admitted);
                move |operation| {
                    lane.try_enqueue(OperationJob::Internal(operation))
                        .map_err(|(error, _)| error)?;
                    let mut count = admitted.0.lock().unwrap();
                    *count += 1;
                    admitted.1.notify_all();
                    Ok(())
                }
            })
            .unwrap();
        let admit = |attempt_id, message: &str, expected_count| {
            harness.ctx.scheduler.notify_fault(
                generation,
                attempt_id,
                RuntimeCaptureFault::BackendFault(message.to_string()),
            );
            let mut count = admitted.0.lock().unwrap();
            while *count < expected_count {
                count = admitted.1.wait(count).unwrap();
            }
        };
        admit(mismatched_attempt, "mismatched", 1);
        admit(matching_attempt, "matching", 2);
        admit(matching_attempt, "duplicate matching", 3);

        let observed = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let starter = Arc::new(PanickingLegacyStarter {
            calls: AtomicUsize::new(0),
        });
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let ctx = Arc::clone(&harness.ctx);
                let starter = Arc::clone(&starter);
                let resources = Arc::clone(&harness.resources);
                let observed = Arc::clone(&observed);
                move |job| {
                    execute_operation_job_with_recovery(
                        &ctx,
                        job,
                        starter.as_ref(),
                        &SuccessfulLegacyRecovery,
                    );
                    let lifecycle = ctx.runtime.lock().unwrap().lifecycle.clone();
                    observed.0.lock().unwrap().push((
                        lifecycle.capture_state,
                        lifecycle.retry_attempt,
                        resources.live(ProbeResource::Backend),
                    ));
                    observed.1.notify_all();
                }
            },
            |_| {},
        );
        let mut states = observed.0.lock().unwrap();
        while states.len() < 3 {
            states = observed.1.wait(states).unwrap();
        }
        assert_eq!(states[0], (CaptureState::Running, 0, 1));
        assert_eq!(states[1], (CaptureState::Faulted, 1, 0));
        assert_eq!(states[2], (CaptureState::Faulted, 1, 0));
        drop(states);

        harness.ctx.scheduler.stop();
        scheduler.join().unwrap();
        lane.close();
        worker.join().unwrap();
    }

    struct CapturingFaultSinkStarter {
        active: Mutex<Option<ActiveCapture>>,
        sink: Mutex<Option<RuntimeFaultSink>>,
    }

    struct UnarmedFaultLegacyStarter {
        health: Arc<Mutex<Option<PipeWireHealth>>>,
    }

    impl LegacyCaptureStarter for UnarmedFaultLegacyStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            let health = PipeWireHealth::unarmed_with_fault_sink(fault_sink);
            health.record_fatal(RuntimeCaptureFault::BackendFault(
                "fault recorded before publication".to_string(),
            ));
            *self.health.lock().unwrap() = Some(health.clone());
            let mut active = test_active_capture();
            active.health = Some(health);
            Ok(active)
        }
    }

    #[test]
    fn configured_initial_legacy_publication_invalidates_then_replays_unarmed_fault() {
        let harness = RetryOperationHarness::new(VecDeque::new());
        let health = Arc::new(Mutex::new(None));
        let starter = UnarmedFaultLegacyStarter {
            health: Arc::clone(&health),
        };

        let _ = attempt_configured_start_with_recovery(
            &harness.ctx,
            &starter,
            &SuccessfulLegacyRecovery,
        );

        harness.wait_for_operations(1);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Faulted);
        assert_eq!(harness.lifecycle().retry_attempt, 1);

        let published_health = health.lock().unwrap().clone().unwrap();
        let pending_before_repeat = harness.ctx.scheduler.pending_operation_for_test();
        published_health.arm();
        let generation = harness.lifecycle().generation;
        let attempt_id = published_health.attempt_id().unwrap();
        published_health.rebind_and_arm(harness.ctx.scheduler.fault_sink(generation, attempt_id));
        assert_eq!(
            harness.ctx.scheduler.pending_operation_for_test(),
            pending_before_repeat
        );
    }

    #[test]
    fn legacy_command_publication_invalidates_then_replays_unarmed_fault() {
        let harness = RetryOperationHarness::new(VecDeque::new());
        let health = Arc::new(Mutex::new(None));
        let starter = UnarmedFaultLegacyStarter {
            health: Arc::clone(&health),
        };

        let response = start_capture_transaction_with_recovery(
            &harness.ctx,
            None,
            false,
            &starter,
            &SuccessfulLegacyRecovery,
        );

        assert!(response.ok, "{}", response.message);
        assert!(health.lock().unwrap().is_some());

        harness.wait_for_operations(1);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Faulted);
        assert_eq!(harness.lifecycle().retry_attempt, 1);
    }

    impl LegacyCaptureStarter for CapturingFaultSinkStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            *self.sink.lock().unwrap() = Some(fault_sink);
            Ok(self.active.lock().unwrap().take().unwrap())
        }
    }

    #[test]
    fn legacy_start_receives_current_generation_fault_sink() {
        let harness = SupervisorHarness::stopped();
        let starter = CapturingFaultSinkStarter {
            active: Mutex::new(Some(test_active_capture())),
            sink: Mutex::new(None),
        };

        let response = start_capture_transaction_with_recovery(
            &harness.ctx,
            None,
            false,
            &starter,
            &SuccessfulLegacyRecovery,
        );
        assert!(response.ok, "{}", response.message);
        let generation = harness.lifecycle().generation;
        let sink = starter.sink.lock().unwrap().take().unwrap();
        let attempt_id = sink.attempt_id();
        sink.notify(RuntimeCaptureFault::BackendFault(
            "generation-bound fault".to_string(),
        ));

        assert_eq!(
            harness.ctx.scheduler.pending_operation_for_test(),
            Some(ScheduledOperation::RuntimeFault {
                generation,
                attempt_id,
                fault: RuntimeCaptureFault::BackendFault("generation-bound fault".to_string()),
            })
        );
    }

    fn missing_target_attempt() -> std::result::Result<ActiveCapture, CaptureAttemptError> {
        Err(CaptureAttemptError {
            class: ErrorClass::Transient,
            capture_state: CaptureState::WaitingForDevice,
            message: "selected PipeWire target is missing".to_string(),
        })
    }

    #[test]
    fn missing_target_enters_waiting_for_device() {
        let harness =
            RetryOperationHarness::new_with_scheduler(VecDeque::from([missing_target_attempt()]));
        harness.attempt_initial_start();

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.daemon_state, DaemonState::Degraded);
        assert_eq!(lifecycle.capture_state, CaptureState::WaitingForDevice);
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(lifecycle.retry_attempt, 1);
        assert_eq!(
            lifecycle.next_retry_at,
            Some(
                harness
                    .clock_now()
                    .checked_add(Duration::from_secs(1))
                    .unwrap()
            )
        );
    }

    #[test]
    fn transient_deadlines_follow_one_two_five_ten_thirty_sixty() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            missing_target_attempt(),
            missing_target_attempt(),
            missing_target_attempt(),
            missing_target_attempt(),
            missing_target_attempt(),
            missing_target_attempt(),
            Ok(test_active_capture()),
        ]));
        harness.attempt_initial_start();
        let pid = harness.pid();
        let socket = harness.socket_identity();

        for (index, expected_seconds) in [1_u64, 2, 5, 10, 30, 60].into_iter().enumerate() {
            let lifecycle = harness.lifecycle();
            assert_eq!(lifecycle.retry_attempt, (index + 1) as u32);
            assert_eq!(
                lifecycle.next_retry_at,
                Some(
                    harness
                        .clock_now()
                        .checked_add(Duration::from_secs(expected_seconds))
                        .unwrap()
                )
            );
            assert_eq!(harness.resource_counts(), [0; 5]);
            harness.advance_to_next_retry();
            harness.wait_for_operations(index + 1);
            assert_eq!(harness.pid(), pid);
            assert_eq!(harness.socket_identity(), socket);
            assert!(harness.status_via_socket().ok);
            harness.assert_listener_live();
            assert_eq!(harness.resource_counts(), [0; 5]);
        }
        assert_eq!(harness.attempt_count(), 7);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
        assert_eq!(harness.pid(), pid);
        assert_eq!(harness.socket_identity(), socket);
        assert!(harness.status_via_socket().ok);
        harness.assert_listener_live();
        assert_eq!(harness.resource_counts(), [0; 5]);
    }

    #[test]
    fn transient_retries_keep_process_and_socket_identity() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            missing_target_attempt(),
            missing_target_attempt(),
            Ok(test_active_capture()),
        ]));
        let pid = harness.pid();
        let socket = harness.socket_identity();
        harness.attempt_initial_start();
        harness.assert_listener_live();

        for completed in 1..=2 {
            harness.advance_to_next_retry();
            harness.wait_for_operations(completed);
            assert_eq!(harness.pid(), pid);
            assert_eq!(harness.socket_identity(), socket);
            harness.assert_listener_live();
            assert!(harness.status_via_socket().ok);
        }
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
    }

    #[test]
    fn failed_attempts_release_all_resource_counters() {
        let harness = RetryOperationHarness::with_post_construction_recovery_failure();
        harness.attempt_initial_start();
        assert_eq!(harness.resource_counts(), [0; 5]);
        harness.advance_to_next_retry();
        harness.wait_for_operations(1);
        assert_eq!(harness.resource_counts(), [1; 5]);
        harness.stop_capture();
        assert_eq!(harness.resource_counts(), [0; 5]);
    }

    #[test]
    fn missing_target_pre_resolution_constructs_no_attempt_resources() {
        let harness = RetryOperationHarness::with_pre_resolution_missing_target();
        harness.attempt_initial_start();

        assert_eq!(harness.constructor_count(), 0);
        assert_eq!(harness.resource_counts(), [0; 5]);
        assert_eq!(
            harness.lifecycle().capture_state,
            CaptureState::WaitingForDevice
        );
    }

    #[test]
    fn successful_retry_resets_attempt_and_deadline() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            missing_target_attempt(),
            Ok(test_active_capture()),
        ]));
        harness.attempt_initial_start();
        harness.advance_to_next_retry();
        harness.wait_for_operations(1);

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.daemon_state, DaemonState::Ready);
        assert_eq!(lifecycle.capture_state, CaptureState::Running);
        assert_eq!(lifecycle.error_class, None);
        assert_eq!(lifecycle.last_error, None);
        assert_eq!(lifecycle.retry_policy, RetryPolicy::None);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
    }

    #[test]
    fn runtime_disconnect_retries_without_status_poll() {
        let harness = RetryOperationHarness::with_successful_probe_attempts(2);
        harness.attempt_initial_start();
        assert_eq!(harness.resource_counts(), [1; 5]);

        assert!(harness.disconnect_published_pipewire_through_callback());
        harness.wait_for_operations(1);
        assert_eq!(
            harness.lifecycle().capture_state,
            CaptureState::WaitingForDevice
        );
        assert_eq!(harness.resource_counts(), [0; 5]);

        harness.advance_to_next_retry();
        harness.wait_for_operations(2);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
        assert_eq!(harness.attempt_count(), 2);
    }

    #[test]
    fn healthy_capture_has_no_scheduler_wakeups_or_extra_attempts() {
        let harness = RetryOperationHarness::with_successful_probe_attempts(1);
        harness.attempt_initial_start();
        let pid = harness.pid();
        let socket = harness.socket_identity();
        let attempts = harness.attempt_count();
        let timed_waits = harness.scheduler_timed_wait_count();
        let thread_starts = harness.scheduler_thread_start_count();
        let resources = harness.resource_counts();

        harness.clock.now_millis.fetch_add(
            Duration::from_secs(600).as_millis() as u64,
            Ordering::SeqCst,
        );
        harness.scheduler_barrier();

        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
        assert_eq!(harness.attempt_count(), attempts);
        assert_eq!(harness.scheduler_timed_wait_count(), timed_waits);
        assert_eq!(harness.scheduler_thread_start_count(), thread_starts);
        assert_eq!(harness.resource_counts(), resources);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert_eq!(harness.pid(), pid);
        assert_eq!(harness.socket_identity(), socket);
        harness.assert_listener_live();
        assert!(harness.status_via_socket().ok);
    }

    #[test]
    fn production_scheduler_executes_exact_capped_backoff_once_per_failed_attempt() {
        let transient = || {
            Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "device unavailable".to_string(),
            })
        };
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            transient(),
            transient(),
            transient(),
            transient(),
            transient(),
            transient(),
            transient(),
            transient(),
        ]));

        harness.attempt_initial_start();
        let expected = [1_u64, 2, 5, 10, 30, 60, 60, 60];
        for (index, seconds) in expected.into_iter().enumerate() {
            let lifecycle = harness.lifecycle();
            assert_eq!(lifecycle.retry_attempt, u32::try_from(index + 1).unwrap());
            assert_eq!(
                lifecycle.next_retry_at.unwrap().as_millis() - harness.now_millis(),
                seconds * 1_000
            );
            if index + 1 < expected.len() {
                harness.advance_to_next_retry();
                harness.wait_for_operations(index + 1);
            }
        }
        assert_eq!(harness.starter_call_count(), expected.len());
        assert_eq!(harness.scheduler_thread_start_count(), 1);
    }

    #[test]
    fn production_scheduler_success_cancels_and_resets_retry_state() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "device unavailable".to_string(),
            }),
            Ok(test_active_capture()),
        ]));
        harness.attempt_initial_start();
        harness.advance_to_next_retry();
        harness.wait_for_operations(1);

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Running);
        assert_eq!(lifecycle.error_class, None);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
    }

    #[test]
    fn production_scheduler_permanent_reclassification_cancels_retry() {
        let harness = RetryOperationHarness::new_with_scheduler(VecDeque::from([
            Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "device unavailable".to_string(),
            }),
            Err(CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message: "profile invalid".to_string(),
            }),
        ]));
        harness.attempt_initial_start();
        harness.advance_to_next_retry();
        harness.wait_for_operations(1);

        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.capture_state, CaptureState::Faulted);
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Permanent));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::Manual);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
    }

    #[test]
    fn production_lane_full_retry_is_retained_admitted_once_and_stop_discards_it() {
        fn filler() -> OperationJob {
            OperationJob::Internal(ScheduledOperation::RuntimeFault {
                generation: 1,
                attempt_id: CaptureAttemptId::from_raw_for_test(5),
                fault: RuntimeCaptureFault::BackendFault("filler".to_string()),
            })
        }

        let clock = Arc::new(DaemonManualClock::default());
        let handle = RetrySchedulerHandle::new();
        let lane = Arc::new(OperationLane::new(1).unwrap());
        lane.try_enqueue(filler()).unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker = spawn_retry_scheduler(handle.clone(), clock.clone(), {
            let lane = Arc::clone(&lane);
            let attempts = Arc::clone(&attempts);
            move |operation| {
                attempts.fetch_add(1, Ordering::SeqCst);
                lane.try_enqueue(OperationJob::Internal(operation))
                    .map_err(|(error, _)| error)
            }
        })
        .unwrap();
        handle.schedule_retry(1, RetryInstant::from_millis(1_000));
        clock.now_millis.store(1_000, Ordering::SeqCst);
        handle.wake_for_test();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            handle.pending_operation_for_test(),
            Some(ScheduledOperation::Retry { generation: 1 })
        );

        handle.notify_lane_available();
        handle.wake_for_test();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let _occupied = lane.pop().unwrap();
        handle.notify_lane_available();
        handle.wake_for_test();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert!(matches!(
            lane.pop(),
            Some(OperationJob::Internal(ScheduledOperation::Retry {
                generation: 1
            }))
        ));
        handle.wake_for_test();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(handle.pending_operation_for_test(), None);
        handle.stop();
        worker.join().unwrap();

        let stopped_clock = Arc::new(DaemonManualClock::default());
        let stopped_handle = RetrySchedulerHandle::new();
        let stopped_lane = Arc::new(OperationLane::new(1).unwrap());
        stopped_lane.try_enqueue(filler()).unwrap();
        let stopped_attempts = Arc::new(AtomicUsize::new(0));
        let stopped_worker =
            spawn_retry_scheduler(stopped_handle.clone(), stopped_clock.clone(), {
                let lane = Arc::clone(&stopped_lane);
                let attempts = Arc::clone(&stopped_attempts);
                move |operation| {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    lane.try_enqueue(OperationJob::Internal(operation))
                        .map_err(|(error, _)| error)
                }
            })
            .unwrap();
        stopped_handle.schedule_retry(2, RetryInstant::from_millis(1_000));
        stopped_clock.now_millis.store(1_000, Ordering::SeqCst);
        stopped_handle.wake_for_test();
        assert_eq!(stopped_attempts.load(Ordering::SeqCst), 1);
        stopped_handle.stop();
        let _occupied = stopped_lane.pop().unwrap();
        stopped_handle.notify_lane_available();
        stopped_worker.join().unwrap();
        assert_eq!(stopped_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(stopped_handle.pending_operation_for_test(), None);
    }

    fn test_active_capture_with_resources(
        counters: &Arc<AttemptResourceCounters>,
    ) -> ActiveCapture {
        let mut session = test_session();
        session.attempt_resource_probes = vec![
            Box::new(counters.lease(ProbeResource::Session)),
            Box::new(counters.lease(ProbeResource::Arena)),
            Box::new(counters.lease(ProbeResource::Workspace)),
            Box::new(counters.lease(ProbeResource::Descriptor)),
        ];
        ActiveCapture {
            backend: Some(CaptureBackend::TestProbe(Box::new(
                counters.lease(ProbeResource::Backend),
            ))),
            session: Some(Arc::new(session)),
            health: None,
            resolved: ResolvedCaptureState {
                channel_count: 1,
                sample_rate: 100,
                resolved_target: Some("probe".to_string()),
            },
            backend_resource_probe: None,
        }
    }

    fn test_app_candidate(
        running: bool,
        counters: Option<&Arc<AttemptResourceCounters>>,
    ) -> AppRuntimeState {
        let mut candidate = empty_runtime(ConfigFamily::App);
        candidate.config.daemon.start_mode = if running { "auto" } else { "manual" }.to_string();
        if running {
            let counters = counters.expect("running test app candidate counters");
            let mut session = test_session();
            session.attempt_resource_probes = vec![
                Box::new(counters.lease(ProbeResource::Session)),
                Box::new(counters.lease(ProbeResource::Arena)),
                Box::new(counters.lease(ProbeResource::Workspace)),
                Box::new(counters.lease(ProbeResource::Descriptor)),
            ];
            candidate.capture = Some(CaptureBackend::TestProbe(Box::new(
                counters.lease(ProbeResource::Backend),
            )));
            candidate.session = Some(Arc::new(session));
            candidate.state = "capturing".to_string();
            candidate
                .lifecycle
                .mark_running(Some("studio".to_string()), Some("test-app".to_string()));
        } else {
            candidate.state = "idle".to_string();
            candidate.lifecycle.mark_stopped(Some("studio".to_string()));
        }
        candidate
    }

    fn production_path_app_loaded(
        ctx: &IdleDaemonContext,
        output_root: &Path,
        start_mode: &str,
    ) -> app_config::LoadedAppConfig {
        let mut config = app_config::AppConfig::default();
        config.daemon.start_mode = start_mode.to_string();
        config.daemon.active_profile = Some("studio".to_string());
        config.daemon.control_socket_path = ctx.control_socket_path.display().to_string();
        config.profiles.insert(
            "studio".to_string(),
            app_config::ProfileConfig {
                backend: Some("jack".to_string()),
                client_name: Some("lamb-test".to_string()),
                capture: app_config::CaptureConfig {
                    ports: vec![app_config::CapturePort {
                        source: Some("system:capture_1".to_string()),
                        name: Some("mic".to_string()),
                        export_mode: None,
                    }],
                    sources: Vec::new(),
                },
                buffer: app_config::BufferConfig { seconds: Some(1) },
                export: app_config::ProfileExportConfig {
                    output_dir: Some(output_root.to_path_buf()),
                    mode: Some("per-channel".to_string()),
                    format: Some("wav".to_string()),
                    ..app_config::ProfileExportConfig::default()
                },
                ..app_config::ProfileConfig::default()
            },
        );
        app_config::LoadedAppConfig {
            config,
            state: ConfigLoadState::Loaded,
            error: None,
        }
    }

    #[test]
    fn initial_app_transient_failure_schedules_and_retries_through_the_production_path() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config.daemon.start_mode = "auto".to_string();
        config.daemon.active_profile = Some("studio".to_string());
        config.daemon.control_socket_path = socket_path.display().to_string();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let starter = TransientAppStarter {
            calls: AtomicUsize::new(0),
        };
        let clock = Arc::new(DaemonManualClock::default());

        let ctx = build_idle_context_with_dependencies(
            bootstrap,
            prepared,
            &starter,
            &SuccessfulLegacyRecovery,
            clock.clone(),
        )
        .unwrap();

        let initial = ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(initial.daemon_state, DaemonState::Degraded);
        assert_eq!(initial.capture_state, CaptureState::WaitingForDevice);
        assert_eq!(initial.error_class, Some(ErrorClass::Transient));
        assert_eq!(initial.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(initial.retry_attempt, 1);
        assert_eq!(
            initial.next_retry_at,
            Some(RetryInstant::from_millis(1_000))
        );
        assert_eq!(initial.generation, 1);
        assert!(ctx.runtime.lock().unwrap().capture.is_none());
        assert!(ctx.runtime.lock().unwrap().session.is_none());
        assert_eq!(
            ctx.scheduler.pending_operation_for_test(),
            Some(ScheduledOperation::Retry { generation: 1 })
        );

        clock.now_millis.store(1_000, Ordering::SeqCst);
        execute_retry_operation(&ctx, 1, &starter, &SuccessfulLegacyRecovery);
        let retried = ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(starter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(retried.retry_attempt, 2);
        assert_eq!(
            retried.next_retry_at,
            Some(RetryInstant::from_millis(3_000))
        );
        assert_eq!(retried.generation, 2);
    }

    #[test]
    fn production_scheduler_retries_app_construction_recovery_and_publication() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config.daemon.start_mode = "auto".to_string();
        config.daemon.active_profile = Some("studio".to_string());
        config.daemon.control_socket_path = socket_path.display().to_string();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(TransientThenSuccessfulAppStarter {
            calls: AtomicUsize::new(0),
            resources: Arc::clone(&resources),
        });
        let recovery = Arc::new(CountingSuccessfulRecovery {
            calls: AtomicUsize::new(0),
        });
        let clock = Arc::new(DaemonManualClock::default());
        let ctx = build_idle_context_with_dependencies(
            bootstrap,
            prepared,
            starter.as_ref(),
            recovery.as_ref(),
            clock.clone(),
        )
        .unwrap();
        let lane = Arc::new(OperationLane::new(1).unwrap());
        let scheduler = spawn_retry_scheduler(ctx.scheduler.clone(), clock.clone(), {
            let lane = Arc::clone(&lane);
            move |operation| {
                lane.try_enqueue(OperationJob::Internal(operation))
                    .map_err(|(error, _)| error)
            }
        })
        .unwrap();
        let completed = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let ctx = Arc::clone(&ctx);
                let starter = Arc::clone(&starter);
                let recovery = Arc::clone(&recovery);
                let completed = Arc::clone(&completed);
                move |job| {
                    ctx.scheduler.notify_lane_available();
                    execute_operation_job_with_recovery(
                        &ctx,
                        job,
                        starter.as_ref(),
                        recovery.as_ref(),
                    );
                    *completed.0.lock().unwrap() = true;
                    completed.1.notify_all();
                }
            },
            |_| {},
        );

        clock.now_millis.store(1_000, Ordering::SeqCst);
        ctx.scheduler.wake_for_test();
        let mut done = completed.0.lock().unwrap();
        while !*done {
            done = completed.1.wait(done).unwrap();
        }
        drop(done);

        let lifecycle = ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(starter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
        assert_eq!(lifecycle.capture_state, CaptureState::Running);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
        assert!(ctx.runtime.lock().unwrap().capture.is_some());
        assert!(ctx.runtime.lock().unwrap().session.is_some());

        ctx.scheduler.stop();
        scheduler.join().unwrap();
        lane.close();
        worker.join().unwrap();
        release_capture_for_shutdown(&ctx);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
    }

    fn poison_runtime(ctx: &Arc<IdleDaemonContext>, message: &'static str) {
        let poisoned = Arc::clone(ctx);
        let _ = std::thread::spawn(move || {
            let _runtime = poisoned.runtime.lock().unwrap();
            panic!("{message}");
        })
        .join();
        assert!(ctx.runtime.is_poisoned());
    }

    #[test]
    fn coherent_poisoned_retry_executes_once_and_publishes_success() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config.daemon.start_mode = "auto".to_string();
        config.daemon.active_profile = Some("studio".to_string());
        config.daemon.control_socket_path = socket_path.display().to_string();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(TransientThenSuccessfulAppStarter {
            calls: AtomicUsize::new(0),
            resources: Arc::clone(&resources),
        });
        let recovery = Arc::new(CountingSuccessfulRecovery {
            calls: AtomicUsize::new(0),
        });
        let clock = Arc::new(DaemonManualClock::default());
        let ctx = build_idle_context_with_dependencies(
            bootstrap,
            prepared,
            starter.as_ref(),
            recovery.as_ref(),
            clock.clone(),
        )
        .unwrap();
        poison_runtime(&ctx, "coherent retry poison");
        let lane = Arc::new(OperationLane::new(1).unwrap());
        let scheduler = spawn_retry_scheduler(ctx.scheduler.clone(), clock.clone(), {
            let lane = Arc::clone(&lane);
            move |operation| {
                lane.try_enqueue(OperationJob::Internal(operation))
                    .map_err(|(error, _)| error)
            }
        })
        .unwrap();
        let completed = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let ctx = Arc::clone(&ctx);
                let starter = Arc::clone(&starter);
                let recovery = Arc::clone(&recovery);
                let completed = Arc::clone(&completed);
                move |job| {
                    ctx.scheduler.notify_lane_available();
                    execute_operation_job_with_recovery(
                        &ctx,
                        job,
                        starter.as_ref(),
                        recovery.as_ref(),
                    );
                    *completed.0.lock().unwrap() = true;
                    completed.1.notify_all();
                }
            },
            |_| {},
        );

        clock.now_millis.store(1_000, Ordering::SeqCst);
        ctx.scheduler.wake_for_test();
        let mut done = completed.0.lock().unwrap();
        while !*done {
            done = completed.1.wait(done).unwrap();
        }
        drop(done);

        let lifecycle = ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(starter.calls.load(Ordering::SeqCst), 2);
        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
        assert_eq!(lifecycle.capture_state, CaptureState::Running);
        assert_eq!(lifecycle.error_class, None);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
        assert_eq!(ctx.scheduler.pending_operation_for_test(), None);
        assert!(ctx.runtime.lock().is_ok());
        ctx.scheduler.wake_for_test();
        assert_eq!(starter.calls.load(Ordering::SeqCst), 2);

        ctx.scheduler.stop();
        scheduler.join().unwrap();
        lane.close();
        worker.join().unwrap();
        release_capture_for_shutdown(&ctx);
        let dropped = resources.dropped();
        assert_eq!(
            &dropped[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );
    }

    #[test]
    fn poisoned_retry_with_active_ownership_terminalizes_and_cleans_backend_first() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config.daemon.start_mode = "auto".to_string();
        config.daemon.active_profile = Some("studio".to_string());
        config.daemon.control_socket_path = socket_path.display().to_string();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(TransientThenSuccessfulAppStarter {
            calls: AtomicUsize::new(0),
            resources: Arc::clone(&resources),
        });
        let clock = Arc::new(DaemonManualClock::default());
        let ctx = build_idle_context_with_dependencies(
            bootstrap,
            prepared,
            starter.as_ref(),
            &SuccessfulLegacyRecovery,
            clock.clone(),
        )
        .unwrap();
        let old_generation = ctx.runtime.lock().unwrap().lifecycle.generation;
        let old_attempt_id = CaptureAttemptId::from_raw_for_test(6);
        let old_fault_sink = ctx.scheduler.fault_sink(old_generation, old_attempt_id);
        let poisoned = Arc::clone(&ctx);
        let mut stale = test_active_capture_with_resources(&resources);
        let _ = std::thread::spawn(move || {
            let mut runtime = poisoned.runtime.lock().unwrap();
            runtime.capture = stale.backend.take();
            runtime.session = stale.session.take();
            panic!("retry poison with stale ownership");
        })
        .join();
        let lane = Arc::new(OperationLane::new(1).unwrap());
        let scheduler = spawn_retry_scheduler(ctx.scheduler.clone(), clock.clone(), {
            let lane = Arc::clone(&lane);
            move |operation| {
                lane.try_enqueue(OperationJob::Internal(operation))
                    .map_err(|(error, _)| error)
            }
        })
        .unwrap();
        let completed = Arc::new((Mutex::new(false), Condvar::new()));
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let ctx = Arc::clone(&ctx);
                let starter = Arc::clone(&starter);
                let completed = Arc::clone(&completed);
                move |job| {
                    ctx.scheduler.notify_lane_available();
                    execute_operation_job_with_recovery(
                        &ctx,
                        job,
                        starter.as_ref(),
                        &SuccessfulLegacyRecovery,
                    );
                    *completed.0.lock().unwrap() = true;
                    completed.1.notify_all();
                }
            },
            |_| {},
        );

        clock.now_millis.store(1_000, Ordering::SeqCst);
        ctx.scheduler.wake_for_test();
        let mut done = completed.0.lock().unwrap();
        while !*done {
            done = completed.1.wait(done).unwrap();
        }
        drop(done);

        let runtime = ctx.runtime.lock().unwrap();
        assert_eq!(starter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.lifecycle.generation, old_generation);
        assert_eq!(runtime.lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(runtime.lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(runtime.lifecycle.retry_attempt, 1);
        assert!(runtime.lifecycle.next_retry_at.is_some());
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        let terminal_lifecycle = runtime.lifecycle.clone();
        drop(runtime);
        assert!(ctx.stop.load(Ordering::Acquire));
        assert_eq!(
            ctx.first_fatal.get().map(String::as_str),
            Some("runtime state lock poison violated scheduled retry invariants")
        );
        assert_eq!(ctx.scheduler.pending_operation_for_test(), None);
        let dropped = resources.dropped();
        assert_eq!(
            &dropped[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );

        old_fault_sink.notify(RuntimeCaptureFault::BackendFault(
            "compromised sink".to_string(),
        ));
        ctx.scheduler.schedule_retry(
            old_generation,
            RetryInstant::from_millis(clock.now_millis.load(Ordering::SeqCst) + 1),
        );
        ctx.scheduler.notify_fault(
            old_generation,
            old_attempt_id,
            RuntimeCaptureFault::DeviceDisconnected("compromised fault".to_string()),
        );
        execute_operation_job_with_recovery(
            &ctx,
            OperationJob::Internal(ScheduledOperation::Retry {
                generation: old_generation,
            }),
            starter.as_ref(),
            &SuccessfulLegacyRecovery,
        );
        execute_operation_job_with_recovery(
            &ctx,
            OperationJob::Internal(ScheduledOperation::RuntimeFault {
                generation: old_generation,
                attempt_id: old_attempt_id,
                fault: RuntimeCaptureFault::BackendFault("queued old fault".to_string()),
            }),
            starter.as_ref(),
            &SuccessfulLegacyRecovery,
        );
        ctx.scheduler.wake_for_test();

        assert_eq!(ctx.scheduler.pending_operation_for_test(), None);
        assert_eq!(ctx.runtime.lock().unwrap().lifecycle, terminal_lifecycle);
        assert_eq!(starter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);

        ctx.scheduler.stop();
        scheduler.join().unwrap();
        lane.close();
        worker.join().unwrap();
    }

    #[test]
    fn incoherent_poison_generation_overflow_stops_and_rejects_equal_generation_work() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let mut stale = test_active_capture_with_resources(&resources);
        let poisoned = Arc::clone(&harness.ctx);
        let _ = std::thread::spawn(move || {
            let mut runtime = poisoned.runtime.lock().unwrap();
            runtime.lifecycle.generation = u64::MAX;
            runtime.lifecycle.mark_transient(
                CaptureState::WaitingForDevice,
                "overflow retry".to_string(),
                RetryInstant::from_millis(0),
            );
            runtime.capture = stale.backend.take();
            runtime.session = stale.session.take();
            panic!("overflow retry poison with stale ownership");
        })
        .join();
        harness.ctx.scheduler.invalidate(u64::MAX);
        let old_attempt_id = CaptureAttemptId::from_raw_for_test(7);
        let old_fault_sink = harness.ctx.scheduler.fault_sink(u64::MAX, old_attempt_id);

        assert_eq!(
            scheduled_generation_decision(&harness.ctx, u64::MAX),
            ScheduledGenerationDecision::Terminalized
        );
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.lifecycle.generation, u64::MAX);
        assert_eq!(runtime.lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(runtime.lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(runtime.lifecycle.retry_attempt, 1);
        assert!(runtime.lifecycle.next_retry_at.is_some());
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        let terminal_lifecycle = runtime.lifecycle.clone();
        drop(runtime);
        assert!(harness.stop_flag());
        assert_eq!(
            harness.ctx.first_fatal.get().map(String::as_str),
            Some("runtime state lock poison violated scheduled retry invariants")
        );
        let dropped = resources.dropped();
        assert_eq!(
            &dropped[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );

        old_fault_sink.notify(RuntimeCaptureFault::BackendFault(
            "overflow sink".to_string(),
        ));
        harness
            .ctx
            .scheduler
            .schedule_retry(u64::MAX, RetryInstant::from_millis(1));
        harness.ctx.scheduler.notify_fault(
            u64::MAX,
            old_attempt_id,
            RuntimeCaptureFault::DeviceDisconnected("overflow fault".to_string()),
        );
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);

        let starter = CountingRetryStarter {
            outcomes: Mutex::new(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "must not run".to_string(),
            })])),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        execute_operation_job_with_recovery(
            &harness.ctx,
            OperationJob::Internal(ScheduledOperation::Retry {
                generation: u64::MAX,
            }),
            &starter,
            &SuccessfulLegacyRecovery,
        );
        execute_operation_job_with_recovery(
            &harness.ctx,
            OperationJob::Internal(ScheduledOperation::RuntimeFault {
                generation: u64::MAX,
                attempt_id: old_attempt_id,
                fault: RuntimeCaptureFault::BackendFault("queued overflow fault".to_string()),
            }),
            &starter,
            &SuccessfulLegacyRecovery,
        );

        assert_eq!(starter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            harness.ctx.runtime.lock().unwrap().lifecycle,
            terminal_lifecycle
        );
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
    }

    #[test]
    fn publish_capture_attempt_error_recovers_poison_and_retains_retry_authority() {
        let harness = SupervisorHarness::stopped();
        poison_runtime(&harness.ctx, "publish capture error poison");

        publish_capture_attempt_error(
            &harness.ctx,
            CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "retry publication".to_string(),
            },
        );

        let lifecycle = harness.ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(lifecycle.retry_attempt, 1);
        assert!(lifecycle.next_retry_at.is_some());
        assert_eq!(
            harness.ctx.scheduler.pending_operation_for_test(),
            lifecycle
                .next_retry_at
                .map(|_due| ScheduledOperation::Retry {
                    generation: lifecycle.generation,
                })
        );
        assert!(harness.ctx.runtime.lock().is_ok());
    }

    #[test]
    fn command_attempt_failure_recovers_poison_and_retains_retry_authority() {
        let harness = SupervisorHarness::stopped();
        let operation =
            begin_new_operation_generation(&harness.ctx, OperationEntry::Command).unwrap();
        let resources = Arc::new(AttemptResourceCounters::default());
        let mut stale = test_active_capture_with_resources(&resources);
        let poisoned = Arc::clone(&harness.ctx);
        let _ = std::thread::spawn(move || {
            let mut runtime = poisoned.runtime.lock().unwrap();
            runtime.capture = stale.backend.take();
            runtime.session = stale.session.take();
            panic!("command failure poison");
        })
        .join();

        let response = command_attempt_failure_with_generation(
            &harness.ctx,
            operation.token,
            Some(operation.generation),
            CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "retry command failure".to_string(),
            },
            false,
        );

        assert!(!response.ok);
        let lifecycle = harness.ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(lifecycle.generation, operation.generation);
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Transient));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(lifecycle.retry_attempt, 1);
        assert!(lifecycle.next_retry_at.is_some());
        assert!(harness.ctx.scheduler.pending_operation_for_test().is_some());
        assert!(harness.ctx.runtime.lock().is_ok());
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        drop(runtime);
        let dropped = resources.dropped();
        assert_eq!(
            &dropped[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );
    }

    #[test]
    fn permanent_capture_attempt_error_recovers_poison_without_retry_authority() {
        let harness = SupervisorHarness::stopped();
        poison_runtime(&harness.ctx, "permanent publication poison");

        publish_capture_attempt_error(
            &harness.ctx,
            CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message: "permanent publication".to_string(),
            },
        );

        let lifecycle = harness.ctx.runtime.lock().unwrap().lifecycle.clone();
        assert_eq!(lifecycle.error_class, Some(ErrorClass::Permanent));
        assert_eq!(lifecycle.retry_policy, RetryPolicy::Manual);
        assert_eq!(lifecycle.retry_attempt, 0);
        assert_eq!(lifecycle.next_retry_at, None);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert!(harness.ctx.runtime.lock().is_ok());
    }

    #[test]
    fn successful_retry_publication_recovers_poison_and_publishes_once() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(ScriptedLegacyCaptureStarter {
            outcomes: Mutex::new(VecDeque::from([Ok(test_active_capture_with_resources(
                &resources,
            ))])),
        });
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::LegacySuccess);
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let command = std::thread::spawn(move || {
            reload_daemon_config_with_recovery(
                &ctx,
                None,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        poison_runtime(&harness.ctx, "success publication poison");
        release.wait();

        let response = command.join().unwrap();
        assert!(response.ok);
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.lifecycle.capture_state, CaptureState::Running);
        assert_eq!(runtime.lifecycle.error_class, None);
        assert_eq!(runtime.lifecycle.retry_attempt, 0);
        assert_eq!(runtime.lifecycle.next_retry_at, None);
        assert!(runtime.capture.is_some());
        assert!(runtime.session.is_some());
        drop(runtime);
        assert!(harness.ctx.runtime.lock().is_ok());
        release_capture_for_shutdown(&harness.ctx);
        let dropped = resources.dropped();
        assert_eq!(
            &dropped[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );
    }

    fn install_final_mutation_pause(
        ctx: &IdleDaemonContext,
        kind: FinalMutationKind,
    ) -> (Arc<std::sync::Barrier>, Arc<std::sync::Barrier>) {
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *ctx.final_mutation_pause.lock().unwrap() = Some(FinalMutationPause {
            kind,
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        (entered, release)
    }

    fn spawn_observed_shutdown(
        ctx: Arc<IdleDaemonContext>,
    ) -> (std::sync::mpsc::Receiver<()>, std::thread::JoinHandle<()>) {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            begin_shutdown(&ctx);
            finished_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        (finished_rx, worker)
    }

    fn stop_was_blocked_before_release(
        ctx: &IdleDaemonContext,
        finished: &std::sync::mpsc::Receiver<()>,
    ) -> bool {
        finished
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err()
            && !ctx.stop.load(Ordering::SeqCst)
    }

    #[test]
    fn invalid_reload_while_running_preserves_active_session_and_lifecycle() {
        let harness = SupervisorHarness::running();
        let before = harness.snapshot();
        let response = harness.reload_with_text("not valid toml");

        assert!(!response.ok);
        assert_eq!(
            response.error_context.error_class,
            Some(ErrorClass::Permanent)
        );
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
    }

    #[test]
    fn running_reload_start_failure_preserves_complete_authority() {
        let harness = SupervisorHarness::running();
        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "replacement unavailable".to_string(),
            }));
        let before = harness.snapshot();
        let response =
            harness.reload_with_text(&toml::to_string(&test_legacy_fake_config()).unwrap());

        assert!(!response.ok);
        assert_eq!(
            response.error_context.error_class,
            Some(ErrorClass::Transient)
        );
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
    }

    #[test]
    fn running_reload_recovery_failure_preserves_complete_authority() {
        let harness = SupervisorHarness::running();
        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(test_active_capture()));
        let before = harness.snapshot();
        let response = reload_daemon_config_with_recovery(
            &harness.ctx,
            None,
            &harness.starter,
            &ScriptedLegacyRecovery(Err(LambError::Control(
                "replacement recovery failed".to_string(),
            ))),
        );

        assert!(!response.ok);
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
    }

    #[test]
    fn direct_stop_invalidates_reload_before_disk_preparation() {
        let harness = SupervisorHarness::running();
        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(test_active_capture()));
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let ctx = Arc::clone(&harness.ctx);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let outcome = harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap();
        let worker = std::thread::spawn(move || {
            let scripted = ScriptedLegacyCaptureStarter {
                outcomes: Mutex::new(VecDeque::from([outcome])),
            };
            reload_daemon_config_with_recovery_and_entry_hook(
                &ctx,
                None,
                &scripted,
                &SuccessfulLegacyRecovery,
                || {
                    worker_entered.wait();
                    worker_release.wait();
                },
            )
        });

        entered.wait();
        begin_shutdown(&harness.ctx);
        release.wait();
        let response = worker.join().unwrap();

        assert!(!response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
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
    fn changed_socket_reload_preserves_running_authority_and_faults_stopped() {
        let mut changed = test_legacy_fake_config();
        changed.control_socket_path = PathBuf::from("/tmp/a-different-lamb.sock");
        let text = toml::to_string(&changed).unwrap();

        let running = SupervisorHarness::running();
        let before = running.snapshot();
        let running_response = running.reload_with_text(&text);
        assert!(!running_response.ok);
        assert_eq!(
            running_response.error_context.error_class,
            Some(ErrorClass::Permanent)
        );
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &running.snapshot(),
        );

        let stopped = SupervisorHarness::stopped();
        let stopped_response = stopped.reload_with_text(&text);
        assert!(!stopped_response.ok);
        assert_eq!(stopped.lifecycle().daemon_state, DaemonState::Degraded);
        assert_eq!(stopped.lifecycle().retry_policy, RetryPolicy::Manual);
    }

    #[test]
    fn same_socket_allows_app_to_legacy_and_legacy_to_app_switches() {
        let harness = SupervisorHarness::running();
        let mut app = app_config::AppConfig::default();
        app.daemon.control_socket_path = harness.ctx.control_socket_path.display().to_string();
        let to_app = harness.reload_with_text(&toml::to_string(&app).unwrap());
        assert!(to_app.ok, "{}", to_app.message);
        {
            let runtime = harness.ctx.runtime.lock().unwrap();
            assert_eq!(runtime.config_family, ConfigFamily::App);
            assert_eq!(runtime.lifecycle.capture_state, CaptureState::Stopped);
        }

        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(test_active_capture()));
        let to_legacy =
            harness.reload_with_text(&toml::to_string(&test_legacy_fake_config()).unwrap());
        assert!(to_legacy.ok, "{}", to_legacy.message);
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.config_family, ConfigFamily::Legacy);
        assert_eq!(runtime.lifecycle.capture_state, CaptureState::Running);
    }

    #[test]
    fn app_manual_and_auto_reload_publish_expected_lifecycle() {
        let manual = SupervisorHarness::stopped();
        let manual_token = begin_command_operation(&manual.ctx).unwrap();
        let manual_response = install_prepared_then_follow_start_mode(
            &manual.ctx,
            manual_token,
            PreparedBootstrap::TestAppCandidate {
                candidate: test_app_candidate(false, None),
                entered: None,
                release: None,
            },
            &manual.starter,
            &SuccessfulLegacyRecovery,
            false,
            false,
            false,
        );
        assert!(manual_response.ok);
        assert_eq!(manual.lifecycle().daemon_state, DaemonState::Ready);
        assert_eq!(manual.lifecycle().capture_state, CaptureState::Stopped);

        let auto = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let auto_token = begin_command_operation(&auto.ctx).unwrap();
        let auto_response = install_prepared_then_follow_start_mode(
            &auto.ctx,
            auto_token,
            PreparedBootstrap::TestAppCandidate {
                candidate: test_app_candidate(true, Some(&resources)),
                entered: None,
                release: None,
            },
            &auto.starter,
            &SuccessfulLegacyRecovery,
            false,
            false,
            false,
        );
        assert!(auto_response.ok);
        assert_eq!(auto.lifecycle().daemon_state, DaemonState::Ready);
        assert_eq!(auto.lifecycle().capture_state, CaptureState::Running);
    }

    #[test]
    fn production_path_app_auto_candidate_prepares_recovers_and_publishes_running() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = ProductionPathAppStarter {
            resources: Arc::clone(&resources),
        };
        let loaded = production_path_app_loaded(
            &harness.ctx,
            &harness._temp.path().join("app-output"),
            "auto",
        );
        let token = begin_command_operation(&harness.ctx).unwrap();

        let response = install_prepared_then_follow_start_mode(
            &harness.ctx,
            token,
            PreparedBootstrap::App(loaded),
            &starter,
            &SuccessfulLegacyRecovery,
            false,
            false,
            false,
        );

        assert!(response.ok, "{}", response.message);
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.config_family, ConfigFamily::App);
        assert_eq!(runtime.lifecycle.daemon_state, DaemonState::Ready);
        assert_eq!(runtime.lifecycle.capture_state, CaptureState::Running);
        assert!(runtime.capture.is_some());
        assert!(runtime.session.is_some());
        assert!(runtime.session.as_ref().unwrap().policy.lock().is_ok());
        drop(runtime);
        assert_eq!(resources.live(ProbeResource::Backend), 1);
        release_capture_for_shutdown(&harness.ctx);
    }

    #[test]
    fn app_command_publication_invalidates_then_replays_unarmed_fault() {
        struct UnarmedFaultAppStarter {
            resources: Arc<AttemptResourceCounters>,
            health: Arc<Mutex<Option<PipeWireHealth>>>,
        }

        impl LegacyCaptureStarter for UnarmedFaultAppStarter {
            fn start(
                &self,
                _prepared: &PreparedLegacyConfig,
                _fault_sink: RuntimeFaultSink,
            ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
                unreachable!("app starter does not start legacy capture")
            }

            fn prepare_app(
                &self,
                profile: &profile::ResolvedProfile,
                params: CaptureRuntimeParams,
                fault_sink: RuntimeFaultSink,
            ) -> std::result::Result<Option<PreparedAppCapture>, CaptureAttemptError> {
                let health = PipeWireHealth::unarmed_with_fault_sink(fault_sink);
                health.record_fatal(RuntimeCaptureFault::BackendFault(
                    "app fault recorded before publication".to_string(),
                ));
                *self.health.lock().unwrap() = Some(health.clone());
                let (runtime, _ingress) =
                    CaptureRuntime::build(params, 100, u32::try_from(profile.ports.len()).unwrap())
                        .map_err(classify_legacy_attempt_error)?;
                Ok(Some(PreparedAppCapture {
                    backend: CaptureBackend::TestAppProbe {
                        resource: Box::new(self.resources.lease(ProbeResource::Backend)),
                        sample_rate: 100,
                    },
                    runtime,
                    health: Some(health),
                    resolved_live_inputs: jack_live_identities(profile)
                        .map_err(classify_legacy_attempt_error)?,
                    session_resource_probes: vec![],
                }))
            }
        }

        let harness = RetryOperationHarness::new(VecDeque::new());
        let resources = Arc::new(AttemptResourceCounters::default());
        let health = Arc::new(Mutex::new(None));
        let starter = UnarmedFaultAppStarter {
            resources,
            health: Arc::clone(&health),
        };
        let loaded = production_path_app_loaded(
            &harness.ctx,
            &harness._temp.path().join("app-replay-output"),
            "auto",
        );
        let token = begin_command_operation(&harness.ctx).unwrap();

        let response = install_prepared_then_follow_start_mode(
            &harness.ctx,
            token,
            PreparedBootstrap::App(loaded),
            &starter,
            &SuccessfulLegacyRecovery,
            false,
            false,
            false,
        );

        assert!(response.ok, "{}", response.message);
        assert!(health.lock().unwrap().is_some());

        harness.wait_for_operations(1);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Faulted);
        assert_eq!(harness.lifecycle().retry_attempt, 1);
    }

    #[test]
    fn production_path_app_recovery_failure_rejects_before_publication_backend_first() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = ProductionPathAppStarter {
            resources: Arc::clone(&resources),
        };
        let loaded = production_path_app_loaded(
            &harness.ctx,
            &harness._temp.path().join("app-output"),
            "auto",
        );
        let token = begin_command_operation(&harness.ctx).unwrap();

        let response = install_prepared_then_follow_start_mode(
            &harness.ctx,
            token,
            PreparedBootstrap::App(loaded),
            &starter,
            &NonzeroAppRecovery,
            false,
            false,
            false,
        );

        assert!(!response.ok);
        assert!(response.message.contains("app startup recovery failed"));
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        drop(runtime);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
        assert_eq!(resources.dropped()[0], ProbeResource::Backend);
        assert_eq!(resources.dropped()[1], ProbeResource::Session);
    }

    #[test]
    fn stale_app_candidate_after_direct_stop_does_not_deadlock_or_publish() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let token = begin_command_operation(&harness.ctx).unwrap();
        let ctx = Arc::clone(&harness.ctx);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let candidate = test_app_candidate(true, Some(&resources));
        let worker = std::thread::spawn(move || {
            install_prepared_then_follow_start_mode(
                &ctx,
                token,
                PreparedBootstrap::TestAppCandidate {
                    candidate,
                    entered: Some(worker_entered),
                    release: Some(worker_release),
                },
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
                false,
                false,
                false,
            )
        });

        entered.wait();
        begin_shutdown(&harness.ctx);
        release.wait();
        let response = worker.join().unwrap();

        assert!(!response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        assert_eq!(
            resources.dropped(),
            vec![
                ProbeResource::Backend,
                ProbeResource::Session,
                ProbeResource::Arena,
                ProbeResource::Workspace,
                ProbeResource::Descriptor,
            ]
        );
        assert!(harness.ctx.runtime.lock().unwrap().session.is_none());
    }

    #[test]
    fn direct_stop_after_start_persistence_keeps_durable_selection_but_not_runtime_candidate() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let persisted = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let hook_persisted = Arc::clone(&persisted);
        let hook_release = Arc::clone(&release);
        *AFTER_START_CONFIG_PERSIST_HOOK.lock().unwrap() = Some(Arc::new(move || {
            hook_persisted.wait();
            hook_release.wait();
        }));
        let mut candidate = test_app_candidate(true, Some(&resources));
        candidate.config.daemon.active_profile = Some("studio".to_string());
        candidate.config.daemon.control_socket_path =
            harness.ctx.control_socket_path.display().to_string();
        let token = begin_command_operation(&harness.ctx).unwrap();
        let ctx = Arc::clone(&harness.ctx);
        let worker = std::thread::spawn(move || {
            install_prepared_then_follow_start_mode(
                &ctx,
                token,
                PreparedBootstrap::TestAppCandidate {
                    candidate,
                    entered: None,
                    release: None,
                },
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
                true,
                true,
                true,
            )
        });

        persisted.wait();
        begin_shutdown(&harness.ctx);
        release.wait();
        let response = worker.join().unwrap();

        assert!(!response.ok);
        let loaded = app_config::load_optional_config(&harness.config_path).unwrap();
        assert_eq!(
            loaded.config.daemon.active_profile.as_deref(),
            Some("studio")
        );
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert!(runtime.session.is_none());
        assert_eq!(runtime.lifecycle.daemon_state, DaemonState::Stopping);
        drop(runtime);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
    }

    #[test]
    fn admitted_active_profile_save_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::PersistenceAdmission);
        let mut candidate = test_app_candidate(true, Some(&resources));
        candidate.config.daemon.active_profile = Some("studio".to_string());
        candidate.config.daemon.control_socket_path =
            harness.ctx.control_socket_path.display().to_string();
        let token = begin_command_operation(&harness.ctx).unwrap();
        let ctx = Arc::clone(&harness.ctx);
        let command = std::thread::spawn(move || {
            install_prepared_then_follow_start_mode(
                &ctx,
                token,
                PreparedBootstrap::TestAppCandidate {
                    candidate,
                    entered: None,
                    release: None,
                },
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
                true,
                true,
                true,
            )
        });

        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let _response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(blocked, "Stop must wait for admitted atomic config save");
        let loaded = app_config::load_optional_config(&harness.config_path).unwrap();
        assert_eq!(
            loaded.config.daemon.active_profile.as_deref(),
            Some("studio")
        );
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        release_capture_for_shutdown(&harness.ctx);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
    }

    #[test]
    fn direct_stop_before_active_profile_save_admission_leaves_disk_unchanged() {
        let harness = SupervisorHarness::stopped();
        let before = fs::read(&harness.config_path).unwrap();
        let resources = Arc::new(AttemptResourceCounters::default());
        let candidate_entered = Arc::new(std::sync::Barrier::new(2));
        let candidate_release = Arc::new(std::sync::Barrier::new(2));
        let mut candidate = test_app_candidate(true, Some(&resources));
        candidate.config.daemon.active_profile = Some("studio".to_string());
        candidate.config.daemon.control_socket_path =
            harness.ctx.control_socket_path.display().to_string();
        let token = begin_command_operation(&harness.ctx).unwrap();
        let ctx = Arc::clone(&harness.ctx);
        let entered = Arc::clone(&candidate_entered);
        let release = Arc::clone(&candidate_release);
        let command = std::thread::spawn(move || {
            install_prepared_then_follow_start_mode(
                &ctx,
                token,
                PreparedBootstrap::TestAppCandidate {
                    candidate,
                    entered: Some(entered),
                    release: Some(release),
                },
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
                true,
                true,
                true,
            )
        });

        candidate_entered.wait();
        begin_shutdown(&harness.ctx);
        candidate_release.wait();
        let response = command.join().unwrap();

        assert!(!response.ok);
        assert_eq!(fs::read(&harness.config_path).unwrap(), before);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
    }

    #[test]
    fn blocked_backend_stop_acknowledges_then_joins_and_cleans_socket_after_release() {
        let harness = SupervisorHarness::stopped();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture_with_resources(&resources))),
        });
        let socket_path = harness._temp.path().join("blocked-stop.sock");
        let mut socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let lane = Arc::new(OperationLane::new(2).unwrap());
        let worker_ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            move |job: OperationJob| {
                let OperationJob::Client { request, stream } = job else {
                    return;
                };
                let response = match request {
                    ControlRequest::Reload => reload_daemon_config_with_recovery(
                        &worker_ctx,
                        None,
                        worker_starter.as_ref(),
                        &SuccessfulLegacyRecovery,
                    ),
                    _ => unreachable!(),
                };
                let _ = write_response(stream, &response);
            },
            |job: OperationJob| {
                if let OperationJob::Client { stream, .. } = job {
                    drop(stream);
                }
            },
        );
        let reload_peer = route_test_request(&harness.ctx, &lane, ControlRequest::Reload);
        entered.wait();

        let stop_started = std::time::Instant::now();
        let stop = read_test_response(route_test_request(
            &harness.ctx,
            &lane,
            ControlRequest::Stop,
        ));
        assert!(stop.ok);
        assert!(stop_started.elapsed() < ROUTE_TEST_TIMEOUT);
        assert!(socket_path.exists());
        release.wait();

        let reload = read_test_response(reload_peer);
        assert!(!reload.ok);
        lane.close();
        join_worker_bounded(worker);
        release_capture_for_shutdown(&harness.ctx);
        socket.cleanup().unwrap();
        assert!(!socket_path.exists());
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
        assert_eq!(resources.dropped()[0], ProbeResource::Backend);
        assert_eq!(resources.dropped()[1], ProbeResource::Session);
    }

    #[test]
    fn direct_stop_route_wakes_scheduler_without_closing_operation_lane() {
        let harness = SupervisorHarness::running();
        let lane = OperationLane::new(1).unwrap();
        let generation = harness.lifecycle().generation;

        let response = read_test_response(route_test_request(
            &harness.ctx,
            &lane,
            ControlRequest::Stop,
        ));

        assert!(response.ok);
        assert!(!lane.is_closed());
        assert_eq!(harness.lifecycle().generation, generation + 1);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        lane.close();
        release_capture_for_shutdown(&harness.ctx);
    }

    #[test]
    fn stop_cancels_occupied_and_queued_mutations_before_scheduler_first_teardown() {
        let temp = tempfile::tempdir().unwrap();
        let export_root = temp.path().join("exports");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config.daemon.active_profile = Some("studio".to_string());
        let profile = profile::resolve_active_profile(&config).unwrap().unwrap();
        let mut context = test_app_context_with_policy(
            temp.path(),
            test_policy(export_root.clone()),
            &[0.5, -0.25],
        );
        profile::save_config(&context.config_path, &config).unwrap();
        {
            let runtime = context.runtime.get_mut().unwrap();
            runtime.config = config;
            runtime.active_profile = Some(profile);
            let session = Arc::get_mut(runtime.session.as_mut().unwrap()).unwrap();
            session.profile_name = "studio".to_string();
            runtime
                .lifecycle
                .mark_running(Some("studio".to_string()), Some("studio".to_string()));
        }
        let session = context.runtime.get_mut().unwrap().session.clone().unwrap();
        let capture_before = session
            .arena
            .status(std::time::Duration::from_secs(1))
            .unwrap();
        let ctx = Arc::new(context);
        let config_before = fs::read(&ctx.config_path).unwrap();
        let runtime_config_before = ctx.runtime.lock().unwrap().config.clone();
        let socket_path = temp.path().join("shutdown-drain.sock");
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let lane = Arc::new(OperationLane::new(4).unwrap());
        let scheduler = spawn_retry_scheduler(ctx.scheduler.clone(), Arc::clone(&ctx.clock), {
            let lane = Arc::clone(&lane);
            move |operation| {
                lane.try_enqueue(OperationJob::Internal(operation))
                    .map_err(|(error, _)| error)
            }
        })
        .unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let ctx = Arc::clone(&ctx);
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                let first = AtomicBool::new(true);
                move |job| {
                    ctx.scheduler.notify_lane_available();
                    if first.swap(false, Ordering::SeqCst) {
                        entered.wait();
                        release.wait();
                    }
                    execute_operation_job_with_recovery(
                        &ctx,
                        job,
                        &RealLegacyCaptureStarter,
                        &RealLegacyStartupRecovery,
                    );
                }
            },
            {
                let ctx = Arc::clone(&ctx);
                move |job| cancel_operation_job(&ctx, job)
            },
        );

        let recall = route_test_request(&ctx, &lane, ControlRequest::Recall);
        entered.wait();
        let clear = route_test_request(&ctx, &lane, ControlRequest::Clear);
        let threshold = route_test_request(
            &ctx,
            &lane,
            ControlRequest::Threshold {
                request: ThresholdRequest::Set {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                    dbfs: -18.0,
                },
            },
        );
        assert!(handle_idle_request(&ctx, ControlRequest::Stop).ok);

        let order = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let finish_order = Arc::clone(&order);
        let finish_ctx = Arc::clone(&ctx);
        let finish_lane = Arc::clone(&lane);
        let finish = std::thread::spawn(move || {
            finish_idle_listener_with_probe(
                finish_ctx,
                socket,
                finish_lane,
                scheduler,
                worker,
                move |step| {
                    finish_order.0.lock().unwrap().push(step);
                    finish_order.1.notify_all();
                },
            )
        });
        let mut observed = order.0.lock().unwrap();
        while !observed.contains(&"lane-closed") {
            observed = order.1.wait(observed).unwrap();
        }
        drop(observed);
        release.wait();
        finish.join().unwrap().unwrap();

        for response in [
            read_test_response(recall),
            read_test_response(clear),
            read_test_response(threshold),
        ] {
            assert!(!response.ok);
            assert_eq!(response.message, "shutting down");
        }
        assert_eq!(fs::read(&ctx.config_path).unwrap(), config_before);
        assert_eq!(ctx.runtime.lock().unwrap().config, runtime_config_before);
        let capture_after = session
            .arena
            .status(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            capture_after.retained_frames,
            capture_before.retained_frames
        );
        assert_eq!(
            capture_after.active_absolute_range,
            capture_before.active_absolute_range
        );
        assert_eq!(fs::read_dir(&export_root).unwrap().count(), 0);
        assert_eq!(
            *order.0.lock().unwrap(),
            vec![
                "scheduler-joined",
                "lane-closed",
                "worker-joined",
                "capture-released",
                "socket-cleaned",
            ]
        );
        assert!(!socket_path.exists());
    }

    #[test]
    fn scheduler_panic_still_cancels_clients_releases_capture_and_cleans_socket_in_order() {
        let harness = SupervisorHarness::running();
        let socket_path = harness._temp.path().join("scheduler-panic.sock");
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let lane = Arc::new(OperationLane::new(2).unwrap());
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            DEFAULT_WORKER_STACK_BYTES as usize,
            {
                let entered = Arc::clone(&entered);
                let release = Arc::clone(&release);
                move |_job: OperationJob| {
                    entered.wait();
                    release.wait();
                }
            },
            {
                let ctx = Arc::clone(&harness.ctx);
                move |job| cancel_operation_job(&ctx, job)
            },
        );
        lane.try_enqueue(OperationJob::Internal(ScheduledOperation::Retry {
            generation: 0,
        }))
        .unwrap();
        entered.wait();
        let (stream, peer) = UnixStream::pair().unwrap();
        lane.try_enqueue(OperationJob::Client {
            request: ControlRequest::Reload,
            stream,
        })
        .unwrap();
        let scheduler = std::thread::spawn(|| panic!("injected scheduler panic"));
        let order = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let finish_order = Arc::clone(&order);
        let finish_ctx = Arc::clone(&harness.ctx);
        let finish_lane = Arc::clone(&lane);
        let finish = std::thread::spawn(move || {
            finish_idle_listener_with_probe(
                finish_ctx,
                socket,
                finish_lane,
                scheduler,
                worker,
                move |step| {
                    finish_order.0.lock().unwrap().push(step);
                    finish_order.1.notify_all();
                },
            )
        });
        let mut observed = order.0.lock().unwrap();
        while !observed.contains(&"lane-closed") {
            observed = order.1.wait(observed).unwrap();
        }
        drop(observed);
        release.wait();
        let error = finish.join().unwrap().unwrap_err();

        assert!(error.to_string().contains("retry scheduler panicked"));
        assert_eq!(read_test_response(peer).message, "shutting down");
        assert_eq!(
            *order.0.lock().unwrap(),
            vec![
                "scheduler-joined",
                "lane-closed",
                "worker-joined",
                "capture-released",
                "socket-cleaned",
            ]
        );
        assert!(!socket_path.exists());
        assert_eq!(harness.live_capture_resources(), 0);
        assert_eq!(harness.resources.dropped()[0], ProbeResource::Backend);
        assert_eq!(harness.resources.dropped()[1], ProbeResource::Session);
    }

    #[test]
    fn operation_worker_panic_still_cancels_queued_client_and_cleans_resources() {
        let harness = SupervisorHarness::running();
        let socket_path = harness._temp.path().join("worker-panic.sock");
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let lane = Arc::new(OperationLane::new(1).unwrap());
        let (stream, peer) = UnixStream::pair().unwrap();
        lane.try_enqueue(OperationJob::Client {
            request: ControlRequest::Reload,
            stream,
        })
        .unwrap();
        let scheduler = spawn_retry_scheduler(
            harness.ctx.scheduler.clone(),
            Arc::clone(&harness.ctx.clock),
            |_| Ok(()),
        )
        .unwrap();
        let worker = std::thread::spawn(|| panic!("injected operation worker panic"));

        let error = finish_idle_listener_with_probe(
            Arc::clone(&harness.ctx),
            socket,
            Arc::clone(&lane),
            scheduler,
            worker,
            |_| {},
        )
        .unwrap_err();

        assert!(error.to_string().contains("operation worker panicked"));
        assert_eq!(read_test_response(peer).message, "shutting down");
        assert!(!socket_path.exists());
        assert_eq!(harness.live_capture_resources(), 0);
        assert_eq!(harness.resources.dropped()[0], ProbeResource::Backend);
        assert_eq!(harness.resources.dropped()[1], ProbeResource::Session);
    }

    #[test]
    fn explicit_start_while_running_is_noop_without_reading_disk() {
        let harness = SupervisorHarness::running();
        let before = harness.snapshot();
        fs::write(&harness.config_path, "not valid toml").unwrap();

        let response = harness.start_capture();

        assert!(response.ok);
        assert_eq!(response.message, "capture already running");
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
    }

    #[test]
    fn running_start_advances_generation_at_entry_and_rejects_old_fault_and_retry() {
        let harness = SupervisorHarness::running();
        let old_generation = harness.lifecycle().generation;
        let old_attempt_id = CaptureAttemptId::from_raw_for_test(8);
        let old_sink = harness
            .ctx
            .scheduler
            .fault_sink(old_generation, old_attempt_id);
        let health = PipeWireHealth::with_fault_sink(old_sink.clone());
        harness.ctx.runtime.lock().unwrap().capture_health = Some(health.clone());
        let before = harness.snapshot();
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::RunningStartNoop);
        let ctx = Arc::clone(&harness.ctx);
        let worker = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                &PanickingLegacyStarter {
                    calls: AtomicUsize::new(0),
                },
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        assert_eq!(harness.lifecycle().generation, old_generation + 1);
        release.wait();
        assert!(worker.join().unwrap().ok);

        old_sink.notify(RuntimeCaptureFault::BackendFault("old sink".to_string()));
        publish_runtime_fault(
            &harness.ctx,
            old_generation,
            old_attempt_id,
            RuntimeCaptureFault::BackendFault("old publication".to_string()),
        );
        let stale_starter = PanickingLegacyStarter {
            calls: AtomicUsize::new(0),
        };
        execute_retry_operation(
            &harness.ctx,
            old_generation,
            &stale_starter,
            &SuccessfulLegacyRecovery,
        );
        assert_eq!(stale_starter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
        assert!(health.record_fatal(RuntimeCaptureFault::BackendFault(
            "fault after no-op start".to_string()
        )));
        assert_eq!(
            harness.ctx.scheduler.pending_operation_for_test(),
            Some(ScheduledOperation::RuntimeFault {
                generation: old_generation + 1,
                attempt_id: old_attempt_id,
                fault: RuntimeCaptureFault::BackendFault("fault after no-op start".to_string()),
            })
        );
    }

    #[test]
    fn preserve_running_reload_advances_at_entry_and_rejects_old_fault_and_retry() {
        let harness = SupervisorHarness::running();
        let old_generation = harness.lifecycle().generation;
        let old_attempt_id = CaptureAttemptId::from_raw_for_test(9);
        let old_sink = harness
            .ctx
            .scheduler
            .fault_sink(old_generation, old_attempt_id);
        let health = PipeWireHealth::with_fault_sink(old_sink.clone());
        harness.ctx.runtime.lock().unwrap().capture_health = Some(health.clone());
        let before = harness.snapshot();
        fs::write(&harness.config_path, "not valid toml").unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let ctx = Arc::clone(&harness.ctx);
        let worker_entered = Arc::clone(&entered);
        let worker_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            reload_daemon_config_with_recovery_and_entry_hook(
                &ctx,
                None,
                &PanickingLegacyStarter {
                    calls: AtomicUsize::new(0),
                },
                &SuccessfulLegacyRecovery,
                || {
                    worker_entered.wait();
                    worker_release.wait();
                },
            )
        });
        entered.wait();
        assert_eq!(harness.lifecycle().generation, old_generation + 1);
        release.wait();
        assert!(!worker.join().unwrap().ok);

        old_sink.notify(RuntimeCaptureFault::DeviceDisconnected(
            "old sink".to_string(),
        ));
        let stale_starter = PanickingLegacyStarter {
            calls: AtomicUsize::new(0),
        };
        execute_retry_operation(
            &harness.ctx,
            old_generation,
            &stale_starter,
            &SuccessfulLegacyRecovery,
        );
        assert_eq!(stale_starter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
        assert!(health.record_fatal(RuntimeCaptureFault::DeviceDisconnected(
            "fault after failed reload".to_string()
        )));
        assert_eq!(
            harness.ctx.scheduler.pending_operation_for_test(),
            Some(ScheduledOperation::RuntimeFault {
                generation: old_generation + 1,
                attempt_id: old_attempt_id,
                fault: RuntimeCaptureFault::DeviceDisconnected(
                    "fault after failed reload".to_string()
                ),
            })
        );
    }

    #[test]
    fn failed_running_reload_startup_callback_preserves_old_capture_and_queues_no_fault() {
        struct CallbackThenFailStarter;

        impl LegacyCaptureStarter for CallbackThenFailStarter {
            fn start(
                &self,
                _prepared: &PreparedLegacyConfig,
                fault_sink: RuntimeFaultSink,
            ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
                let health = PipeWireHealth::unarmed_with_fault_sink(fault_sink);
                assert!(health.record_fatal(RuntimeCaptureFault::BackendFault(
                    "candidate callback fault".to_string(),
                )));
                Err(CaptureAttemptError {
                    class: ErrorClass::Transient,
                    capture_state: CaptureState::Faulted,
                    message: "candidate startup failed".to_string(),
                })
            }
        }

        let harness = SupervisorHarness::running();
        let before = harness.snapshot();

        let response = reload_daemon_config_with_recovery(
            &harness.ctx,
            None,
            &CallbackThenFailStarter,
            &SuccessfulLegacyRecovery,
        );

        assert!(!response.ok);
        assert_visible_authority_preserved_with_one_generation_advance(
            &before,
            &harness.snapshot(),
        );
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
    }

    #[test]
    fn stop_capture_advances_at_entry_and_rejects_old_fault_and_retry() {
        let harness = SupervisorHarness::running();
        let old_generation = harness.lifecycle().generation;
        let old_attempt_id = CaptureAttemptId::from_raw_for_test(10);
        let old_sink = harness
            .ctx
            .scheduler
            .fault_sink(old_generation, old_attempt_id);
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::StopCapture);
        let ctx = Arc::clone(&harness.ctx);
        let worker = std::thread::spawn(move || stop_capture_transaction(&ctx));
        entered.wait();
        assert_eq!(harness.lifecycle().generation, old_generation + 1);
        release.wait();
        assert!(worker.join().unwrap().ok);

        old_sink.notify(RuntimeCaptureFault::BackendFault("old sink".to_string()));
        publish_runtime_fault(
            &harness.ctx,
            old_generation,
            old_attempt_id,
            RuntimeCaptureFault::BackendFault("old publication".to_string()),
        );
        let stale_starter = PanickingLegacyStarter {
            calls: AtomicUsize::new(0),
        };
        execute_retry_operation(
            &harness.ctx,
            old_generation,
            &stale_starter,
            &SuccessfulLegacyRecovery,
        );
        assert_eq!(stale_starter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
    }

    #[test]
    fn legacy_final_commit_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let starter = Arc::new(ScriptedLegacyCaptureStarter {
            outcomes: Mutex::new(VecDeque::from([Ok(test_active_capture_with_resources(
                &resources,
            ))])),
        });
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::LegacySuccess);
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let command = std::thread::spawn(move || {
            reload_daemon_config_with_recovery(
                &ctx,
                None,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(
            blocked,
            "Stop must not publish authority inside a command commit gate"
        );
        assert!(response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        release_capture_for_shutdown(&harness.ctx);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.dropped()[0], ProbeResource::Backend);
    }

    #[test]
    fn app_final_commit_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let token = begin_command_operation(&harness.ctx).unwrap();
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::AppSuccess);
        let ctx = Arc::clone(&harness.ctx);
        let candidate = test_app_candidate(true, Some(&resources));
        let command = std::thread::spawn(move || {
            install_prepared_then_follow_start_mode(
                &ctx,
                token,
                PreparedBootstrap::TestAppCandidate {
                    candidate,
                    entered: None,
                    release: None,
                },
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
                false,
                false,
                false,
            )
        });
        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(blocked, "Stop must wait for app final publication");
        assert!(response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        release_capture_for_shutdown(&harness.ctx);
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.dropped()[0], ProbeResource::Backend);
    }

    #[test]
    fn failure_publication_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::stopped();
        let starter = Arc::new(ScriptedLegacyCaptureStarter {
            outcomes: Mutex::new(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message: "candidate failed".to_string(),
            })])),
        });
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::Failure);
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let command = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(blocked, "Stop must wait for failure publication");
        assert!(!response.ok);
        assert_eq!(response.message, "candidate failed");
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
    }

    #[test]
    fn stop_capture_publication_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::running();
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::StopCapture);
        let ctx = Arc::clone(&harness.ctx);
        let command = std::thread::spawn(move || stop_capture_transaction(&ctx));
        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(blocked, "Stop must wait for StopCapture publication");
        assert!(response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        assert_eq!(harness.live_capture_resources(), 0);
    }

    #[test]
    fn running_start_noop_snapshot_linearizes_before_direct_stop() {
        let harness = SupervisorHarness::running();
        let before = harness.snapshot();
        let (entered, release) =
            install_final_mutation_pause(&harness.ctx, FinalMutationKind::RunningStartNoop);
        let ctx = Arc::clone(&harness.ctx);
        let command = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                &ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                },
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        let (stop_finished, stop_worker) = spawn_observed_shutdown(Arc::clone(&harness.ctx));
        let blocked = stop_was_blocked_before_release(&harness.ctx, &stop_finished);
        release.wait();
        let response = command.join().unwrap();
        if blocked {
            stop_finished.recv().unwrap();
        }
        stop_worker.join().unwrap();

        assert!(blocked, "Stop must wait for the running Start snapshot");
        assert!(response.ok);
        assert_eq!(response.message, "capture already running");
        assert_eq!(
            response.status.unwrap().lifecycle.daemon_state,
            DaemonState::Ready
        );
        assert_eq!(before.session_identity, harness.snapshot().session_identity);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        release_capture_for_shutdown(&harness.ctx);
    }

    #[test]
    fn poisoned_runtime_direct_stop_returns_coherent_stopping_status_and_clears_poison() {
        let harness = SupervisorHarness::running();
        let ctx = Arc::clone(&harness.ctx);
        let _ = std::thread::spawn(move || {
            let _guard = ctx.runtime.lock().unwrap();
            panic!("poison runtime for direct Stop");
        })
        .join();

        let response = handle_idle_request(&harness.ctx, ControlRequest::Stop);

        assert!(response.ok);
        let status = response.status.unwrap();
        assert_eq!(status.lifecycle.daemon_state, DaemonState::Stopping);
        assert_eq!(status.lifecycle.capture_state, CaptureState::Running);
        assert!(harness.ctx.runtime.lock().is_ok());
        release_capture_for_shutdown(&harness.ctx);
    }

    #[test]
    fn stop_capture_releases_capture_without_stopping_daemon() {
        let harness = SupervisorHarness::running();
        harness.stop_capture();
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
        assert!(!harness.stop_flag());
        assert_eq!(harness.live_capture_resources(), 0);
    }

    #[test]
    fn corrected_legacy_reload_starts_capture() {
        let harness =
            SupervisorHarness::stopped_with_outcomes(VecDeque::from([Ok(test_active_capture())]));
        let valid_text = toml::to_string(&test_legacy_fake_config()).unwrap();
        let response = harness.reload_with_text(&valid_text);
        assert!(response.ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Ready);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Running);
    }

    #[test]
    fn explicit_start_permanent_error_is_structured_and_not_terminal() {
        let harness =
            SupervisorHarness::stopped_with_outcomes(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Permanent,
                capture_state: CaptureState::Faulted,
                message: "invalid profile".to_string(),
            })]));
        let response = harness.start_capture();
        assert!(!response.ok);
        assert_eq!(
            response.error_context.error_class,
            Some(ErrorClass::Permanent)
        );
        assert_eq!(harness.lifecycle().retry_policy, RetryPolicy::Manual);
        let status = response.status.unwrap();
        assert_eq!(status.lifecycle.daemon_state, DaemonState::Degraded);
        assert_eq!(status.lifecycle.capture_state, CaptureState::Faulted);
        assert_eq!(status.lifecycle.retry_policy, RetryPolicy::Manual);
        assert_eq!(
            status.lifecycle.last_error.as_deref(),
            Some("invalid profile")
        );
        assert!(!harness.stop_flag());
    }

    #[test]
    fn explicit_start_transient_error_is_structured_and_not_terminal() {
        let harness =
            SupervisorHarness::stopped_with_outcomes(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "target missing".to_string(),
            })]));
        let response = harness.start_capture();
        assert!(!response.ok);
        assert_eq!(
            response.error_context.error_class,
            Some(ErrorClass::Transient)
        );
        assert_eq!(
            harness.lifecycle().capture_state,
            CaptureState::WaitingForDevice
        );
        let status = response.status.unwrap();
        assert_eq!(status.lifecycle.daemon_state, DaemonState::Degraded);
        assert_eq!(
            status.lifecycle.capture_state,
            CaptureState::WaitingForDevice
        );
        assert_eq!(status.lifecycle.retry_policy, RetryPolicy::BoundedBackoff);
        assert_eq!(status.lifecycle.retry_attempt, 1);
        assert!(status.lifecycle.next_retry_at.is_some());
        assert_eq!(
            status.lifecycle.last_error.as_deref(),
            Some("target missing")
        );
        assert!(!harness.stop_flag());
    }

    #[test]
    fn operator_generation_overflow_is_fatal_without_retry_or_other_mutation() {
        let harness = SupervisorHarness::stopped();
        {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            runtime.lifecycle.generation = u64::MAX;
        }
        let before = harness.snapshot();

        let response = harness.start_capture();

        assert!(!response.ok);
        assert_eq!(response.error_context.error_class, Some(ErrorClass::Fatal));
        assert_eq!(harness.snapshot(), before);
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
    }

    #[test]
    fn fatal_capture_attempt_state_application_rejects_without_mutation() {
        let harness = SupervisorHarness::stopped();
        let before = harness.snapshot();

        let rejected = {
            let mut runtime = harness.ctx.runtime.lock().unwrap();
            apply_capture_attempt_error_to_state(
                &mut runtime,
                CaptureAttemptError {
                    class: ErrorClass::Fatal,
                    capture_state: CaptureState::Faulted,
                    message: "fatal state application".to_string(),
                },
                harness.ctx.clock.now(),
            )
        };

        assert_eq!(
            rejected,
            Err(CaptureAttemptError {
                class: ErrorClass::Fatal,
                capture_state: CaptureState::Faulted,
                message: "fatal state application".to_string(),
            })
        );
        assert_eq!(harness.snapshot(), before);
        assert!(!harness.stop_flag());
    }

    #[test]
    fn publish_generation_overflow_cleans_poisoned_resources_then_signals_fatal() {
        let harness = SupervisorHarness::stopped();
        let resources = Arc::new(AttemptResourceCounters::default());
        let mut stale = test_active_capture_with_resources(&resources);
        let poisoned = Arc::clone(&harness.ctx);
        let _ = std::thread::spawn(move || {
            let mut runtime = poisoned.runtime.lock().unwrap();
            runtime.lifecycle.generation = u64::MAX;
            runtime.capture = stale.backend.take();
            runtime.session = stale.session.take();
            panic!("publish overflow poison");
        })
        .join();

        publish_capture_attempt_error(
            &harness.ctx,
            CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "candidate unavailable".to_string(),
            },
        );

        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.lifecycle.generation, u64::MAX);
        assert_eq!(runtime.lifecycle.error_class, None);
        assert_ne!(runtime.lifecycle.retry_policy, RetryPolicy::Manual);
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        drop(runtime);
        assert!(harness.stop_flag());
        assert!(harness.ctx.scheduler.is_stopped());
        assert!(harness
            .ctx
            .first_fatal
            .get()
            .unwrap()
            .contains("generation overflow"));
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
        assert_eq!(
            &resources.dropped()[..2],
            &[ProbeResource::Backend, ProbeResource::Session]
        );
    }

    #[test]
    fn recovered_poison_generation_overflow_returns_fatal_and_retains_first_cause() {
        let harness = SupervisorHarness::stopped();
        let token = begin_command_operation(&harness.ctx).unwrap();
        harness
            .ctx
            .first_fatal
            .set("earlier fatal cause".to_string())
            .unwrap();
        let resources = Arc::new(AttemptResourceCounters::default());
        let mut stale = test_active_capture_with_resources(&resources);
        let poisoned = Arc::clone(&harness.ctx);
        let _ = std::thread::spawn(move || {
            let mut runtime = poisoned.runtime.lock().unwrap();
            runtime.lifecycle.generation = u64::MAX;
            runtime.capture = stale.backend.take();
            runtime.session = stale.session.take();
            panic!("command overflow poison");
        })
        .join();

        let response = command_attempt_failure(
            &harness.ctx,
            token,
            CaptureAttemptError {
                class: ErrorClass::Transient,
                capture_state: CaptureState::WaitingForDevice,
                message: "candidate unavailable".to_string(),
            },
            false,
        );

        assert!(!response.ok);
        assert_eq!(response.error_context.error_class, Some(ErrorClass::Fatal));
        assert!(response.message.contains("generation overflow"));
        assert_eq!(
            harness.ctx.first_fatal.get().map(String::as_str),
            Some("earlier fatal cause")
        );
        assert!(harness.stop_flag());
        assert_eq!(resources.live(ProbeResource::Backend), 0);
        assert_eq!(resources.live(ProbeResource::Session), 0);
        let runtime = harness.ctx.runtime.lock().unwrap();
        assert_eq!(runtime.lifecycle.error_class, None);
        assert_ne!(runtime.lifecycle.retry_policy, RetryPolicy::Manual);
    }

    #[test]
    fn stale_fatal_attempt_is_superseded_without_signalling() {
        let harness = SupervisorHarness::stopped();
        let stale_token = begin_command_operation(&harness.ctx).unwrap();
        let _current_token = begin_command_operation(&harness.ctx).unwrap();

        let response = command_attempt_failure(
            &harness.ctx,
            stale_token,
            CaptureAttemptError {
                class: ErrorClass::Fatal,
                capture_state: CaptureState::Faulted,
                message: "stale fatal".to_string(),
            },
            false,
        );

        assert_eq!(
            response.error_context.error_class,
            Some(ErrorClass::Transient)
        );
        assert!(!harness.stop_flag());
        assert!(harness.ctx.first_fatal.get().is_none());
    }

    #[test]
    fn real_listener_fatal_start_exits_without_external_wake_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = test_legacy_fake_config();
        config.control_socket_path = socket_path.clone();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let ctx = build_idle_context(bootstrap, prepared).unwrap();
        {
            let mut runtime = ctx.runtime.lock().unwrap();
            runtime.state = "stopped".to_string();
            runtime.lifecycle.mark_stopped(None);
        }
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let starter: Arc<dyn LegacyCaptureStarter> = Arc::new(ScriptedLegacyCaptureStarter {
            outcomes: Mutex::new(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Fatal,
                capture_state: CaptureState::Faulted,
                message: "scripted fatal capture start".to_string(),
            })])),
        });
        let recovery: Arc<dyn LegacyStartupRecovery> = Arc::new(SuccessfulLegacyRecovery);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let listener_ctx = Arc::clone(&ctx);
        let listener = std::thread::spawn(move || {
            let result = run_idle_listener_with_dependencies(
                listener_ctx,
                socket,
                starter,
                recovery,
                || {},
                |_| {},
                || {},
            );
            finished_tx.send(result).unwrap();
        });

        let response_result = (|| -> Result<ControlResponse> {
            let mut stream = UnixStream::connect(&socket_path)
                .map_err(|source| io_error(&socket_path, source))?;
            stream
                .set_read_timeout(Some(ROUTE_TEST_TIMEOUT))
                .map_err(|source| io_error(&socket_path, source))?;
            let request = serde_json::to_string(&ControlRequest::StartCapture {
                profile: None,
                activate: false,
            })
            .map_err(|error| LambError::Control(error.to_string()))?;
            writeln!(stream, "{request}").map_err(|source| io_error(&socket_path, source))?;
            let mut response = String::new();
            BufReader::new(stream)
                .read_line(&mut response)
                .map_err(|source| io_error(&socket_path, source))?;
            serde_json::from_str(&response)
                .map_err(|error| LambError::Control(format!("invalid response: {error}")))
        })();
        let response = match response_result {
            Ok(response) => response,
            Err(error) => {
                signal_fatal(&ctx, format!("fatal listener test watchdog: {error}"));
                let _ = finished_rx.recv_timeout(ROUTE_TEST_TIMEOUT);
                let _ = listener.join();
                panic!("fatal StartCapture response was not delivered: {error}");
            }
        };
        assert!(!response.ok);
        assert_eq!(response.error_context.error_class, Some(ErrorClass::Fatal));
        assert_eq!(response.message, "scripted fatal capture start");

        let result = match finished_rx.recv_timeout(ROUTE_TEST_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                signal_fatal(&ctx, format!("fatal listener exit watchdog: {error}"));
                let result = finished_rx
                    .recv_timeout(ROUTE_TEST_TIMEOUT)
                    .expect("listener did not exit after watchdog");
                listener.join().unwrap();
                panic!("listener required watchdog to exit: {result:?}");
            }
        };
        listener.join().unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.process_exit_code(), 1);
        assert_ne!(error.process_exit_code(), 78);
        assert!(error.to_string().contains("scripted fatal capture start"));
        assert!(!socket_path.exists());
        assert_eq!(ctx.scheduler.pending_operation_for_test(), None);
        let runtime = ctx.runtime.lock().unwrap();
        assert_eq!(runtime.lifecycle.error_class, None);
        assert_ne!(runtime.lifecycle.retry_policy, RetryPolicy::Manual);
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        assert!(runtime.capture_health.is_none());
        assert!(runtime.resolved_capture.is_none());
    }

    #[test]
    fn fatal_signal_is_observed_with_silent_already_accepted_client() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("lamb.toml");
        let socket_path = temp.path().join("control.sock");
        let mut config = test_legacy_fake_config();
        config.control_socket_path = socket_path.clone();
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let bootstrap = bootstrap_config(&config_path).unwrap();
        let prepared = prepare_bootstrap_config(&bootstrap);
        let ctx = build_idle_context(bootstrap, prepared).unwrap();
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let accepted = Arc::new(std::sync::Barrier::new(2));
        let release_read = Arc::new(std::sync::Barrier::new(2));
        let first_accept = Arc::new(AtomicBool::new(true));
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let listener_ctx = Arc::clone(&ctx);
        let listener_accepted = Arc::clone(&accepted);
        let listener_release = Arc::clone(&release_read);
        let listener_first_accept = Arc::clone(&first_accept);
        let listener = std::thread::spawn(move || {
            let result = run_idle_listener_with_dependencies(
                listener_ctx,
                socket,
                Arc::new(ScriptedLegacyCaptureStarter {
                    outcomes: Mutex::new(VecDeque::new()),
                }),
                Arc::new(SuccessfulLegacyRecovery),
                move || {
                    if listener_first_accept.swap(false, Ordering::SeqCst) {
                        listener_accepted.wait();
                        listener_release.wait();
                    }
                },
                |_| {},
                || {},
            );
            finished_tx.send(result).unwrap();
        });

        let mut silent_client = Some(UnixStream::connect(&socket_path).unwrap());
        accepted.wait();
        let started = std::time::Instant::now();
        signal_fatal(&ctx, "silent client fatal".to_string());
        release_read.wait();
        let result = match finished_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result,
            Err(error) => {
                drop(silent_client.take());
                let _ = finished_rx.recv_timeout(ROUTE_TEST_TIMEOUT);
                let _ = listener.join();
                panic!("listener exceeded bounded silent-client shutdown: {error}");
            }
        };
        let elapsed = started.elapsed();
        listener.join().unwrap();

        assert!(silent_client.is_some(), "silent client stayed connected");
        let error = result.unwrap_err();
        assert_eq!(error.process_exit_code(), 1);
        assert!(error.to_string().contains("silent client fatal"));
        assert!(elapsed < Duration::from_secs(3), "elapsed: {elapsed:?}");
        assert!(!socket_path.exists());
    }

    #[test]
    fn internal_retry_fatal_install_response_signals_shutdown() {
        let harness = SupervisorHarness::stopped();
        let generation = harness.lifecycle().generation;
        let starter = ScriptedLegacyCaptureStarter {
            outcomes: Mutex::new(VecDeque::from([Err(CaptureAttemptError {
                class: ErrorClass::Fatal,
                capture_state: CaptureState::Faulted,
                message: "fatal retry install".to_string(),
            })])),
        };

        execute_retry_operation(
            &harness.ctx,
            generation,
            &starter,
            &SuccessfulLegacyRecovery,
        );

        assert!(harness.stop_flag());
        assert_eq!(
            harness.ctx.first_fatal.get().map(String::as_str),
            Some("fatal retry install")
        );
        assert!(harness.ctx.scheduler.is_stopped());
        assert_eq!(harness.ctx.scheduler.pending_operation_for_test(), None);
        let lifecycle = harness.lifecycle();
        assert_eq!(lifecycle.error_class, None);
        assert_ne!(lifecycle.retry_policy, RetryPolicy::Manual);
    }

    #[test]
    fn stop_generation_prevents_stale_reload_completion_from_publishing() {
        let harness = SupervisorHarness::running();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture())),
        });
        let config_path = harness.config_path.clone();
        let replacement = config_path.with_extension("replacement");
        fs::write(
            &replacement,
            toml::to_string(&test_legacy_fake_config()).unwrap(),
        )
        .unwrap();
        fs::rename(replacement, config_path).unwrap();
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let reload = std::thread::spawn(move || {
            reload_daemon_config_with_recovery(
                &ctx,
                None,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });

        entered.wait();
        let stopped = harness.stop_capture();
        assert!(stopped.ok);
        release.wait();
        let stale = reload.join().unwrap();

        assert!(!stale.ok);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
        assert!(harness.ctx.runtime.lock().unwrap().session.is_none());
    }

    #[test]
    fn stale_start_after_stop_capture_does_not_publish() {
        let harness = SupervisorHarness::stopped();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture())),
        });
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let worker = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        assert!(harness.stop_capture().ok);
        release.wait();

        assert!(!worker.join().unwrap().ok);
        assert_eq!(harness.lifecycle().capture_state, CaptureState::Stopped);
    }

    #[test]
    fn stale_start_after_reload_does_not_replace_new_authority() {
        let harness = SupervisorHarness::stopped();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture())),
        });
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let worker = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(test_active_capture()));
        let replacement =
            harness.reload_with_text(&toml::to_string(&test_legacy_fake_config()).unwrap());
        assert!(replacement.ok);
        let replacement_session = harness.snapshot().session_identity;
        release.wait();

        assert!(!worker.join().unwrap().ok);
        assert_eq!(harness.snapshot().session_identity, replacement_session);
    }

    #[test]
    fn stale_start_after_direct_stop_does_not_publish() {
        let harness = SupervisorHarness::stopped();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture())),
        });
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let worker = std::thread::spawn(move || {
            start_capture_transaction_with_recovery(
                &ctx,
                None,
                false,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        begin_shutdown(&harness.ctx);
        release.wait();

        assert!(!worker.join().unwrap().ok);
        assert_eq!(harness.lifecycle().daemon_state, DaemonState::Stopping);
        assert!(harness.ctx.runtime.lock().unwrap().session.is_none());
    }

    #[test]
    fn stale_reload_after_newer_reload_does_not_replace_new_authority() {
        let harness = SupervisorHarness::running();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let starter = Arc::new(BlockingLegacyCaptureStarter {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            active: Mutex::new(Some(test_active_capture())),
        });
        let ctx = Arc::clone(&harness.ctx);
        let worker_starter = Arc::clone(&starter);
        let worker = std::thread::spawn(move || {
            reload_daemon_config_with_recovery(
                &ctx,
                None,
                worker_starter.as_ref(),
                &SuccessfulLegacyRecovery,
            )
        });
        entered.wait();
        harness
            .starter
            .outcomes
            .lock()
            .unwrap()
            .push_back(Ok(test_active_capture()));
        assert!(
            harness
                .reload_with_text(&toml::to_string(&test_legacy_fake_config()).unwrap())
                .ok
        );
        let replacement_session = harness.snapshot().session_identity;
        release.wait();

        assert!(!worker.join().unwrap().ok);
        assert_eq!(harness.snapshot().session_identity, replacement_session);
    }

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
    fn late_listener_setup_failure_drops_real_attempt_before_worker_publication() {
        let prepared = prepare_legacy_config(test_legacy_fake_config()).unwrap();
        let counters = Arc::new(AttemptResourceCounters::default());
        let worker_published = Arc::new(AtomicBool::new(false));
        let published = Arc::clone(&worker_published);
        let mut active = start_legacy_capture(&prepared, RuntimeFaultSink::default()).unwrap();
        active.backend_resource_probe = Some(Box::new(counters.lease(ProbeResource::Backend)));
        let session = Arc::get_mut(active.session.as_mut().unwrap()).unwrap();
        session.attempt_resource_probes = vec![
            Box::new(counters.lease(ProbeResource::Session)),
            Box::new(counters.lease(ProbeResource::Arena)),
            Box::new(counters.lease(ProbeResource::Workspace)),
            Box::new(counters.lease(ProbeResource::Descriptor)),
        ];

        let result = finish_legacy_capture_startup_with(
            active,
            &prepared,
            (),
            |_| {
                Err(LambError::Control(
                    "injected final listener setup failure".to_string(),
                ))
            },
            move |_, _| {
                published.store(true, Ordering::SeqCst);
                Ok(())
            },
        );

        let error = result.unwrap_err();
        assert!(matches!(&error, LambError::Control(_)));
        assert_eq!(error.process_exit_code(), 1);
        assert!(!worker_published.load(Ordering::SeqCst));
        assert_eq!(counters.live(ProbeResource::Backend), 0);
        assert_eq!(counters.live(ProbeResource::Session), 0);
        assert_eq!(counters.live(ProbeResource::Arena), 0);
        assert_eq!(counters.live(ProbeResource::Workspace), 0);
        assert_eq!(counters.live(ProbeResource::Descriptor), 0);
        let dropped = counters.dropped();
        assert_eq!(dropped.len(), 5);
        assert_eq!(dropped.first(), Some(&ProbeResource::Backend));
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

    #[test]
    fn app_context_records_post_bind_initialization_error_instead_of_returning_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = app_config::AppConfig::default();
        config.daemon.active_profile = Some("missing-profile".to_string());
        let bootstrap = BootstrapConfig {
            config_path: temp.path().join("lamb.toml"),
            text: None,
            family: ConfigFamily::App,
            control_socket_path: temp.path().join("control.sock"),
        };
        let prepared = PreparedBootstrap::App(app_config::LoadedAppConfig {
            config,
            state: ConfigLoadState::Loaded,
            error: None,
        });

        let ctx = build_idle_context(bootstrap, prepared)
            .expect("post-bind app initialization faults must remain inspectable");
        let runtime = ctx.runtime.lock().unwrap();
        assert_eq!(runtime.state, "faulted");
        assert_eq!(runtime.lifecycle.daemon_state, DaemonState::Degraded);
        assert_eq!(runtime.lifecycle.capture_state, CaptureState::Faulted);
        assert_eq!(runtime.lifecycle.error_class, Some(ErrorClass::Permanent));
        assert_eq!(runtime.lifecycle.retry_policy, RetryPolicy::Manual);
        assert_eq!(runtime.lifecycle.retry_attempt, 0);
        assert_eq!(runtime.lifecycle.next_retry_at, None);
        assert!(runtime
            .last_error
            .as_deref()
            .unwrap()
            .contains("missing-profile"));
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        drop(runtime);
        assert_eq!(ctx.scheduler.pending_operation_for_test(), None);
    }

    struct PanickingLegacyStarter {
        calls: AtomicUsize,
    }

    impl LegacyCaptureStarter for PanickingLegacyStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("permanent prepared fault must not invoke legacy starter");
        }
    }

    #[test]
    fn permanent_prepared_fault_never_invokes_legacy_starter() {
        let temp = tempfile::tempdir().unwrap();
        let bootstrap = BootstrapConfig {
            config_path: temp.path().join("lamb.toml"),
            text: None,
            family: ConfigFamily::Legacy,
            control_socket_path: temp.path().join("control.sock"),
        };
        let ctx = build_idle_context(
            bootstrap,
            PreparedBootstrap::Faulted {
                family: ConfigFamily::Legacy,
                fallback_app_config: app_config::AppConfig::default(),
                message: "schema fault".to_string(),
            },
        )
        .unwrap();
        let starter = PanickingLegacyStarter {
            calls: AtomicUsize::new(0),
        };

        let _ = attempt_configured_start(&ctx, &starter);

        assert_eq!(starter.calls.load(Ordering::SeqCst), 0);
        let runtime = ctx.runtime.lock().unwrap();
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        assert_eq!(runtime.lifecycle.error_class, Some(ErrorClass::Permanent));
    }

    struct SingleActiveStarter {
        active: Mutex<Option<ActiveCapture>>,
    }

    impl LegacyCaptureStarter for SingleActiveStarter {
        fn start(
            &self,
            _prepared: &PreparedLegacyConfig,
            _fault_sink: RuntimeFaultSink,
        ) -> std::result::Result<ActiveCapture, CaptureAttemptError> {
            Ok(self.active.lock().unwrap().take().unwrap())
        }
    }

    struct FailingLegacyRecovery {
        calls: AtomicUsize,
    }

    impl LegacyStartupRecovery for FailingLegacyRecovery {
        fn failed_count(&self, _session: &CaptureSession) -> Result<usize> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LambError::Control(
                "injected startup recovery failure".to_string(),
            ))
        }
    }

    #[test]
    fn legacy_recovery_failure_cleans_attempt_before_degraded_publication() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = prepare_legacy_config(test_legacy_fake_config()).unwrap();
        let counters = Arc::new(AttemptResourceCounters::default());
        let mut active = start_legacy_capture(&prepared, RuntimeFaultSink::default()).unwrap();
        active.backend_resource_probe = Some(Box::new(counters.lease(ProbeResource::Backend)));
        Arc::get_mut(active.session.as_mut().unwrap())
            .unwrap()
            .attempt_resource_probes = vec![Box::new(counters.lease(ProbeResource::Session))];
        let starter = SingleActiveStarter {
            active: Mutex::new(Some(active)),
        };
        let recovery = FailingLegacyRecovery {
            calls: AtomicUsize::new(0),
        };
        let ctx = build_idle_context(
            BootstrapConfig {
                config_path: temp.path().join("lamb.toml"),
                text: None,
                family: ConfigFamily::Legacy,
                control_socket_path: temp.path().join("control.sock"),
            },
            PreparedBootstrap::Legacy(prepared),
        )
        .unwrap();

        let _ = attempt_configured_start_with_recovery(&ctx, &starter, &recovery);

        assert_eq!(recovery.calls.load(Ordering::SeqCst), 1);
        assert_eq!(counters.live(ProbeResource::Backend), 0);
        assert_eq!(counters.live(ProbeResource::Session), 0);
        assert_eq!(
            counters.dropped(),
            vec![ProbeResource::Backend, ProbeResource::Session]
        );
        let runtime = ctx.runtime.lock().unwrap();
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        assert_eq!(runtime.lifecycle.daemon_state, DaemonState::Degraded);
        assert!(runtime
            .last_error
            .as_deref()
            .unwrap()
            .contains("startup recovery failure"));
    }

    struct OrderedDropProbe {
        name: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for OrderedDropProbe {
        fn drop(&mut self) {
            self.order.lock().unwrap().push(self.name);
        }
    }

    #[test]
    fn stopped_ownership_drops_backend_before_final_session() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let backend = OrderedDropProbe {
            name: "backend",
            order: Arc::clone(&order),
        };
        let session = OrderedDropProbe {
            name: "session",
            order: Arc::clone(&order),
        };

        drop_capture_then_session(Some(backend), Some(session));

        assert_eq!(*order.lock().unwrap(), vec!["backend", "session"]);
    }

    #[test]
    fn private_app_post_backend_failure_drops_backend_before_session_and_retains_nothing() {
        let counters = Arc::new(AttemptResourceCounters::default());
        let backend = CaptureBackend::TestProbe(Box::new(counters.lease(ProbeResource::Backend)));
        let mut session = test_session();
        session.attempt_resource_probes = vec![Box::new(counters.lease(ProbeResource::Session))];
        let mut attempt = PrivateAppCaptureAttempt::for_test(backend, Arc::new(session));

        let result = attempt.run_post_backend_step(|_| {
            Err(LambError::Control(
                "injected post-backend policy failure".to_string(),
            ))
        });
        assert!(result.is_err());
        drop(attempt);

        assert_eq!(counters.live(ProbeResource::Backend), 0);
        assert_eq!(counters.live(ProbeResource::Session), 0);
        assert_eq!(
            counters.dropped(),
            vec![ProbeResource::Backend, ProbeResource::Session]
        );
    }

    fn listener_test_request(socket: &Path, request: &ControlRequest) -> UnixStream {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(ROUTE_TEST_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(ROUTE_TEST_TIMEOUT)).unwrap();
        writeln!(stream, "{}", serde_json::to_string(request).unwrap()).unwrap();
        stream
    }

    fn assert_config_failure_shared_listener(
        root: &Path,
        config_path: PathBuf,
        diagnostic: String,
        rejected_request: ControlRequest,
    ) {
        let socket_path = root.join("control.sock");
        let socket = ControlSocketOwner::bind(socket_path.clone()).unwrap();
        let (server_done_tx, server_done_rx) = mpsc::sync_channel(1);
        let (operation_entered_tx, operation_entered_rx) = mpsc::sync_channel(1);
        let (operation_release_tx, operation_release_rx) = mpsc::sync_channel(1);
        let server_socket_path = socket_path.clone();
        let server_calibration_root = root.join("calibration");
        let server_diagnostic = diagnostic.clone();
        let server = std::thread::spawn(move || {
            let result = run_idle_fallback_on_listener(
                config_path,
                server_socket_path,
                server_diagnostic,
                server_calibration_root,
                socket,
                move |request| {
                    if matches!(
                        request,
                        ControlRequest::Threshold {
                            request: ThresholdRequest::Show { profile }
                        } if profile == "active"
                    ) {
                        operation_entered_tx.send(()).unwrap();
                        operation_release_rx
                            .recv_timeout(ROUTE_TEST_TIMEOUT)
                            .unwrap();
                    }
                },
            );
            server_done_tx.send(result).unwrap();
        });

        let status =
            read_test_response(listener_test_request(&socket_path, &ControlRequest::Status));
        assert!(status.ok);
        assert_eq!(
            status.status.unwrap().last_error.as_deref(),
            Some(diagnostic.as_str())
        );

        let rejected = read_test_response(listener_test_request(&socket_path, &rejected_request));
        assert!(!rejected.ok);
        assert_eq!(rejected.message, diagnostic);
        assert_eq!(
            rejected.status.unwrap().last_error.as_deref(),
            Some(diagnostic.as_str())
        );

        let active = listener_test_request(
            &socket_path,
            &ControlRequest::Threshold {
                request: ThresholdRequest::Show {
                    profile: "active".to_string(),
                },
            },
        );
        operation_entered_rx
            .recv_timeout(ROUTE_TEST_TIMEOUT)
            .unwrap();

        let queued_request = ControlRequest::Threshold {
            request: ThresholdRequest::Show {
                profile: "queued".to_string(),
            },
        };
        let queued: Vec<_> = (0..DEFAULT_CONTROL_QUEUE_CAPACITY)
            .map(|_| listener_test_request(&socket_path, &queued_request))
            .collect();
        let busy = read_test_response(listener_test_request(&socket_path, &queued_request));
        assert!(!busy.ok);
        assert_eq!(busy.message, "operation queue is busy or shutting down");

        let stop = read_test_response(listener_test_request(&socket_path, &ControlRequest::Stop));
        assert!(stop.ok);
        assert_eq!(stop.message, "stopping");
        operation_release_tx.send(()).unwrap();
        let active = read_test_response(active);
        assert!(!active.ok);
        assert_eq!(active.message, "shutting down");
        for stream in queued {
            let cancelled = read_test_response(stream);
            assert!(!cancelled.ok);
            assert_eq!(cancelled.message, "shutting down");
        }
        server_done_rx
            .recv_timeout(ROUTE_TEST_TIMEOUT)
            .expect("shared listener did not finish")
            .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn missing_config_uses_shared_bounded_listener_and_preserves_originating_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("missing.toml");
        let diagnostic = format!("config file not found: {}", config_path.display());
        assert_config_failure_shared_listener(
            temp.path(),
            config_path,
            diagnostic,
            ControlRequest::Threshold {
                request: ThresholdRequest::Show {
                    profile: "studio".to_string(),
                },
            },
        );
    }

    #[test]
    fn invalid_config_uses_shared_bounded_listener_and_preserves_originating_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("invalid.toml");
        fs::write(&config_path, "not = [valid\n").unwrap();
        let loaded = app_config::load_optional_config(&config_path).unwrap();
        assert_eq!(loaded.state, ConfigLoadState::Invalid);
        let diagnostic = loaded.error.unwrap();
        assert_config_failure_shared_listener(
            temp.path(),
            config_path,
            diagnostic,
            ControlRequest::StartCapture {
                profile: Some("studio".to_string()),
                activate: false,
            },
        );
    }

    fn route_test_request(
        ctx: &IdleDaemonContext,
        lane: &OperationLane<OperationJob>,
        request: ControlRequest,
    ) -> UnixStream {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(ROUTE_TEST_TIMEOUT)).unwrap();
        peer.set_write_timeout(Some(ROUTE_TEST_TIMEOUT)).unwrap();
        writeln!(peer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        route_idle_stream(ctx, lane, stream).unwrap();
        peer
    }

    fn join_worker_bounded(worker: std::thread::JoinHandle<()>) {
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let joiner = std::thread::spawn(move || {
            finished_tx.send(worker.join()).unwrap();
        });
        finished_rx
            .recv_timeout(ROUTE_TEST_TIMEOUT)
            .expect("operation worker did not finish")
            .unwrap();
        joiner.join().unwrap();
    }

    fn read_test_response(stream: UnixStream) -> ControlResponse {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn threshold_test_config(root: &Path, ports: &[(&str, &str)]) -> app_config::AppConfig {
        let mut config = app_config::AppConfig::default();
        profile::create_profile(&mut config, "studio", "jack").unwrap();
        profile::set_profile_field(&mut config, "studio", "clientName", "lamb").unwrap();
        profile::set_profile_field(&mut config, "studio", "buffer.seconds", "1").unwrap();
        profile::set_profile_field(
            &mut config,
            "studio",
            "export.outputDir",
            root.to_str().unwrap(),
        )
        .unwrap();
        profile::set_profile_field(&mut config, "studio", "export.mode", "per-channel").unwrap();
        profile::set_profile_field(&mut config, "studio", "export.format", "wav").unwrap();
        for (name, source) in ports {
            profile::add_capture_port(&mut config, "studio", source, name).unwrap();
        }
        config
    }

    fn threshold_mutation_context(
        root: &Path,
        config: app_config::AppConfig,
        active_profile: Option<profile::ResolvedProfile>,
        session: Option<Arc<CaptureSession>>,
    ) -> IdleDaemonContext {
        profile::save_config(&root.join("lamb.toml"), &config).unwrap();
        let test_capture_attached = session.is_some();
        IdleDaemonContext {
            config_path: root.join("lamb.toml"),
            control_socket_path: root.join("control.sock"),
            calibration_root: root.join("calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config,
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: if session.is_some() {
                    "capturing".to_string()
                } else {
                    "idle".to_string()
                },
                last_error: None,
                config_load_error: None,
                active_profile,
                capture: None,
                capture_health: None,
                session,
                test_capture_attached,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        }
    }

    fn threshold_capture_session(
        resolved: &profile::ResolvedProfile,
        sample_rate: u32,
    ) -> Arc<CaptureSession> {
        threshold_capture_session_with_ingress(resolved, sample_rate).0
    }

    fn threshold_capture_session_with_ingress(
        resolved: &profile::ResolvedProfile,
        sample_rate: u32,
    ) -> (Arc<CaptureSession>, crate::capture_arena::CaptureIngress) {
        let session_frames = sample_rate.checked_mul(resolved.buffer_seconds).unwrap();
        let params = CaptureRuntimeParams {
            seconds: resolved.buffer_seconds,
            chunk_frames_override: Some(session_frames),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 1,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let channel_count = u32::try_from(resolved.ports.len()).unwrap();
        let (runtime, ingress) = CaptureRuntime::build(params, sample_rate, channel_count).unwrap();
        let session = Arc::new(
            CaptureSession::from_app_runtime(
                runtime,
                resolved,
                sample_rate,
                jack_live_identities(resolved).unwrap(),
            )
            .unwrap(),
        );
        (session, ingress)
    }

    struct CalibratedReplacementFixture {
        _temp: tempfile::TempDir,
        ctx: Arc<IdleDaemonContext>,
        session: Arc<CaptureSession>,
        ingress: crate::capture_arena::CaptureIngress,
        old_config: app_config::AppConfig,
        old_disk: Vec<u8>,
        old_active_profile: profile::ResolvedProfile,
        old_policy: crate::export_policy::ResolvedActivityPolicy,
        input_id: String,
        old_id: String,
        old_path: PathBuf,
        old_sample: Vec<u8>,
        old_metadata: Vec<u8>,
        old_identity: (u64, u64),
    }

    fn calibrated_replacement_fixture() -> CalibratedReplacementFixture {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let initial = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let configured = configured_identity(&initial, "mic").unwrap();
        let live = jack_live_identities(&initial).unwrap()[0].clone().unwrap();
        let old_threshold = crate::calibration::derive_calibrated_threshold(&mut [0.125]).unwrap();
        let old_id = "old-authoritative".to_string();
        let store =
            crate::calibration::CalibrationStore::new(temp.path().join("calibration"), 3).unwrap();
        let mut old = store
            .prepare(
                &configured,
                &live,
                &old_id,
                &[0.125, 0.125, 0.125],
                3,
                old_threshold,
                &mut [0.125],
                &[0.125],
                900,
            )
            .unwrap();
        old.mark_authoritative();
        let old_path = old.path().to_path_buf();
        let old_sample = fs::read(old.sample_path()).unwrap();
        let old_metadata = fs::read(old.metadata_path()).unwrap();
        let metadata = fs::metadata(&old_path).unwrap();
        let old_identity = (metadata.dev(), metadata.ino());
        drop(old);

        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: f64::from(old_threshold),
                    threshold_source: crate::activity::ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 900,
                    input_id: configured.input_id().to_string(),
                    calibration_id: Some(old_id.clone()),
                }),
            },
        );
        let old_active_profile =
            profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let (session, ingress) = threshold_capture_session_with_ingress(&old_active_profile, 3);
        let old_policy = session.policy.lock().unwrap().activity.clone();
        let ctx = Arc::new(threshold_mutation_context(
            temp.path(),
            config.clone(),
            Some(old_active_profile.clone()),
            Some(Arc::clone(&session)),
        ));
        let old_disk = fs::read(&ctx.config_path).unwrap();

        CalibratedReplacementFixture {
            _temp: temp,
            ctx,
            session,
            ingress,
            old_config: config,
            old_disk,
            old_active_profile,
            old_policy,
            input_id: configured.input_id().to_string(),
            old_id,
            old_path,
            old_sample,
            old_metadata,
            old_identity,
        }
    }

    fn run_calibrated_replacement<H, C>(
        fixture: &CalibratedReplacementFixture,
        durability_hook: H,
        cleanup_hook: C,
    ) -> Result<(String, ThresholdReport)>
    where
        H: FnMut(crate::calibration::DurabilityCheckpoint) -> Result<()> + Send + 'static,
        C: FnMut(&Path, crate::calibration::CleanupCheckpoint) -> Result<()> + Send + 'static,
    {
        run_calibrated_replacement_with_samples(
            fixture,
            [0.25, 0.25, 0.25],
            durability_hook,
            cleanup_hook,
        )
    }

    fn run_calibrated_replacement_with_samples<H, C>(
        fixture: &CalibratedReplacementFixture,
        samples: [f32; 3],
        durability_hook: H,
        cleanup_hook: C,
    ) -> Result<(String, ThresholdReport)>
    where
        H: FnMut(crate::calibration::DurabilityCheckpoint) -> Result<()> + Send + 'static,
        C: FnMut(&Path, crate::calibration::CleanupCheckpoint) -> Result<()> + Send + 'static,
    {
        let arena = Arc::clone(&fixture.session.arena);
        let (accepted, release) = arena.install_calibration_pause(false);
        let ctx = Arc::clone(&fixture.ctx);
        let transaction = std::thread::spawn(move || {
            let mut durability_hook = durability_hook;
            let mut cleanup_hook = cleanup_hook;
            calibrate_threshold_with(
                &ctx,
                "studio",
                "mic",
                1,
                || 1_000,
                &mut durability_hook,
                &mut cleanup_hook,
            )
        });
        accepted.wait();
        fixture.ingress.try_push_interleaved(&samples, 1).unwrap();
        release.wait();
        transaction.join().unwrap()
    }

    fn generation_names(fixture: &CalibratedReplacementFixture) -> Vec<String> {
        let mut names = fs::read_dir(fixture.ctx.calibration_root.join(&fixture.input_id))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn assert_old_authority_after_definite_calibration_failure(
        fixture: &CalibratedReplacementFixture,
        cleanup_paths: &[PathBuf],
    ) {
        use std::os::unix::fs::MetadataExt;

        // Catches publication being installed despite a definite transaction failure.
        assert_eq!(
            fs::read(&fixture.ctx.config_path).unwrap(),
            fixture.old_disk
        );
        let runtime = fixture.ctx.runtime.lock().unwrap();
        // Catches candidate assignment running before the durability seam succeeds.
        assert_eq!(runtime.config, fixture.old_config);
        // Catches active-profile replacement running independently of config assignment.
        assert_eq!(
            runtime.active_profile.as_ref(),
            Some(&fixture.old_active_profile)
        );
        // Catches session replacement or loss while the failed command holds daemon authority.
        assert!(Arc::ptr_eq(
            runtime.session.as_ref().unwrap(),
            &fixture.session
        ));
        drop(runtime);
        // Catches future live-policy installation before the durable config commit point.
        assert_eq!(
            fixture.session.policy.lock().unwrap().activity,
            fixture.old_policy
        );
        // Catches cleanup targeting the old authoritative generation on a failure path.
        assert!(!cleanup_paths.iter().any(|path| path == &fixture.old_path));
        // Catches sample or metadata damage hidden behind a still-present old directory.
        assert_eq!(
            fs::read(fixture.old_path.join("sample.wav")).unwrap(),
            fixture.old_sample
        );
        assert_eq!(
            fs::read(fixture.old_path.join("metadata.json")).unwrap(),
            fixture.old_metadata
        );
        let metadata = fs::metadata(&fixture.old_path).unwrap();
        // Catches replacement of the old path with foreign bytes under the same generation ID.
        assert_eq!((metadata.dev(), metadata.ino()), fixture.old_identity);
        // Catches a failed prepared handle surviving Drop instead of cleaning only its new state.
        assert_eq!(generation_names(fixture), vec![fixture.old_id.clone()]);
    }

    #[test]
    fn inactive_set_uses_fixed_clock_and_commits_exact_candidate_and_report() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let ctx = threshold_mutation_context(temp.path(), config, None, None);

        let (message, report) = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-42.5),
            || 1_234,
            profile::save_config,
            |_, _| panic!("set must not clean calibration generations"),
        )
        .unwrap();

        assert_eq!(message, "threshold updated");
        assert!(!report.active_profile);
        assert!(!report.capturing);
        assert_eq!(report.channels[0].effective_threshold_dbfs, Some(-42.5));
        let runtime = ctx.runtime.lock().unwrap();
        let installed = runtime.config.clone();
        drop(runtime);
        let persisted: app_config::AppConfig =
            toml::from_str(&fs::read_to_string(&ctx.config_path).unwrap()).unwrap();
        assert_eq!(persisted, installed);
        let threshold = persisted.profiles["studio"].channels["mic"]
            .activity
            .as_ref()
            .unwrap();
        assert_eq!(threshold.threshold_dbfs, -42.5);
        assert_eq!(threshold.updated_at_unix_seconds, 1_234);
        assert_eq!(
            threshold.threshold_source,
            crate::activity::ThresholdSource::Manual
        );
        assert_eq!(threshold.input_id.len(), 64);

        let (_, reset_report) = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            None,
            || 1_235,
            profile::save_config,
            |_, referenced| {
                assert!(referenced.is_empty());
                Ok(Vec::new())
            },
        )
        .unwrap();
        assert!(!reset_report.active_profile);
        assert!(reset_report.channels[0].stored.is_none());
        let persisted: app_config::AppConfig =
            toml::from_str(&fs::read_to_string(&ctx.config_path).unwrap()).unwrap();
        assert!(persisted.profiles["studio"].channels["mic"]
            .activity
            .is_none());
    }

    #[test]
    fn active_idle_reset_commits_before_cleanup_and_reports_pending_as_success() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&resolved, "mic").unwrap();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -50.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 100,
                    input_id: input.input_id().to_string(),
                    calibration_id: Some("old-generation".to_string()),
                }),
            },
        );
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let ctx = threshold_mutation_context(temp.path(), config, Some(resolved), None);

        let (_, set_report) = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-40.0),
            || 150,
            profile::save_config,
            |_, _| unreachable!(),
        )
        .unwrap();
        assert!(set_report.active_profile);
        assert!(!set_report.capturing);
        assert_eq!(set_report.channels[0].effective_threshold_dbfs, Some(-40.0));

        let (message, report) = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            None,
            || 200,
            profile::save_config,
            |root, referenced| {
                assert_eq!(root, temp.path().join("calibration"));
                assert!(referenced.is_empty());
                let persisted: app_config::AppConfig =
                    toml::from_str(&fs::read_to_string(temp.path().join("lamb.toml")).unwrap())
                        .unwrap();
                assert!(persisted.profiles["studio"].channels["mic"]
                    .activity
                    .is_none());
                Ok(vec![root.join("pending-generation")])
            },
        )
        .unwrap();

        assert!(message.contains("threshold reset"));
        assert!(message.contains("cleanup pending"));
        assert!(report.active_profile);
        assert!(!report.capturing);
        assert!(report.channels[0].stored.is_none());
        let runtime = ctx.runtime.lock().unwrap();
        assert!(runtime.config.profiles["studio"].channels["mic"]
            .activity
            .is_none());
        assert_eq!(
            runtime
                .active_profile
                .as_ref()
                .unwrap()
                .export_policy
                .activity,
            profile::validate_profile("studio", &runtime.config.profiles["studio"])
                .unwrap()
                .export_policy
                .activity
        );
    }

    #[test]
    fn manual_set_retains_only_a_same_configured_input_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&resolved, "mic").unwrap();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -55.0,
                    threshold_source: crate::activity::ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 10,
                    input_id: input.input_id().to_string(),
                    calibration_id: Some("same-input".to_string()),
                }),
            },
        );
        let ctx = threshold_mutation_context(temp.path(), config, None, None);
        mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-40.0),
            || 20,
            profile::save_config,
            |_, _| unreachable!(),
        )
        .unwrap();
        assert_eq!(
            ctx.runtime.lock().unwrap().config.profiles["studio"].channels["mic"]
                .activity
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some("same-input")
        );

        ctx.runtime
            .lock()
            .unwrap()
            .config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("mic")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
            .input_id = "0".repeat(64);
        mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-39.0),
            || 21,
            profile::save_config,
            |_, _| unreachable!(),
        )
        .unwrap();
        assert_eq!(
            ctx.runtime.lock().unwrap().config.profiles["studio"].channels["mic"]
                .activity
                .as_ref()
                .unwrap()
                .calibration_id,
            None
        );
    }

    #[test]
    fn active_set_preserves_session_identity_and_filters_untouched_stale_calibration() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(
            temp.path(),
            &[("mic", "system:capture_1"), ("room", "system:capture_2")],
        );
        let initial = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let room = configured_identity(&initial, "room").unwrap();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "room".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -60.0,
                    threshold_source: crate::activity::ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 100,
                    input_id: room.input_id().to_string(),
                    calibration_id: Some("missing-generation".to_string()),
                }),
            },
        );
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let session = threshold_capture_session(&active, 100);
        let arena = session.arena.clone();
        let coordinator = session.coordinator.clone();
        let ctx =
            threshold_mutation_context(temp.path(), config, Some(active), Some(session.clone()));

        mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-35.0),
            || 200,
            profile::save_config,
            |_, _| unreachable!(),
        )
        .unwrap();

        let runtime = ctx.runtime.lock().unwrap();
        let installed = runtime.session.as_ref().unwrap();
        assert!(Arc::ptr_eq(installed, &session));
        assert!(Arc::ptr_eq(&installed.arena, &arena));
        assert!(Arc::ptr_eq(&installed.coordinator, &coordinator));
        let activity = installed.policy.lock().unwrap().activity.clone();
        assert_eq!(
            activity.channels[0]
                .threshold
                .as_ref()
                .unwrap()
                .threshold_dbfs,
            -35.0
        );
        assert!(activity.channels[1].threshold.is_none());
        assert_eq!(
            runtime
                .active_profile
                .as_ref()
                .unwrap()
                .export_policy
                .activity
                .channels[1]
                .threshold
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some("missing-generation")
        );
    }

    #[test]
    fn new_session_policy_filters_raw_stale_calibrated_thresholds() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let initial = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&initial, "mic").unwrap();
        let live = jack_live_identities(&initial).unwrap()[0].clone().unwrap();
        let root = temp.path().join("calibration");
        let capacity = 3;
        let created_at = 100;
        let fixed_now = 200;
        let calibrated_dbfs =
            crate::calibration::derive_calibrated_threshold(&mut [0.125]).unwrap();
        let store = crate::calibration::CalibrationStore::new(&root, capacity).unwrap();
        let mut prepared = store
            .prepare(
                &input,
                &live,
                "valid-v2",
                &[0.125, 0.125, 0.125],
                3,
                calibrated_dbfs,
                &mut [0.125],
                &[0.125],
                created_at,
            )
            .unwrap();
        prepared.mark_authoritative();
        let valid_metadata = fs::read(prepared.metadata_path()).unwrap();

        let activity = |source, id: Option<&str>| app_config::ActivityThresholdConfig {
            threshold_dbfs: if source == crate::activity::ThresholdSource::Manual {
                -45.0
            } else {
                f64::from(calibrated_dbfs)
            },
            threshold_source: source,
            updated_at_unix_seconds: created_at,
            input_id: input.input_id().to_string(),
            calibration_id: id.map(str::to_string),
        };
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(activity(crate::activity::ThresholdSource::Manual, None)),
            },
        );
        let mut resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let mut session = threshold_capture_session(&resolved, 3);
        assert_eq!(session.configured_inputs, vec![input.clone()]);
        assert_eq!(session.resolved_live_inputs, vec![Some(live.clone())]);
        assert_eq!(session.calibration_sample_frames, capacity);
        assert_eq!(session.sample_rate, 3);
        assert_eq!(session.channel_count, 1);

        // A Manual record with the exact configured identity remains effective.
        install_effective_session_activity_policy(&config, &resolved, &session, &root, fixed_now)
            .unwrap();
        assert_eq!(
            session.policy.lock().unwrap().activity.channels[0]
                .threshold
                .as_ref()
                .unwrap()
                .threshold_dbfs,
            -45.0
        );

        // A complete metadata-v2 record with exact configured/live/rate/capacity
        // identity also remains effective.
        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("mic")
            .unwrap()
            .activity = Some(activity(
            crate::activity::ThresholdSource::Calibrated,
            Some("valid-v2"),
        ));
        resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        install_effective_session_activity_policy(&config, &resolved, &session, &root, fixed_now)
            .unwrap();
        assert_eq!(
            session.policy.lock().unwrap().activity.channels[0]
                .threshold
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some("valid-v2")
        );

        let assert_fail_open = |config: &app_config::AppConfig,
                                resolved: &profile::ResolvedProfile,
                                session: &CaptureSession,
                                now| {
            install_effective_session_activity_policy(config, resolved, session, &root, now)
                .unwrap();
            assert!(session.policy.lock().unwrap().activity.channels[0]
                .threshold
                .is_none());
        };

        // Missing state is accepted as stale and fails open.
        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("mic")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
            .calibration_id = Some("missing-generation".to_string());
        resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        assert_fail_open(&config, &resolved, &session, fixed_now);

        // Restore the complete artifact, then change only the current live key.
        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("mic")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
            .calibration_id = Some("valid-v2".to_string());
        Arc::get_mut(&mut session).unwrap().resolved_live_inputs[0] = Some(
            ResolvedLiveInputIdentity::new(
                crate::calibration::InputBackend::Jack,
                crate::calibration::LiveDeviceKeyKind::JackSourceClient,
                "other",
                "other:capture_1",
            )
            .unwrap(),
        );
        assert_fail_open(&config, &resolved, &session, fixed_now);
        Arc::get_mut(&mut session).unwrap().resolved_live_inputs[0] = Some(live.clone());

        // Metadata rate mismatch is stale even though every session identity is exact.
        let mut mismatched_rate: serde_json::Value =
            serde_json::from_slice(&valid_metadata).unwrap();
        mismatched_rate["sample_rate"] = serde_json::json!(4);
        fs::write(
            prepared.metadata_path(),
            serde_json::to_vec_pretty(&mismatched_rate).unwrap(),
        )
        .unwrap();
        assert_fail_open(&config, &resolved, &session, fixed_now);
        fs::write(prepared.metadata_path(), &valid_metadata).unwrap();

        // Expiry is strict: exactly 30 days remains valid, one second later is stale.
        let expiry_boundary = created_at + 30 * 24 * 60 * 60;
        install_effective_session_activity_policy(
            &config,
            &resolved,
            &session,
            &root,
            expiry_boundary,
        )
        .unwrap();
        assert!(session.policy.lock().unwrap().activity.channels[0]
            .threshold
            .is_some());
        assert_fail_open(&config, &resolved, &session, expiry_boundary + 1);

        // Corrupt metadata is classified as stale rather than blocking startup.
        fs::write(prepared.metadata_path(), b"{").unwrap();
        assert_fail_open(&config, &resolved, &session, fixed_now);
    }

    #[test]
    fn failed_atomic_save_changes_no_disk_runtime_or_policy_and_skips_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let session = threshold_capture_session(&active, 100);
        let ctx = threshold_mutation_context(
            temp.path(),
            config.clone(),
            Some(active),
            Some(session.clone()),
        );
        let before_disk = fs::read(&ctx.config_path).unwrap();
        let before_policy = session.policy.lock().unwrap().activity.clone();
        let cleanup_called = std::cell::Cell::new(false);

        let error = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            None,
            || 999,
            |_, _| {
                Err(LambError::Control(
                    "injected atomic save failure".to_string(),
                ))
            },
            |_, _| {
                cleanup_called.set(true);
                Ok(Vec::new())
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("injected atomic save failure"));
        assert_eq!(fs::read(&ctx.config_path).unwrap(), before_disk);
        assert_eq!(ctx.runtime.lock().unwrap().config, config);
        assert_eq!(session.policy.lock().unwrap().activity, before_policy);
        assert!(!cleanup_called.get());
    }

    #[test]
    fn indeterminate_reset_publication_installs_nothing_and_leaves_disk_authority_uncertain() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&resolved, "mic").unwrap();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -45.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 10,
                    input_id: input.input_id().to_string(),
                    calibration_id: Some("retained-until-known".to_string()),
                }),
            },
        );
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let session = threshold_capture_session(&active, 100);
        let ctx = threshold_mutation_context(
            temp.path(),
            config.clone(),
            Some(active),
            Some(session.clone()),
        );
        let (before_active_profile, before_session) = {
            let runtime = ctx.runtime.lock().unwrap();
            (
                runtime.active_profile.clone(),
                runtime.session.as_ref().unwrap().clone(),
            )
        };
        let before_policy = session.policy.lock().unwrap().activity.clone();
        let cleanup_called = std::cell::Cell::new(false);

        let error = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            None,
            || 20,
            |path, candidate| {
                profile::save_config(path, candidate)?;
                Err(LambError::IndeterminatePublication {
                    operation: Box::new(LambError::Control(
                        "injected post-publication uncertainty".to_string(),
                    )),
                })
            },
            |_, _| {
                cleanup_called.set(true);
                Ok(Vec::new())
            },
        )
        .unwrap_err();

        assert!(matches!(error, LambError::IndeterminatePublication { .. }));
        let runtime = ctx.runtime.lock().unwrap();
        assert_eq!(runtime.config, config);
        assert_eq!(runtime.active_profile, before_active_profile);
        assert!(Arc::ptr_eq(
            runtime.session.as_ref().unwrap(),
            &before_session
        ));
        drop(runtime);
        assert_eq!(session.policy.lock().unwrap().activity, before_policy);
        assert!(!cleanup_called.get());
        let uncertain_disk: app_config::AppConfig =
            toml::from_str(&fs::read_to_string(&ctx.config_path).unwrap()).unwrap();
        assert!(uncertain_disk.profiles["studio"].channels["mic"]
            .activity
            .is_none());
        assert_ne!(uncertain_disk, config);
    }

    #[test]
    fn reset_retains_candidate_references_across_profiles_and_cleanup_error_is_warning() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        profile::create_profile(&mut config, "other", "jack").unwrap();
        profile::set_profile_field(&mut config, "other", "clientName", "lamb").unwrap();
        profile::set_profile_field(&mut config, "other", "buffer.seconds", "1").unwrap();
        profile::set_profile_field(
            &mut config,
            "other",
            "export.outputDir",
            temp.path().to_str().unwrap(),
        )
        .unwrap();
        profile::set_profile_field(&mut config, "other", "export.mode", "per-channel").unwrap();
        profile::set_profile_field(&mut config, "other", "export.format", "wav").unwrap();
        profile::add_capture_port(&mut config, "other", "system:capture_2", "room").unwrap();
        let studio = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let other = profile::validate_profile("other", &config.profiles["other"]).unwrap();
        let studio_input = configured_identity(&studio, "mic").unwrap();
        let other_input = configured_identity(&other, "room").unwrap();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -45.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 1,
                    input_id: studio_input.input_id().to_string(),
                    calibration_id: Some("removed".to_string()),
                }),
            },
        );
        config.profiles.get_mut("other").unwrap().channels.insert(
            "room".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -55.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 2,
                    input_id: other_input.input_id().to_string(),
                    calibration_id: Some("retained-manual".to_string()),
                }),
            },
        );
        let ctx = threshold_mutation_context(temp.path(), config, None, None);

        let (message, report) = mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            None,
            || 3,
            profile::save_config,
            |_, referenced| {
                assert_eq!(
                    referenced,
                    &std::collections::BTreeSet::from([(
                        other_input.input_id().to_string(),
                        "retained-manual".to_string(),
                    )])
                );
                Err(LambError::Control("injected cleanup failure".to_string()))
            },
        )
        .unwrap();

        assert!(message.contains("cleanup warning"));
        assert!(message.contains("injected cleanup failure"));
        assert!(report.channels[0].stored.is_none());
        assert!(
            ctx.runtime.lock().unwrap().config.profiles["studio"].channels["mic"]
                .activity
                .is_none()
        );
    }

    #[test]
    fn active_commit_acquires_every_fallible_runtime_and_policy_guard_before_save() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let session = threshold_capture_session(&active, 100);
        let ctx =
            threshold_mutation_context(temp.path(), config, Some(active), Some(session.clone()));

        mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-30.0),
            || 7,
            |path, candidate| {
                assert!(ctx.runtime.try_lock().is_err());
                assert!(session.policy.try_lock().is_err());
                profile::save_config(path, candidate)
            },
            |_, _| unreachable!(),
        )
        .unwrap();
    }

    #[test]
    fn committed_threshold_keeps_a_failed_frozen_retry_and_changes_the_next_new_range() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = threshold_test_config(
            temp.path(),
            &[
                ("mic", "system:capture_1"),
                ("room", "system:capture_2"),
                ("silent", "system:capture_3"),
            ],
        );
        profile::set_profile_field(&mut config, "studio", "buffer.seconds", "6").unwrap();
        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .export
            .default_channel_mode = Some(crate::activity::ChannelExportMode::Auto);
        let initial = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        for (channel, threshold_dbfs) in [("mic", -30.0), ("room", -30.0), ("silent", -3.0)] {
            let input = configured_identity(&initial, channel).unwrap();
            config.profiles.get_mut("studio").unwrap().channels.insert(
                channel.to_string(),
                app_config::ProfileChannelConfig {
                    activity: Some(app_config::ActivityThresholdConfig {
                        threshold_dbfs,
                        threshold_source: crate::activity::ThresholdSource::Manual,
                        updated_at_unix_seconds: 1,
                        input_id: input.input_id().to_string(),
                        calibration_id: None,
                    }),
                },
            );
        }
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let (session, ingress) = threshold_capture_session_with_ingress(&active, 100);
        let ctx =
            threshold_mutation_context(temp.path(), config, Some(active), Some(session.clone()));
        let mut samples = Vec::with_capacity(270 * 3);
        for frame in 0..270 {
            samples.extend_from_slice(&[
                if frame >= 253 { 0.1 } else { 0.0 },
                if frame >= 255 { 0.1 } else { 0.0 },
                0.0,
            ]);
        }
        ingress.try_push_interleaved(&samples, 3).unwrap();

        let timestamp = "20260826T120000";
        let collision = temp.path().join(format!(
            "lamb-{timestamp}-mic-100Hz-000000052-000000270-part001.wav"
        ));
        fs::write(&collision, b"collision").unwrap();
        // The coordinator's freeze command is sequenced after the admitted ingress
        // slot, so this call is the deterministic completion barrier for the batch.
        let first_error = session
            .persist(ExportCommand::Recall, timestamp)
            .unwrap_err();
        assert!(first_error.to_string().contains("already exists"));
        let frozen_before = session
            .coordinator
            .pending_frozen_decision_for_test()
            .unwrap()
            .unwrap();
        assert_eq!(
            frozen_before.channels,
            vec![
                crate::activity::FrozenChannelDecision::retained(
                    crate::activity::ChannelExportMode::Auto,
                    crate::activity::ActivityResult::Active,
                    Some(252),
                ),
                crate::activity::FrozenChannelDecision::retained(
                    crate::activity::ChannelExportMode::Auto,
                    crate::activity::ActivityResult::Active,
                    Some(254),
                ),
                crate::activity::FrozenChannelDecision {
                    mode: crate::activity::ChannelExportMode::Auto,
                    result: crate::activity::ActivityResult::Inactive,
                    disposition: crate::activity::ChannelDisposition::Omit,
                    first_evidence_frame: None,
                },
            ]
        );
        assert_eq!(frozen_before.export_range, 52..270);
        assert_eq!(frozen_before.sample_rate, 100);
        assert!(frozen_before.valid);
        assert!(frozen_before.frozen_epoch.is_some());
        assert_ne!(frozen_before.storage_id, 0);

        mutate_threshold_with(
            &ctx,
            "studio",
            "mic",
            Some(-3.0),
            || 2,
            profile::save_config,
            |_, _| unreachable!(),
        )
        .unwrap();
        let frozen_after = session
            .coordinator
            .pending_frozen_decision_for_test()
            .unwrap()
            .unwrap();
        assert_eq!(frozen_after, frozen_before);

        fs::remove_file(&collision).unwrap();
        let retry = session.persist(ExportCommand::Recall, timestamp).unwrap();
        assert_eq!(
            retry,
            DumpOutcome::Written {
                range: crate::dump::FrameRange { start: 0, end: 270 },
                frames: 270,
                export_start_frame: 52,
                export_frames: 218,
                losses: crate::dump::LossBreakdown::default(),
                output_directory: temp.path().to_path_buf(),
                files: vec![
                    temp.path().join(format!(
                        "lamb-{timestamp}-mic-100Hz-000000052-000000270-part001.wav"
                    )),
                    temp.path().join(format!(
                        "lamb-{timestamp}-room-100Hz-000000052-000000270-part001.wav"
                    )),
                ],
            }
        );

        ingress.try_push_interleaved(&samples, 3).unwrap();
        let next_timestamp = "20260826T120001";
        // The next persist command is likewise the exact barrier for this batch.
        let next = session
            .persist(ExportCommand::Recall, next_timestamp)
            .unwrap();
        assert_eq!(
            next,
            DumpOutcome::Written {
                range: crate::dump::FrameRange {
                    start: 270,
                    end: 540,
                },
                frames: 270,
                export_start_frame: 324,
                export_frames: 216,
                losses: crate::dump::LossBreakdown::default(),
                output_directory: temp.path().to_path_buf(),
                files: vec![temp.path().join(format!(
                    "lamb-{next_timestamp}-room-100Hz-000000324-000000540-part001.wav"
                ))],
            }
        );
    }

    #[test]
    fn set_rejects_every_non_finite_and_out_of_range_value_server_side() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let ctx = threshold_mutation_context(temp.path(), config, None, None);
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -120.1, 0.1] {
            let error = set_threshold(&ctx, "studio", "mic", value).unwrap_err();
            assert!(error
                .to_string()
                .contains("finite and within [-120.0, 0.0]"));
        }
    }

    #[test]
    fn threshold_report_distinguishes_active_idle_from_another_profile_capture() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("calibration");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let studio = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();

        let idle = threshold_report(&config, "studio", Some(&studio), None, &root, 10).unwrap();
        assert!(idle.active_profile);
        assert!(!idle.capturing);
        assert!(idle.channels[0].current_live_identity.is_none());
        assert_eq!(
            idle.channels[0].calibration_evaluation,
            CalibrationEvaluation::NotResolved
        );

        config
            .profiles
            .insert("other".to_string(), config.profiles["studio"].clone());
        let other = profile::validate_profile("other", &config.profiles["other"]).unwrap();
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(10),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 1,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let runtime = CaptureRuntime::build(params, 100, 1).unwrap().0;
        let other_session = CaptureSession::from_app_runtime(
            runtime,
            &other,
            100,
            jack_live_identities(&other).unwrap(),
        )
        .unwrap();
        let other_capture = threshold_report(
            &config,
            "studio",
            Some(&other),
            Some(&other_session),
            &root,
            10,
        )
        .unwrap();
        assert!(!other_capture.active_profile);
        assert!(!other_capture.capturing);
        assert!(other_capture.channels[0].current_live_identity.is_none());
        assert_eq!(other_capture.channels[0].configured_identity_matches, None);
    }

    #[test]
    fn threshold_report_uses_the_resolved_exact_zero_detector_contract() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("calibration");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .export
            .silence_policy = Some(crate::activity::SilencePolicyPreset::PerChannelExactZero);

        let report = threshold_report(&config, "studio", None, None, &root, 10).unwrap();
        assert_eq!(report.channels[0].detector, "exact-zero");
        assert_eq!(report.channels[0].detector_version, "exact-zero-v1");
    }

    #[test]
    fn startup_reconciliation_keeps_manual_and_calibrated_references_and_skips_invalid_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("calibration");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&resolved, "mic").unwrap();
        let input_root = root.join(input.input_id());
        for id in ["manual-retained", "orphan"] {
            fs::create_dir_all(input_root.join(id)).unwrap();
            fs::write(input_root.join(id).join("marker"), id).unwrap();
        }
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -50.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 10,
                    input_id: input.input_id().to_string(),
                    calibration_id: Some("manual-retained".to_string()),
                }),
            },
        );
        let loaded = app_config::LoadedAppConfig {
            config: config.clone(),
            state: app_config::ConfigLoadState::Loaded,
            error: None,
        };

        assert!(reconcile_startup_calibrations(&root, &loaded)
            .unwrap()
            .is_empty());
        assert!(input_root.join("manual-retained").exists());
        assert!(!input_root.join("orphan").exists());

        fs::create_dir_all(input_root.join("invalid-must-survive")).unwrap();
        let invalid = app_config::LoadedAppConfig {
            config: app_config::AppConfig::default(),
            state: app_config::ConfigLoadState::Invalid,
            error: Some("unparseable".to_string()),
        };
        assert!(reconcile_startup_calibrations(&root, &invalid)
            .unwrap()
            .is_empty());
        assert!(input_root.join("invalid-must-survive").exists());
    }

    #[test]
    fn inactive_threshold_report_inspects_artifacts_in_profile_port_order_without_live_validity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("calibration");
        let mut config = threshold_test_config(
            temp.path(),
            &[
                ("right", "system:capture_2"),
                ("left", "system:capture_1"),
                ("dry", "system:capture_3"),
            ],
        );
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let right = configured_identity(&resolved, "right").unwrap();
        let left = configured_identity(&resolved, "left").unwrap();
        let left_live = ResolvedLiveInputIdentity::new(
            crate::calibration::InputBackend::Jack,
            crate::calibration::LiveDeviceKeyKind::JackSourceClient,
            "system",
            "system:capture_1",
        )
        .unwrap();
        let store = crate::calibration::CalibrationStore::new(&root, 100).unwrap();
        let mut prepared = store
            .prepare(
                &left,
                &left_live,
                "left-complete",
                &[0.25],
                100,
                -50.0,
                &mut [0.001],
                &[0.1],
                100,
            )
            .unwrap();
        prepared.mark_authoritative();
        let channels = &mut config.profiles.get_mut("studio").unwrap().channels;
        channels.insert(
            "right".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -45.0,
                    threshold_source: crate::activity::ThresholdSource::Manual,
                    updated_at_unix_seconds: 90,
                    input_id: right.input_id().to_string(),
                    calibration_id: None,
                }),
            },
        );
        channels.insert(
            "left".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -50.0,
                    threshold_source: crate::activity::ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 100,
                    input_id: left.input_id().to_string(),
                    calibration_id: Some("left-complete".to_string()),
                }),
            },
        );

        let report = threshold_report(&config, "studio", None, None, &root, 110).unwrap();
        assert_eq!(
            report
                .channels
                .iter()
                .map(|channel| channel.channel.as_str())
                .collect::<Vec<_>>(),
            vec!["right", "left", "dry"]
        );
        assert_eq!(
            report.channels[0].artifact_status,
            CalibrationReportStatus::NotApplicable
        );
        assert_eq!(
            report.channels[0].calibration_evaluation,
            CalibrationEvaluation::Valid
        );
        assert_eq!(report.channels[0].effective_threshold_dbfs, Some(-45.0));
        assert_eq!(
            report.channels[1].artifact_status,
            CalibrationReportStatus::Complete
        );
        assert_eq!(
            report.channels[1].calibration_evaluation,
            CalibrationEvaluation::NotResolved
        );
        assert_eq!(report.channels[1].effective_threshold_dbfs, None);
        assert_eq!(
            report.channels[1].stored.as_ref().unwrap().age_seconds,
            Some(10)
        );
        assert_eq!(
            report.channels[2].artifact_status,
            CalibrationReportStatus::NotConfigured
        );
        assert_eq!(
            report.channels[2].calibration_evaluation,
            CalibrationEvaluation::NotResolved
        );
        assert_eq!(report.channels[2].effective_threshold_dbfs, None);

        let expired = threshold_report(
            &config,
            "studio",
            None,
            None,
            &root,
            100 + 30 * 24 * 60 * 60 + 1,
        )
        .unwrap();
        assert_eq!(
            expired.channels[1].artifact_status,
            CalibrationReportStatus::Stale {
                reason: crate::calibration::StaleReason::Expired,
            }
        );
        assert_eq!(
            expired.channels[1].calibration_evaluation,
            CalibrationEvaluation::NotResolved
        );

        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("left")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
            .calibration_id = Some("missing-generation".to_string());
        let missing = threshold_report(&config, "studio", None, None, &root, 110).unwrap();
        assert_eq!(
            missing.channels[1].artifact_status,
            CalibrationReportStatus::Stale {
                reason: crate::calibration::StaleReason::MissingState,
            }
        );

        config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("left")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
            .calibration_id = Some("left-complete".to_string());
        let mut stale_metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(prepared.metadata_path()).unwrap()).unwrap();
        stale_metadata["detector_version"] = serde_json::json!("foreign-detector-v99");
        fs::write(
            prepared.metadata_path(),
            serde_json::to_vec_pretty(&stale_metadata).unwrap(),
        )
        .unwrap();
        let detector_mismatch =
            threshold_report(&config, "studio", None, None, &root, 110).unwrap();
        assert_eq!(
            detector_mismatch.channels[1].artifact_status,
            CalibrationReportStatus::Stale {
                reason: crate::calibration::StaleReason::DetectorMismatch,
            }
        );
        assert_eq!(
            detector_mismatch.channels[1].detector_version,
            crate::activity::WINDOWED_RMS_PEAK_DETECTOR_VERSION
        );

        fs::write(prepared.metadata_path(), b"{").unwrap();
        let corrupt = threshold_report(&config, "studio", None, None, &root, 110).unwrap();
        assert_eq!(
            corrupt.channels[1].artifact_status,
            CalibrationReportStatus::Stale {
                reason: crate::calibration::StaleReason::CorruptMetadata,
            }
        );
        assert_eq!(
            corrupt.channels[1].calibration_evaluation,
            CalibrationEvaluation::NotResolved
        );
    }

    #[test]
    fn active_threshold_report_validates_exact_session_identity_rate_and_manual_diagnostics() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("calibration");
        let mut config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let input = configured_identity(&resolved, "mic").unwrap();
        let live = jack_live_identities(&resolved).unwrap()[0].clone().unwrap();
        let store = crate::calibration::CalibrationStore::new(&root, 100).unwrap();
        let mut prepared = store
            .prepare(
                &input,
                &live,
                "active-valid",
                &[0.25],
                100,
                -50.0,
                &mut [0.001],
                &[0.1],
                100,
            )
            .unwrap();
        prepared.mark_authoritative();
        config.profiles.get_mut("studio").unwrap().channels.insert(
            "mic".to_string(),
            app_config::ProfileChannelConfig {
                activity: Some(app_config::ActivityThresholdConfig {
                    threshold_dbfs: -50.0,
                    threshold_source: crate::activity::ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 100,
                    input_id: input.input_id().to_string(),
                    calibration_id: Some("active-valid".to_string()),
                }),
            },
        );
        let resolved = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(10),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 1,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let runtime = || CaptureRuntime::build(params, 100, 1).unwrap().0;
        let mut session =
            CaptureSession::from_app_runtime(runtime(), &resolved, 100, vec![Some(live.clone())])
                .unwrap();

        let report = threshold_report(
            &config,
            "studio",
            Some(&resolved),
            Some(&session),
            &root,
            110,
        )
        .unwrap();
        assert_eq!(
            report.channels[0].artifact_status,
            CalibrationReportStatus::Complete
        );
        assert_eq!(report.channels[0].configured_identity_matches, Some(true));
        assert_eq!(
            report.channels[0].calibration_evaluation,
            CalibrationEvaluation::Valid
        );
        assert_eq!(report.channels[0].effective_threshold_dbfs, Some(-50.0));
        assert_eq!(
            report.channels[0]
                .current_live_identity
                .as_ref()
                .unwrap()
                .key_value,
            "system"
        );

        session.resolved_live_inputs[0] = None;
        let no_live = threshold_report(
            &config,
            "studio",
            Some(&resolved),
            Some(&session),
            &root,
            110,
        )
        .unwrap();
        assert_eq!(
            no_live.channels[0].calibration_evaluation,
            CalibrationEvaluation::Stale {
                reason: crate::calibration::StaleReason::MissingLiveIdentity,
            }
        );

        session.resolved_live_inputs[0] = Some(
            ResolvedLiveInputIdentity::new(
                crate::calibration::InputBackend::Jack,
                crate::calibration::LiveDeviceKeyKind::JackSourceClient,
                "other",
                "other:capture_1",
            )
            .unwrap(),
        );
        let live_mismatch = threshold_report(
            &config,
            "studio",
            Some(&resolved),
            Some(&session),
            &root,
            110,
        )
        .unwrap();
        assert_eq!(
            live_mismatch.channels[0].calibration_evaluation,
            CalibrationEvaluation::Stale {
                reason: crate::calibration::StaleReason::LiveIdentityMismatch,
            }
        );

        session.configured_inputs[0] = ConfiguredInputIdentity::new(
            crate::calibration::InputBackend::Jack,
            crate::calibration::ConfiguredDeviceSelector::JackSourceClient("other".to_string()),
            "mic",
            "other:capture_1",
        )
        .unwrap();
        let configured_mismatch = threshold_report(
            &config,
            "studio",
            Some(&resolved),
            Some(&session),
            &root,
            110,
        )
        .unwrap();
        assert_eq!(
            configured_mismatch.channels[0].configured_identity_matches,
            Some(false)
        );
        assert_eq!(
            configured_mismatch.channels[0].calibration_evaluation,
            CalibrationEvaluation::Stale {
                reason: crate::calibration::StaleReason::InputMismatch,
            }
        );

        let threshold = config
            .profiles
            .get_mut("studio")
            .unwrap()
            .channels
            .get_mut("mic")
            .unwrap()
            .activity
            .as_mut()
            .unwrap();
        threshold.threshold_source = crate::activity::ThresholdSource::Manual;
        session.configured_inputs[0] = input;
        let manual = threshold_report(
            &config,
            "studio",
            Some(&resolved),
            Some(&session),
            &root,
            110,
        )
        .unwrap();
        assert!(manual.channels[0].current_live_identity.is_some());
        assert_eq!(
            manual.channels[0].calibration_evaluation,
            CalibrationEvaluation::Valid
        );
        assert_eq!(manual.channels[0].effective_threshold_dbfs, Some(-50.0));
    }

    #[test]
    fn app_session_identity_helpers_preserve_profile_port_order_and_no_durable_key() {
        use crate::calibration::{ConfiguredDeviceSelector, InputBackend, LiveDeviceKeyKind};

        let profile = profile::ResolvedProfile {
            name: "studio".to_string(),
            backend: "pipewire".to_string(),
            client_name: "lamb".to_string(),
            ports: vec![
                profile::ResolvedCapturePort {
                    name: "left".to_string(),
                    source: "source_FL".to_string(),
                },
                profile::ResolvedCapturePort {
                    name: "right".to_string(),
                    source: "source_FR".to_string(),
                },
            ],
            buffer_seconds: 1,
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: Some(PipeWireCaptureConfig {
                target: None,
                capture_ports: Vec::new(),
                sample_rate: 100,
                dont_remix: true,
                latency: None,
            }),
        };
        let target = ResolvedTarget {
            id: Some(77),
            name: "ephemeral-name".to_string(),
            description: None,
            channels: 2,
            sample_rate: 100,
            format: "F32LE".to_string(),
            source_ports: vec![
                crate::capture_pipewire::ResolvedSourcePort {
                    global_id: 1,
                    node_id: 77,
                    port_id: 3,
                    name: "resolved_FL".to_string(),
                },
                crate::capture_pipewire::ResolvedSourcePort {
                    global_id: 2,
                    node_id: 77,
                    port_id: 4,
                    name: "resolved_FR".to_string(),
                },
            ],
            durable_live_key: Some((LiveDeviceKeyKind::ObjectPath, "/devices/mic".to_string())),
        };

        let configured = configured_identities(&profile).unwrap();
        let live = pipewire_live_identities(&profile, &target).unwrap();
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0].name, "left");
        assert_eq!(configured[1].name, "right");
        assert_eq!(configured[0].backend, InputBackend::PipeWire);
        assert_eq!(
            configured[0].selector,
            ConfiguredDeviceSelector::PipeWireAuto
        );
        assert_eq!(
            live[0].as_ref().map(|identity| (
                &identity.key_kind,
                &identity.key_value,
                &identity.resolved_source
            )),
            Some((
                &LiveDeviceKeyKind::ObjectPath,
                &"/devices/mic".to_string(),
                &"resolved_FL".to_string()
            ))
        );
        assert_eq!(live[1].as_ref().unwrap().resolved_source, "resolved_FR");

        let no_key = ResolvedTarget {
            durable_live_key: None,
            ..target
        };
        assert_eq!(
            pipewire_live_identities(&profile, &no_key).unwrap(),
            vec![None, None]
        );
    }

    #[test]
    fn app_session_constructor_retains_startup_capacity_and_ordered_identities() {
        let profile = profile::ResolvedProfile {
            name: "studio".to_string(),
            backend: "jack".to_string(),
            client_name: "lamb".to_string(),
            ports: vec![
                profile::ResolvedCapturePort {
                    name: "right".to_string(),
                    source: "system:capture_2".to_string(),
                },
                profile::ResolvedCapturePort {
                    name: "left".to_string(),
                    source: "system:capture_1".to_string(),
                },
            ],
            buffer_seconds: 1,
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: None,
        };
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(10),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 30,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, _) = CaptureRuntime::build(params, 100, 2).unwrap();

        let session = CaptureSession::from_app_runtime(
            runtime,
            &profile,
            100,
            jack_live_identities(&profile).unwrap(),
        )
        .unwrap();

        assert_eq!(session.calibration_sample_frames, 3_000);
        assert_eq!(&*session.policy.lock().unwrap(), &profile.export_policy);
        assert_eq!(session.configured_inputs[0].name, "right");
        assert_eq!(session.configured_inputs[1].name, "left");
        assert_eq!(
            session.resolved_live_inputs[0]
                .as_ref()
                .map(|identity| (&identity.key_value, &identity.resolved_source)),
            Some((&"system".to_string(), &"system:capture_2".to_string()))
        );
        assert_eq!(
            session.resolved_live_inputs[1]
                .as_ref()
                .unwrap()
                .resolved_source,
            "system:capture_1"
        );
    }

    #[test]
    fn app_session_constructor_rejects_per_index_identity_incoherence() {
        use crate::calibration::{InputBackend, LiveDeviceKeyKind};

        let jack = profile::ResolvedProfile {
            name: "studio".to_string(),
            backend: "jack".to_string(),
            client_name: "lamb".to_string(),
            ports: vec![
                profile::ResolvedCapturePort {
                    name: "left".to_string(),
                    source: "system:capture_1".to_string(),
                },
                profile::ResolvedCapturePort {
                    name: "right".to_string(),
                    source: "other:capture_2".to_string(),
                },
            ],
            buffer_seconds: 1,
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: None,
        };
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(10),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 1,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let runtime = |channels| CaptureRuntime::build(params, 100, channels).unwrap().0;
        let valid = jack_live_identities(&jack).unwrap();

        let mut swapped = valid.clone();
        swapped.swap(0, 1);
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, swapped).is_err());

        let mut wrong_backend = valid.clone();
        wrong_backend[0].as_mut().unwrap().backend = InputBackend::PipeWire;
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, wrong_backend).is_err());

        let mut wrong_source = valid.clone();
        wrong_source[0].as_mut().unwrap().resolved_source = "system:capture_9".to_string();
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, wrong_source).is_err());

        let mut wrong_kind = valid.clone();
        wrong_kind[0].as_mut().unwrap().key_kind = LiveDeviceKeyKind::NodeName;
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, wrong_kind).is_err());

        let mut wrong_client = valid.clone();
        wrong_client[0].as_mut().unwrap().key_value = "unrelated".to_string();
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, wrong_client).is_err());

        let mut missing = valid;
        missing[0] = None;
        assert!(CaptureSession::from_app_runtime(runtime(2), &jack, 100, missing).is_err());

        let pipewire = profile::ResolvedProfile {
            name: "pipewire".to_string(),
            backend: "pipewire".to_string(),
            client_name: "lamb".to_string(),
            ports: vec![profile::ResolvedCapturePort {
                name: "mic".to_string(),
                source: "source_FL".to_string(),
            }],
            buffer_seconds: 1,
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: Some(PipeWireCaptureConfig {
                target: None,
                capture_ports: Vec::new(),
                sample_rate: 100,
                dont_remix: true,
                latency: None,
            }),
        };
        assert!(CaptureSession::from_app_runtime(runtime(1), &pipewire, 100, vec![None]).is_ok());
        let valid_pipewire = ResolvedLiveInputIdentity::new(
            InputBackend::PipeWire,
            LiveDeviceKeyKind::NodeName,
            "node",
            "source_FL",
        )
        .unwrap();
        assert!(CaptureSession::from_app_runtime(
            runtime(1),
            &pipewire,
            100,
            vec![Some(valid_pipewire.clone())],
        )
        .is_ok());
        let mut unrelated_pipewire = valid_pipewire.clone();
        unrelated_pipewire.resolved_source = "source_FR".to_string();
        assert!(CaptureSession::from_app_runtime(
            runtime(1),
            &pipewire,
            100,
            vec![Some(unrelated_pipewire)],
        )
        .is_err());
        let mut invalid_pipewire = valid_pipewire;
        invalid_pipewire.key_kind = LiveDeviceKeyKind::JackSourceClient;
        assert!(CaptureSession::from_app_runtime(
            runtime(1),
            &pipewire,
            100,
            vec![Some(invalid_pipewire)],
        )
        .is_err());
    }

    #[test]
    fn stop_capture_cancels_calibration_only_when_lane_handler_runs() {
        use std::sync::{mpsc, Barrier};

        let profile = profile::ResolvedProfile {
            name: "studio".to_string(),
            backend: "jack".to_string(),
            client_name: "lamb".to_string(),
            ports: vec![profile::ResolvedCapturePort {
                name: "mic".to_string(),
                source: "system:capture_1".to_string(),
            }],
            buffer_seconds: 1,
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: None,
        };
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(10),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 1,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, _ingress) = CaptureRuntime::build(params, 100, 1).unwrap();
        let session = Arc::new(
            CaptureSession::from_app_runtime(
                runtime,
                &profile,
                100,
                jack_live_identities(&profile).unwrap(),
            )
            .unwrap(),
        );
        let arena = Arc::clone(&session.arena);
        let ctx = Arc::new(IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: Some(profile),
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        });

        let admitted = Arc::new(Barrier::new(2));
        let admitted_capture = Arc::clone(&admitted);
        let (calibration_done_tx, calibration_done_rx) = mpsc::channel();
        let calibration = std::thread::spawn(move || {
            let first_check = AtomicBool::new(true);
            let result = arena.calibrate_channel_until(
                crate::capture_arena::CalibrationCaptureRequest {
                    channel: 0,
                    frames: 100,
                },
                Duration::from_secs(6),
                || {
                    if first_check.swap(false, Ordering::AcqRel) {
                        admitted_capture.wait();
                    }
                    false
                },
            );
            calibration_done_tx.send(result.map(|_| ())).unwrap();
        });
        admitted.wait();

        let lane = Arc::new(OperationLane::new(2).unwrap());
        let worker_entered = Arc::new(Barrier::new(2));
        let worker_release = Arc::new(Barrier::new(2));
        let stop_finished = Arc::new(Barrier::new(2));
        let jobs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entered = Arc::clone(&worker_entered);
        let release = Arc::clone(&worker_release);
        let finished = Arc::clone(&stop_finished);
        let worker_jobs = Arc::clone(&jobs);
        let worker_ctx = Arc::clone(&ctx);
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            64 * 1024,
            move |job: OperationJob| {
                let OperationJob::Client { request, stream } = job else {
                    return;
                };
                if worker_jobs.fetch_add(1, Ordering::AcqRel) == 0 {
                    entered.wait();
                    release.wait();
                } else {
                    let response = handle_idle_request(&worker_ctx, request);
                    write_response(stream, &response).unwrap();
                    finished.wait();
                }
            },
            |_job: OperationJob| {},
        );
        let (occupied_stream, _occupied_peer) = UnixStream::pair().unwrap();
        lane.try_enqueue(OperationJob::Client {
            request: ControlRequest::Reload,
            stream: occupied_stream,
        })
        .unwrap();
        worker_entered.wait();

        let (route_stream, mut route_peer) = UnixStream::pair().unwrap();
        writeln!(
            route_peer,
            "{}",
            serde_json::to_string(&ControlRequest::StopCapture).unwrap()
        )
        .unwrap();
        route_idle_stream(&ctx, &lane, route_stream).unwrap();

        let cancelled_before_worker_release = calibration_done_rx
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        assert!(ctx.runtime.lock().unwrap().session.is_some());
        worker_release.wait();
        stop_finished.wait();
        assert!(ctx.runtime.lock().unwrap().session.is_none());
        lane.close();
        worker.join().unwrap();
        if !cancelled_before_worker_release {
            calibration_done_rx.recv().unwrap().unwrap_err();
        }
        calibration.join().unwrap();
        assert!(
            !cancelled_before_worker_release,
            "StopCapture admission must not mutate capture before its queued handler runs"
        );
    }

    fn calibration_test_context(
        sample_rate: u32,
        maximum_seconds: u32,
        durable_live_key: bool,
    ) -> (Arc<IdleDaemonContext>, crate::capture_arena::CaptureIngress) {
        use crate::calibration::{InputBackend, LiveDeviceKeyKind};

        let profile = profile::ResolvedProfile {
            name: "studio".to_string(),
            backend: if durable_live_key { "jack" } else { "pipewire" }.to_string(),
            client_name: "lamb".to_string(),
            ports: vec![profile::ResolvedCapturePort {
                name: "mic".to_string(),
                source: "system:capture_1".to_string(),
            }],
            buffer_seconds: maximum_seconds.max(1),
            export_policy: test_policy(PathBuf::from("/tmp/out")),
            pipewire_config: (!durable_live_key).then_some(PipeWireCaptureConfig {
                target: None,
                capture_ports: Vec::new(),
                sample_rate,
                dont_remix: true,
                latency: None,
            }),
        };
        let capacity = sample_rate.checked_mul(maximum_seconds).unwrap();
        let params = CaptureRuntimeParams {
            seconds: maximum_seconds.max(1),
            chunk_frames_override: Some(capacity.max(1)),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: maximum_seconds,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, ingress) = CaptureRuntime::build(params, sample_rate, 1).unwrap();
        let live = durable_live_key.then(|| {
            ResolvedLiveInputIdentity::new(
                InputBackend::Jack,
                LiveDeviceKeyKind::JackSourceClient,
                "system",
                "system:capture_1",
            )
            .unwrap()
        });
        let session = Arc::new(
            CaptureSession::from_app_runtime(runtime, &profile, sample_rate, vec![live]).unwrap(),
        );
        let mut config = app_config::AppConfig::default();
        config
            .profiles
            .insert("studio".to_string(), app_config::ProfileConfig::default());
        (
            Arc::new(IdleDaemonContext {
                config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
                control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
                calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
                runtime: Mutex::new(AppRuntimeState {
                    config,
                    config_family: ConfigFamily::App,
                    prepared_legacy: None,
                    resolved_capture: None,
                    lifecycle: LifecycleState::ready_stopped(None),
                    state: "capturing".to_string(),
                    last_error: None,
                    config_load_error: None,
                    active_profile: Some(profile),
                    capture: None,
                    capture_health: None,
                    session: Some(session),
                    test_capture_attached: true,
                }),
                clock: Arc::new(SystemRetryClock::default()),
                scheduler: RetrySchedulerHandle::new(),
                operation_authority: Mutex::new(()),
                operation_epoch: AtomicU64::new(0),
                #[cfg(test)]
                final_mutation_pause: Mutex::new(None),
                stop: AtomicBool::new(false),
                first_fatal: std::sync::OnceLock::new(),
            }),
            ingress,
        )
    }

    fn observe_after_worker_acceptance(
        ctx: Arc<IdleDaemonContext>,
        ingress: &crate::capture_arena::CaptureIngress,
        seconds: u32,
        sample: f32,
    ) -> Result<()> {
        let arena = {
            let runtime = ctx.runtime.lock().unwrap();
            Arc::clone(&runtime.session.as_ref().unwrap().arena)
        };
        let (accepted, release) = arena.install_calibration_pause(false);
        let observer =
            std::thread::spawn(move || observe_app_calibration(&ctx, "studio", "mic", seconds));
        accepted.wait();
        let frames = usize::try_from(u64::from(seconds) * 3).unwrap();
        ingress.try_push_interleaved(&vec![sample; frames], 1)?;
        release.wait();
        observer.join().unwrap()
    }

    #[test]
    fn session_target_reports_exact_profile_channel_capture_and_live_key_errors() {
        let (ctx, _ingress) = calibration_test_context(3, 30, true);

        assert_eq!(
            session_calibration_target(&ctx, "undefined", "mic")
                .err()
                .unwrap()
                .to_string(),
            "configuration error: profile undefined does not exist"
        );
        {
            let mut runtime = ctx.runtime.lock().unwrap();
            runtime
                .config
                .profiles
                .insert("other".to_string(), app_config::ProfileConfig::default());
        }
        assert_eq!(
            session_calibration_target(&ctx, "other", "mic")
                .err()
                .unwrap()
                .to_string(),
            "control error: profile other is not active"
        );
        assert_eq!(
            session_calibration_target(&ctx, "studio", "missing")
                .err()
                .unwrap()
                .to_string(),
            "configuration error: profile studio has no channel missing"
        );
        let session = ctx.runtime.lock().unwrap().session.take();
        assert_eq!(
            session_calibration_target(&ctx, "studio", "mic")
                .err()
                .unwrap()
                .to_string(),
            "control error: profile studio is not capturing"
        );
        ctx.runtime.lock().unwrap().session = session;

        let (no_key, _ingress) = calibration_test_context(3, 30, false);
        assert_eq!(
            session_calibration_target(&no_key, "studio", "mic")
                .err()
                .unwrap()
                .to_string(),
            "control error: profile studio channel mic has no durable live key"
        );
    }

    #[test]
    fn observation_uses_exact_one_thirty_and_startup_capacity_arithmetic() {
        let (ctx, ingress) = calibration_test_context(3, 30, true);
        let target = session_calibration_target(&ctx, "studio", "mic").unwrap();
        assert_eq!(target.sample_rate, 3);
        assert_eq!(target.capacity, 90);

        assert!(observe_after_worker_acceptance(Arc::clone(&ctx), &ingress, 1, 0.25).is_ok());
        assert!(observe_after_worker_acceptance(Arc::clone(&ctx), &ingress, 30, 0.5).is_ok());
        assert!(matches!(
            observe_app_calibration(&ctx, "studio", "mic", 0),
            Err(LambError::Validation(_))
        ));
        assert!(matches!(
            observe_app_calibration(&ctx, "studio", "mic", 31),
            Err(LambError::Validation(_))
        ));
    }

    #[test]
    fn observation_is_future_only_and_preserves_frozen_and_session_topology() {
        let (ctx, ingress) = calibration_test_context(3, 30, true);
        ingress.try_push_interleaved(&[9.0, 9.0, 9.0], 1).unwrap();
        let session = ctx
            .runtime
            .lock()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .clone();
        let mut frozen = session
            .arena
            .freeze_since(None, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let frozen_range = frozen.absolute_range();
        let before = session.arena.status(Duration::from_secs(1)).unwrap();
        let configured_before = session.configured_inputs.clone();
        let live_before = session.resolved_live_inputs.clone();

        assert!(observe_after_worker_acceptance(Arc::clone(&ctx), &ingress, 1, 0.25).is_ok());

        let after = session.arena.status(Duration::from_secs(1)).unwrap();
        assert!(before.frozen_pending && after.frozen_pending);
        assert_eq!(frozen.absolute_range(), frozen_range);
        assert_eq!(after.capacity_frames, before.capacity_frames);
        assert_eq!(session.channel_count, 1);
        assert_eq!(session.configured_inputs, configured_before);
        assert_eq!(session.resolved_live_inputs, live_before);
        session
            .arena
            .release_frozen(&mut frozen, Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn initially_stopped_calibration_is_rejected_before_arena_admission() {
        let fixture = calibrated_replacement_fixture();
        fixture.ctx.stop.store(true, Ordering::Release);

        let error = calibrate_threshold_with(
            &fixture.ctx,
            "studio",
            "mic",
            1,
            || 1_000,
            &mut |_| panic!("initial rejection must precede generation durability"),
            &mut |_, _| panic!("initial rejection must not clean generations"),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "capture error: calibration target is not currently capturing and healthy"
        );
        assert!(!fixture.session.arena.cancel_calibration());
        assert_old_authority_after_definite_calibration_failure(&fixture, &[]);

        fixture.ctx.stop.store(false, Ordering::Release);
        assert!(observe_after_worker_acceptance(
            Arc::clone(&fixture.ctx),
            &fixture.ingress,
            1,
            0.25,
        )
        .is_ok());
        assert_old_authority_after_definite_calibration_failure(&fixture, &[]);
    }

    #[test]
    fn initially_invalid_calibration_target_matrix_never_admits_arena_work() {
        for invalidity in ["health", "attachment", "state", "identity"] {
            let fixture = calibrated_replacement_fixture();
            {
                let mut runtime = fixture.ctx.runtime.lock().unwrap();
                match invalidity {
                    "health" => {
                        let health = PipeWireHealth::default();
                        assert!(health.record_fatal("pre-published capture fault"));
                        runtime.capture_health = Some(health);
                    }
                    "attachment" => runtime.test_capture_attached = false,
                    "state" => runtime.state = "idle".to_string(),
                    "identity" => {
                        let active = runtime.active_profile.as_mut().unwrap();
                        active.client_name = "replacement-client".to_string();
                        active.ports[0].source = "other:capture_9".to_string();
                    }
                    _ => unreachable!(),
                }
            }
            let runtime_before = {
                let runtime = fixture.ctx.runtime.lock().unwrap();
                (
                    runtime.state.clone(),
                    runtime.active_profile.clone(),
                    runtime
                        .capture_health
                        .as_ref()
                        .and_then(PipeWireHealth::fault),
                    runtime.test_capture_attached,
                )
            };

            let error = calibrate_threshold_with(
                &fixture.ctx,
                "studio",
                "mic",
                1,
                || 1_000,
                &mut |_| panic!("initial rejection must precede generation durability"),
                &mut |_, _| panic!("initial rejection must not clean generations"),
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "capture error: calibration target is not currently capturing and healthy",
                "invalidity {invalidity}"
            );
            assert!(!fixture.session.arena.cancel_calibration());
            assert_eq!(
                fs::read(&fixture.ctx.config_path).unwrap(),
                fixture.old_disk
            );
            assert_eq!(generation_names(&fixture), vec![fixture.old_id.clone()]);
            assert_eq!(
                fixture.session.policy.lock().unwrap().activity,
                fixture.old_policy
            );
            {
                let runtime = fixture.ctx.runtime.lock().unwrap();
                assert_eq!(runtime.config, fixture.old_config);
                assert!(Arc::ptr_eq(
                    runtime.session.as_ref().unwrap(),
                    &fixture.session
                ));
                assert!(runtime.capture.is_none());
                assert_eq!(
                    (
                        runtime.state.clone(),
                        runtime.active_profile.clone(),
                        runtime
                            .capture_health
                            .as_ref()
                            .and_then(PipeWireHealth::fault),
                        runtime.test_capture_attached,
                    ),
                    runtime_before
                );
            }

            {
                let mut runtime = fixture.ctx.runtime.lock().unwrap();
                runtime.state = "capturing".to_string();
                runtime.capture_health = None;
                runtime.test_capture_attached = true;
                runtime.active_profile = Some(fixture.old_active_profile.clone());
            }
            assert!(observe_after_worker_acceptance(
                Arc::clone(&fixture.ctx),
                &fixture.ingress,
                1,
                0.25,
            )
            .is_ok());
            assert_old_authority_after_definite_calibration_failure(&fixture, &[]);
        }
    }

    #[test]
    fn live_calibration_definite_failure_matrix_preserves_exact_old_authority() {
        use crate::calibration::DurabilityCheckpoint::{
            ConfigParentSynced, ConfigRenamed, ConfigTempSynced, GenerationDirectorySynced,
            InputDirectorySynced, MetadataSynced, MetadataWritten, RootDirectorySynced,
            SampleSynced, SampleWritten,
        };

        let checkpoints = [
            SampleWritten,
            SampleSynced,
            MetadataWritten,
            MetadataSynced,
            GenerationDirectorySynced,
            InputDirectorySynced,
            RootDirectorySynced,
            ConfigTempSynced,
            ConfigRenamed,
            ConfigParentSynced,
        ];
        for checkpoint in checkpoints {
            let fixture = calibrated_replacement_fixture();
            let cleanup_calls = Arc::new(Mutex::new(Vec::new()));
            let recorded_cleanup = Arc::clone(&cleanup_calls);
            let error = run_calibrated_replacement(
                &fixture,
                move |seen| {
                    if seen == checkpoint {
                        Err(LambError::Control(format!(
                            "injected definite failure at {checkpoint:?}"
                        )))
                    } else {
                        Ok(())
                    }
                },
                move |path, _| {
                    recorded_cleanup.lock().unwrap().push(path.to_path_buf());
                    Ok(())
                },
            )
            .unwrap_err();

            // Catches a checkpoint silently ignored by either generation or config persistence.
            assert!(error
                .to_string()
                .contains(&format!("injected definite failure at {checkpoint:?}")));
            let cleanup_paths = cleanup_calls.lock().unwrap().clone();
            assert_old_authority_after_definite_calibration_failure(&fixture, &cleanup_paths);
        }
    }

    #[test]
    fn live_calibration_indeterminate_publication_preserves_prepared_generation_only() {
        let fixture = calibrated_replacement_fixture();
        let config_path = fixture.ctx.config_path.clone();
        let moved_candidate = config_path.with_file_name("candidate-moved-away.toml");
        let foreign_disk = b"foreign publication authority\n".to_vec();
        let hook_foreign_disk = foreign_disk.clone();
        let cleanup_calls = Arc::new(Mutex::new(Vec::new()));
        let recorded_cleanup = Arc::clone(&cleanup_calls);

        let error = run_calibrated_replacement(
            &fixture,
            move |checkpoint| {
                if checkpoint == crate::calibration::DurabilityCheckpoint::ConfigRenamed {
                    fs::rename(&config_path, &moved_candidate).unwrap();
                    fs::write(&config_path, &hook_foreign_disk).unwrap();
                    return Err(LambError::Control(
                        "injected post-publication identity disturbance".to_string(),
                    ));
                }
                Ok(())
            },
            move |path, _| {
                recorded_cleanup.lock().unwrap().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap_err();

        // Catches downgrade of uncertain publication into an ordinary rollback claim.
        assert!(matches!(error, LambError::IndeterminatePublication { .. }));
        let runtime = fixture.ctx.runtime.lock().unwrap();
        // Catches runtime config installation despite unknown disk authority.
        assert_eq!(runtime.config, fixture.old_config);
        // Catches resolved-profile installation despite unknown disk authority.
        assert_eq!(
            runtime.active_profile.as_ref(),
            Some(&fixture.old_active_profile)
        );
        // Catches session replacement/loss on the indeterminate command path.
        assert!(Arc::ptr_eq(
            runtime.session.as_ref().unwrap(),
            &fixture.session
        ));
        drop(runtime);
        // Catches live-policy installation when the candidate's publication is uncertain.
        assert_eq!(
            fixture.session.policy.lock().unwrap().activity,
            fixture.old_policy
        );
        // Catches any attempt to clean old authority while publication is unresolved.
        assert!(!cleanup_calls
            .lock()
            .unwrap()
            .iter()
            .any(|path| path == &fixture.old_path));
        // Catches damage to the still-authoritative old generation during uncertainty handling.
        assert_eq!(
            fs::read(fixture.old_path.join("sample.wav")).unwrap(),
            fixture.old_sample
        );
        assert_eq!(
            fs::read(fixture.old_path.join("metadata.json")).unwrap(),
            fixture.old_metadata
        );
        use std::os::unix::fs::MetadataExt;
        let old_metadata = fs::metadata(&fixture.old_path).unwrap();
        // Catches replacement of the old generation identity during uncertain publication.
        assert_eq!(
            (old_metadata.dev(), old_metadata.ino()),
            fixture.old_identity
        );
        // Catches PreparedCalibrationGeneration::Drop deleting reconciliation evidence.
        let generations = generation_names(&fixture);
        assert_eq!(generations.len(), 2);
        assert!(generations.contains(&fixture.old_id));
        // This deliberately asserts foreign current bytes, not rollback of uncertain disk state.
        assert_eq!(fs::read(&fixture.ctx.config_path).unwrap(), foreign_disk);
    }

    #[test]
    fn live_calibration_cleanup_refusal_or_error_is_success_with_pending_warning() {
        for refusal in [false, true] {
            let fixture = calibrated_replacement_fixture();
            let old_path = fixture.old_path.clone();
            let preserved_old = old_path.with_file_name(if refusal {
                "old-authoritative-preserved"
            } else {
                "unused-preserved-path"
            });
            let hook_old_path = old_path.clone();
            let hook_preserved_old = preserved_old.clone();
            let (message, report) = run_calibrated_replacement(
                &fixture,
                |_| Ok(()),
                move |path, checkpoint| {
                    if path == hook_old_path
                        && checkpoint == crate::calibration::CleanupCheckpoint::IdentityCaptured
                    {
                        if refusal {
                            fs::rename(path, &hook_preserved_old).unwrap();
                            fs::create_dir(path).unwrap();
                            fs::write(path.join("foreign"), b"foreign generation").unwrap();
                            return Ok(());
                        }
                        return Err(LambError::Control(
                            "injected old cleanup hook error".to_string(),
                        ));
                    }
                    Ok(())
                },
            )
            .unwrap();

            // Catches cleanup Pending being promoted to a failed calibrated command.
            assert!(message.contains("threshold calibrated; old calibration cleanup pending:"));
            let runtime = fixture.ctx.runtime.lock().unwrap();
            let installed = runtime.config.clone();
            let installed_profile = runtime.active_profile.clone().unwrap();
            let new_id = report.channels[0]
                .stored
                .as_ref()
                .unwrap()
                .calibration_id
                .as_ref()
                .unwrap()
                .clone();
            // Catches a cleanup warning corrupting command-boundary active/capturing truth.
            assert!(report.active_profile && report.capturing);
            // Catches a successful calibration report retaining the old/manual source kind.
            assert_eq!(
                report.channels[0].stored.as_ref().unwrap().source,
                crate::activity::ThresholdSource::Calibrated
            );
            // Catches report reconstruction using stale policy instead of the captured frames.
            assert_eq!(
                report.channels[0].effective_threshold_dbfs,
                Some(-2.041200637817383)
            );
            // Catches report/config divergence after the durable commit boundary.
            assert_eq!(
                installed.profiles["studio"].channels["mic"]
                    .activity
                    .as_ref()
                    .unwrap()
                    .calibration_id
                    .as_deref(),
                Some(new_id.as_str())
            );
            // Catches active-profile assignment lagging behind authoritative runtime config.
            assert_eq!(
                installed_profile,
                profile::validate_profile("studio", &installed.profiles["studio"]).unwrap()
            );
            // Catches session replacement while only its future policy should change.
            assert!(Arc::ptr_eq(
                runtime.session.as_ref().unwrap(),
                &fixture.session
            ));
            drop(runtime);
            // Catches disk/runtime disagreement after cleanup reports Pending.
            let persisted: app_config::AppConfig =
                toml::from_str(&fs::read_to_string(&fixture.ctx.config_path).unwrap()).unwrap();
            assert_eq!(persisted, installed);
            // Catches the live policy retaining the old calibrated generation.
            assert_eq!(
                fixture.session.policy.lock().unwrap().activity.channels[0]
                    .threshold
                    .as_ref()
                    .unwrap()
                    .calibration_id
                    .as_deref(),
                Some(new_id.as_str())
            );
            // Catches cleanup of the new authoritative generation after handler return/Drop.
            assert!(fixture
                .ctx
                .calibration_root
                .join(&fixture.input_id)
                .join(&new_id)
                .exists());
            if refusal {
                // Catches deletion of either the foreign replacement or identity-refused old inode.
                assert_eq!(
                    fs::read(old_path.join("foreign")).unwrap(),
                    b"foreign generation"
                );
                assert_eq!(
                    fs::read(preserved_old.join("sample.wav")).unwrap(),
                    fixture.old_sample
                );
            } else {
                // Catches swallowing the warning by nevertheless deleting the hook-failed old path.
                assert_eq!(
                    fs::read(old_path.join("sample.wav")).unwrap(),
                    fixture.old_sample
                );
            }
        }
    }

    #[test]
    fn live_calibration_success_removes_old_and_leaves_one_referenced_generation() {
        let fixture = calibrated_replacement_fixture();
        fixture
            .ingress
            .try_push_interleaved(&[0.5, 0.5, 0.5], 1)
            .unwrap();
        let mut frozen = fixture
            .session
            .arena
            .freeze_since(None, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let frozen_range = frozen.absolute_range().clone();
        let session_before = Arc::clone(&fixture.session);
        let arena_before = Arc::clone(&fixture.session.arena);
        let coordinator_before = Arc::clone(&fixture.session.coordinator);
        let workspace_before = std::ptr::addr_of!(fixture.session.workspace) as usize;
        let runtime_id_before = fixture.session.arena.runtime_id();
        let status_before = fixture
            .session
            .arena
            .status(Duration::from_secs(1))
            .unwrap();
        let pending_decision_before = fixture
            .session
            .coordinator
            .pending_frozen_decision_for_test()
            .unwrap();
        let configured_before = fixture.session.configured_inputs.clone();
        let live_before = fixture.session.resolved_live_inputs.clone();
        assert_eq!(
            configured_before[0].backend,
            crate::calibration::InputBackend::Jack
        );
        assert_eq!(
            live_before[0].as_ref().unwrap().backend,
            crate::calibration::InputBackend::Jack
        );
        let topology_before = (
            fixture.session.sample_rate,
            fixture.session.channel_count,
            fixture.session.profile_name.clone(),
            fixture.session.calibration_sample_frames,
        );
        let runtime_authority_before = {
            let runtime = fixture.ctx.runtime.lock().unwrap();
            (
                runtime.state.clone(),
                runtime.last_error.clone(),
                runtime.capture.is_none(),
                runtime.capture_health.is_none(),
            )
        };

        let (message, report) =
            run_calibrated_replacement(&fixture, |_| Ok(()), |_, _| Ok(())).unwrap();

        // Catches ordinary identity-safe cleanup being misreported as pending.
        assert_eq!(message, "threshold calibrated");
        let new_id = report.channels[0]
            .stored
            .as_ref()
            .unwrap()
            .calibration_id
            .as_ref()
            .unwrap()
            .clone();
        // Catches success leaving stale owned generations beside the latest reference.
        assert_eq!(generation_names(&fixture), vec![new_id.clone()]);
        // Catches the old path surviving an ordinary identity-safe cleanup.
        assert!(!fixture.old_path.exists());
        let runtime = fixture.ctx.runtime.lock().unwrap();
        let installed_session = runtime.session.as_ref().unwrap();
        assert!(Arc::ptr_eq(installed_session, &session_before));
        assert!(Arc::ptr_eq(&installed_session.arena, &arena_before));
        assert!(Arc::ptr_eq(
            &installed_session.coordinator,
            &coordinator_before
        ));
        assert_eq!(
            std::ptr::addr_of!(installed_session.workspace) as usize,
            workspace_before
        );
        assert_eq!(installed_session.arena.runtime_id(), runtime_id_before);
        assert_eq!(installed_session.configured_inputs, configured_before);
        assert_eq!(installed_session.resolved_live_inputs, live_before);
        assert_eq!(
            (
                installed_session.sample_rate,
                installed_session.channel_count,
                installed_session.profile_name.clone(),
                installed_session.calibration_sample_frames,
            ),
            topology_before
        );
        assert_eq!(
            (
                runtime.state.clone(),
                runtime.last_error.clone(),
                runtime.capture.is_none(),
                runtime.capture_health.is_none(),
            ),
            runtime_authority_before
        );
        // Catches the sole retained generation not being the runtime-authoritative reference.
        assert_eq!(
            runtime.config.profiles["studio"].channels["mic"]
                .activity
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some(new_id.as_str())
        );
        let installed_config = runtime.config.clone();
        let installed_profile = runtime.active_profile.clone().unwrap();
        assert_eq!(
            installed_profile,
            profile::validate_profile("studio", &installed_config.profiles["studio"]).unwrap()
        );
        assert_eq!(
            calibration_references(&installed_config),
            std::collections::BTreeSet::from([(fixture.input_id.clone(), new_id.clone(),)])
        );
        let installed_policy = installed_session.policy.lock().unwrap().clone();
        assert_eq!(installed_policy, installed_profile.export_policy);
        let expected_report = threshold_report(
            &installed_config,
            "studio",
            Some(&installed_profile),
            Some(installed_session),
            &fixture.ctx.calibration_root,
            1_000,
        )
        .unwrap();
        assert_eq!(report, expected_report);
        drop(runtime);

        let persisted: app_config::AppConfig =
            toml::from_str(&fs::read_to_string(&fixture.ctx.config_path).unwrap()).unwrap();
        assert_eq!(persisted, installed_config);
        let metadata_path = fixture
            .ctx
            .calibration_root
            .join(&fixture.input_id)
            .join(&new_id)
            .join("metadata.json");
        let metadata: crate::calibration::CalibrationMetadata =
            serde_json::from_slice(&fs::read(metadata_path).unwrap()).unwrap();
        assert_eq!(metadata.version, 2);
        assert_eq!(metadata.calibration_id, new_id);
        assert_eq!(
            metadata.configured_input,
            Some(configured_before[0].clone())
        );
        assert_eq!(metadata.resolved_live_input, live_before[0].clone());
        assert_eq!(metadata.input_id, fixture.input_id);
        assert_eq!(
            metadata.detector_version,
            crate::activity::WINDOWED_RMS_PEAK_DETECTOR_VERSION
        );
        assert_eq!(metadata.threshold_dbfs, -2.0412006);
        assert_eq!(metadata.p95_rms, 0.25);
        assert_eq!(metadata.observed_peak, 0.25);
        assert_eq!(metadata.dropped_frames, 0);
        assert_eq!(metadata.frames, 3);
        assert_eq!(metadata.sample_rate, 3);
        assert_eq!(metadata.created_at_unix_seconds, 1_000);

        let status_after = fixture
            .session
            .arena
            .status(Duration::from_secs(1))
            .unwrap();
        assert_eq!(status_before.capacity_frames, 3);
        assert_eq!(status_after.capacity_frames, status_before.capacity_frames);
        assert!(status_before.frozen_pending && status_after.frozen_pending);
        assert_eq!(frozen.absolute_range(), frozen_range);
        assert_eq!(
            fixture
                .session
                .coordinator
                .pending_frozen_decision_for_test()
                .unwrap(),
            pending_decision_before
        );
        fixture
            .session
            .arena
            .release_frozen(&mut frozen, Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn live_calibration_prepares_metadata_v2_then_commits_config_and_future_policy() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let active = profile::validate_profile("studio", &config.profiles["studio"]).unwrap();
        let (session, ingress) = threshold_capture_session_with_ingress(&active, 3);
        let ctx = Arc::new(threshold_mutation_context(
            temp.path(),
            config,
            Some(active),
            Some(Arc::clone(&session)),
        ));
        let arena = Arc::clone(&session.arena);
        let (accepted, release) = arena.install_calibration_pause(false);
        let transaction_ctx = Arc::clone(&ctx);
        let transaction = std::thread::spawn(move || {
            calibrate_threshold_with(
                &transaction_ctx,
                "studio",
                "mic",
                1,
                || 1_000,
                &mut |_| Ok(()),
                &mut |_, _| Ok(()),
            )
        });
        accepted.wait();
        ingress
            .try_push_interleaved(&[0.25, 0.25, 0.25], 1)
            .unwrap();
        release.wait();

        let (message, report) = transaction.join().unwrap().unwrap();
        assert_eq!(message, "threshold calibrated");
        let stored = report.channels[0].stored.as_ref().unwrap();
        assert_eq!(stored.source, crate::activity::ThresholdSource::Calibrated);
        assert_eq!(stored.updated_at_unix_seconds, 1_000);
        assert_eq!(
            report.channels[0].effective_threshold_dbfs,
            Some(-2.041200637817383)
        );
        let calibration_id = stored.calibration_id.as_ref().unwrap();
        let persisted: app_config::AppConfig =
            toml::from_str(&fs::read_to_string(&ctx.config_path).unwrap()).unwrap();
        assert_eq!(persisted, ctx.runtime.lock().unwrap().config);
        assert_eq!(
            persisted.profiles["studio"].channels["mic"]
                .activity
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some(calibration_id.as_str())
        );
        let metadata: crate::calibration::CalibrationMetadata = serde_json::from_slice(
            &fs::read(
                ctx.calibration_root
                    .join(
                        persisted.profiles["studio"].channels["mic"]
                            .activity
                            .as_ref()
                            .unwrap()
                            .input_id
                            .as_str(),
                    )
                    .join(calibration_id)
                    .join("metadata.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata.version, 2);
        assert_eq!(metadata.created_at_unix_seconds, 1_000);
        assert_eq!(metadata.frames, 3);
        assert_eq!(metadata.sample_rate, 3);
        assert_eq!(
            session.policy.lock().unwrap().activity.channels[0]
                .threshold
                .as_ref()
                .unwrap()
                .calibration_id
                .as_deref(),
            Some(calibration_id.as_str())
        );
    }

    #[test]
    fn direct_stop_during_prepared_generation_aborts_before_config_commit() {
        use std::sync::Barrier;

        let fixture = calibrated_replacement_fixture();
        fixture
            .ingress
            .try_push_interleaved(&[0.5, 0.5, 0.5], 1)
            .unwrap();
        let mut frozen = fixture
            .session
            .arena
            .freeze_since(None, Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let frozen_range = frozen.absolute_range().clone();
        let arena = Arc::clone(&fixture.session.arena);
        let coordinator = Arc::clone(&fixture.session.coordinator);
        let runtime_id = arena.runtime_id();
        let configured = fixture.session.configured_inputs.clone();
        let live = fixture.session.resolved_live_inputs.clone();
        assert_eq!(
            configured[0].backend,
            crate::calibration::InputBackend::Jack
        );
        assert_eq!(
            live[0].as_ref().unwrap().backend,
            crate::calibration::InputBackend::Jack
        );
        let topology = (
            fixture.session.sample_rate,
            fixture.session.channel_count,
            fixture.session.profile_name.clone(),
            fixture.session.calibration_sample_frames,
        );
        let pending_decision = coordinator.pending_frozen_decision_for_test().unwrap();
        let status_before = arena.status(Duration::from_secs(1)).unwrap();

        let (capture_accepted, capture_release) = arena.install_calibration_pause(false);
        let preparation_entered = Arc::new(Barrier::new(2));
        let preparation_release = Arc::new(Barrier::new(2));
        let hook_entered = Arc::clone(&preparation_entered);
        let hook_release = Arc::clone(&preparation_release);
        let cleanup_calls = Arc::new(Mutex::new(Vec::new()));
        let recorded_cleanup = Arc::clone(&cleanup_calls);
        let transaction_ctx = Arc::clone(&fixture.ctx);
        let transaction = std::thread::spawn(move || {
            calibrate_threshold_with(
                &transaction_ctx,
                "studio",
                "mic",
                1,
                || 1_000,
                &mut |checkpoint| {
                    if checkpoint == crate::calibration::DurabilityCheckpoint::RootDirectorySynced {
                        hook_entered.wait();
                        hook_release.wait();
                    }
                    Ok(())
                },
                &mut |path, _| {
                    recorded_cleanup.lock().unwrap().push(path.to_path_buf());
                    Ok(())
                },
            )
        });
        capture_accepted.wait();
        fixture
            .ingress
            .try_push_interleaved(&[0.25, 0.25, 0.25], 1)
            .unwrap();
        capture_release.wait();
        preparation_entered.wait();

        let response = handle_idle_request(&fixture.ctx, ControlRequest::Stop);
        assert!(response.ok);
        assert_eq!(response.message, "stopping");
        assert!(fixture.ctx.stop.load(Ordering::Acquire));
        preparation_release.wait();
        let error = transaction.join().unwrap().unwrap_err();
        assert!(error
            .to_string()
            .contains("calibration target changed before commit"));

        let cleanup_paths = cleanup_calls.lock().unwrap().clone();
        assert_old_authority_after_definite_calibration_failure(&fixture, &cleanup_paths);
        let runtime = fixture.ctx.runtime.lock().unwrap();
        let installed = runtime.session.as_ref().unwrap();
        assert!(Arc::ptr_eq(installed, &fixture.session));
        assert!(Arc::ptr_eq(&installed.arena, &arena));
        assert!(Arc::ptr_eq(&installed.coordinator, &coordinator));
        assert_eq!(installed.arena.runtime_id(), runtime_id);
        assert_eq!(installed.configured_inputs, configured);
        assert_eq!(installed.resolved_live_inputs, live);
        assert_eq!(
            (
                installed.sample_rate,
                installed.channel_count,
                installed.profile_name.clone(),
                installed.calibration_sample_frames,
            ),
            topology
        );
        assert_eq!(runtime.state, "capturing");
        assert!(runtime.last_error.is_none());
        assert!(runtime.capture.is_none());
        assert!(runtime.capture_health.is_none());
        drop(runtime);
        let status_after = arena.status(Duration::from_secs(1)).unwrap();
        assert_eq!(status_after.capacity_frames, status_before.capacity_frames);
        assert!(status_before.frozen_pending && status_after.frozen_pending);
        assert_eq!(frozen.absolute_range(), frozen_range);
        assert_eq!(
            coordinator.pending_frozen_decision_for_test().unwrap(),
            pending_decision
        );
        arena
            .release_frozen(&mut frozen, Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn out_of_range_calibration_derivation_creates_no_candidate_or_generation() {
        let fixture = calibrated_replacement_fixture();
        let cleanup_calls = Arc::new(Mutex::new(Vec::new()));
        let recorded_cleanup = Arc::clone(&cleanup_calls);
        let error = run_calibrated_replacement_with_samples(
            &fixture,
            [2.0, 2.0, 2.0],
            |_| Ok(()),
            move |path, _| {
                recorded_cleanup.lock().unwrap().push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("derived calibration threshold is outside [-120, 0] dBFS"));
        assert_old_authority_after_definite_calibration_failure(
            &fixture,
            &cleanup_calls.lock().unwrap(),
        );
        assert!(!fixture.ctx.stop.load(Ordering::Acquire));
        let runtime = fixture.ctx.runtime.lock().unwrap();
        let installed = runtime.session.as_ref().unwrap();
        assert!(Arc::ptr_eq(installed, &fixture.session));
        assert!(Arc::ptr_eq(&installed.arena, &fixture.session.arena));
        assert!(Arc::ptr_eq(
            &installed.coordinator,
            &fixture.session.coordinator
        ));
        assert_eq!(
            installed.configured_inputs,
            fixture.session.configured_inputs
        );
        assert_eq!(
            installed.resolved_live_inputs,
            fixture.session.resolved_live_inputs
        );
        assert_eq!(runtime.state, "capturing");
        assert!(runtime.capture.is_none());
        assert!(runtime.capture_health.is_none());
    }

    #[test]
    fn direct_stop_aborts_observation_and_status_remains_responsive() {
        use std::sync::Barrier;

        let (ctx, _ingress) = calibration_test_context(3, 30, true);
        let admitted = Arc::new(Barrier::new(2));
        let admitted_observer = Arc::clone(&admitted);
        let arena = ctx
            .runtime
            .lock()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .arena
            .clone();
        let observer = std::thread::spawn(move || {
            let first = AtomicBool::new(true);
            arena
                .calibrate_channel_until(
                    crate::capture_arena::CalibrationCaptureRequest {
                        channel: 0,
                        frames: 3,
                    },
                    Duration::from_secs(6),
                    || {
                        if first.swap(false, Ordering::AcqRel) {
                            admitted_observer.wait();
                        }
                        false
                    },
                )
                .map(|_| ())
        });
        admitted.wait();
        let status = handle_idle_request(&ctx, ControlRequest::Status);
        assert!(status.ok);
        let stop = handle_idle_request(&ctx, ControlRequest::Stop);
        assert!(stop.ok);
        assert!(matches!(
            observer.join().unwrap(),
            Err(LambError::CaptureInvariant(_))
        ));
    }

    #[test]
    fn observation_aborts_when_health_or_active_identity_changes() {
        for invalidate in ["health", "identity"] {
            let (ctx, _ingress) = calibration_test_context(3, 30, true);
            let arena = ctx
                .runtime
                .lock()
                .unwrap()
                .session
                .as_ref()
                .unwrap()
                .arena
                .clone();
            let (accepted, release) = arena.install_calibration_pause(false);
            let observer_ctx = Arc::clone(&ctx);
            let observer = std::thread::spawn(move || {
                observe_app_calibration(&observer_ctx, "studio", "mic", 1)
            });
            accepted.wait();
            if invalidate == "health" {
                let health = PipeWireHealth::default();
                assert!(health.record_fatal("capture health fault"));
                ctx.runtime.lock().unwrap().capture_health = Some(health);
            } else {
                ctx.runtime.lock().unwrap().active_profile = None;
            }
            release.wait();
            assert!(matches!(
                observer.join().unwrap(),
                Err(LambError::CaptureInvariant(_))
            ));
        }
    }

    #[test]
    fn observation_aborts_when_same_named_resolved_profile_is_replaced() {
        let (ctx, ingress) = calibration_test_context(3, 30, true);
        let arena = ctx
            .runtime
            .lock()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .arena
            .clone();
        let (accepted, release) = arena.install_calibration_pause(false);
        let observer_ctx = Arc::clone(&ctx);
        let observer =
            std::thread::spawn(move || observe_app_calibration(&observer_ctx, "studio", "mic", 1));
        accepted.wait();
        {
            let mut runtime = ctx.runtime.lock().unwrap();
            let mut replacement = runtime.active_profile.clone().unwrap();
            replacement.client_name = "replacement-client".to_string();
            replacement.ports[0].source = "other:capture_9".to_string();
            runtime.active_profile = Some(replacement);
        }
        ingress.try_push_interleaved(&[0.25; 3], 1).unwrap();
        release.wait();

        assert!(matches!(
            observer.join().unwrap(),
            Err(LambError::CaptureInvariant(_))
        ));
    }

    #[test]
    fn threshold_operations_queue_in_order_while_status_routes_directly() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let ctx = threshold_mutation_context(temp.path(), config, None, None);
        let lane = Arc::new(OperationLane::new(4).unwrap());
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (processed_tx, processed_rx) = mpsc::channel();
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            64 * 1024,
            move |job: OperationJob| {
                let OperationJob::Client { request, stream } = job else {
                    return;
                };
                if matches!(
                    request,
                    ControlRequest::Threshold {
                        request: ThresholdRequest::Calibrate { .. }
                    }
                ) {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap();
                }
                processed_tx.send(request).unwrap();
                write_response(
                    stream,
                    &ControlResponse {
                        ok: true,
                        message: "processed by operation worker".to_string(),
                        status: None,
                        error_context: crate::control::ControlErrorContext::default(),
                        persistence_outcome: None,
                        threshold_report: None,
                    },
                )
                .unwrap();
            },
            |_job: OperationJob| {},
        );

        let expected = [
            ControlRequest::Threshold {
                request: ThresholdRequest::Calibrate {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                    seconds: 5,
                },
            },
            ControlRequest::Threshold {
                request: ThresholdRequest::Set {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                    dbfs: -42.0,
                },
            },
            ControlRequest::Threshold {
                request: ThresholdRequest::Show {
                    profile: "studio".to_string(),
                },
            },
            ControlRequest::Threshold {
                request: ThresholdRequest::Reset {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                },
            },
        ];

        let mut queued_peers = Vec::new();
        queued_peers.push(route_test_request(&ctx, &lane, expected[0].clone()));
        entered_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap();
        for request in &expected[1..] {
            queued_peers.push(route_test_request(&ctx, &lane, request.clone()));
        }

        let status = read_test_response(route_test_request(&ctx, &lane, ControlRequest::Status));
        assert!(status.ok);
        assert_eq!(status.message, "status");
        assert_eq!(status.threshold_report, None);

        release_tx.send(()).unwrap();
        for request in expected {
            assert_eq!(
                processed_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap(),
                request
            );
        }
        for peer in queued_peers {
            assert_eq!(
                read_test_response(peer).message,
                "processed by operation worker"
            );
        }
        lane.close();
        join_worker_bounded(worker);
    }

    #[test]
    fn saturated_or_closed_threshold_lane_returns_the_bounded_busy_response() {
        let temp = tempfile::tempdir().unwrap();
        let config = threshold_test_config(temp.path(), &[("mic", "system:capture_1")]);
        let ctx = threshold_mutation_context(temp.path(), config, None, None);
        let lane = Arc::new(OperationLane::new(1).unwrap());
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let worker = spawn_operation_worker(
            Arc::clone(&lane),
            64 * 1024,
            move |job: OperationJob| {
                let OperationJob::Client { .. } = job else {
                    return;
                };
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap();
            },
            |_job: OperationJob| {},
        );

        let calibrate = ControlRequest::Threshold {
            request: ThresholdRequest::Calibrate {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                seconds: 5,
            },
        };
        let _calibrate_peer = route_test_request(&ctx, &lane, calibrate);
        entered_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap();
        let _queued_peer = route_test_request(
            &ctx,
            &lane,
            ControlRequest::Threshold {
                request: ThresholdRequest::Set {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                    dbfs: -42.0,
                },
            },
        );
        let full = read_test_response(route_test_request(
            &ctx,
            &lane,
            ControlRequest::Threshold {
                request: ThresholdRequest::Show {
                    profile: "studio".to_string(),
                },
            },
        ));
        assert!(!full.ok);
        assert_eq!(full.message, "operation queue is busy or shutting down");

        lane.close();
        let closed = read_test_response(route_test_request(
            &ctx,
            &lane,
            ControlRequest::Threshold {
                request: ThresholdRequest::Reset {
                    profile: "studio".to_string(),
                    channel: "mic".to_string(),
                },
            },
        ));
        assert!(!closed.ok);
        assert_eq!(closed.message, "operation queue is busy or shutting down");
        release_tx.send(()).unwrap();
        join_worker_bounded(worker);
    }

    fn test_legacy_idle_context(
        capture_health: Option<PipeWireHealth>,
        stopped: bool,
    ) -> IdleDaemonContext {
        let mut lifecycle = LifecycleState::ready_stopped(None);
        lifecycle.mark_running(None, None);
        IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::Legacy,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle,
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health,
                session: Some(Arc::new(test_session())),
                test_capture_attached: false,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(stopped),
            first_fatal: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn every_legacy_threshold_operation_returns_the_exact_unsupported_response() {
        let ctx = test_legacy_idle_context(None, false);
        let requests = [
            ThresholdRequest::Calibrate {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                seconds: 5,
            },
            ThresholdRequest::Set {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                dbfs: -42.0,
            },
            ThresholdRequest::Show {
                profile: "studio".to_string(),
            },
            ThresholdRequest::Reset {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
            },
        ];
        for request in requests {
            let response = handle_idle_request(&ctx, ControlRequest::Threshold { request });
            assert!(!response.ok);
            assert_eq!(
                response.message,
                "profile threshold commands are unsupported for legacy configuration"
            );
            assert_eq!(response.threshold_report, None);
        }
    }

    #[test]
    fn app_threshold_errors_do_not_fall_back_to_legacy_and_calibrate_rechecks_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let ctx =
            threshold_mutation_context(temp.path(), app_config::AppConfig::default(), None, None);
        let requests = [
            ThresholdRequest::Calibrate {
                profile: "missing".to_string(),
                channel: "mic".to_string(),
                seconds: 5,
            },
            ThresholdRequest::Set {
                profile: "missing".to_string(),
                channel: "mic".to_string(),
                dbfs: -42.0,
            },
            ThresholdRequest::Show {
                profile: "missing".to_string(),
            },
            ThresholdRequest::Reset {
                profile: "missing".to_string(),
                channel: "mic".to_string(),
            },
        ];
        for request in requests {
            let response = handle_app_threshold(&ctx, request);
            assert!(!response.ok);
            assert_ne!(
                response.message,
                "profile threshold commands are unsupported for legacy configuration"
            );
        }
        for seconds in [0, 31] {
            let response = handle_app_threshold(
                &ctx,
                ThresholdRequest::Calibrate {
                    profile: "missing".to_string(),
                    channel: "mic".to_string(),
                    seconds,
                },
            );
            assert!(!response.ok);
            assert_eq!(
                response.message,
                "validation error: calibration seconds must be within 1..=30"
            );
        }
    }

    #[test]
    fn pipewire_runtime_fault_changes_legacy_and_profile_status_to_faulted() {
        let health = crate::capture_pipewire::PipeWireHealth::default();
        assert!(health.record_fatal("PipeWire core/proxy error: server disconnected"));

        let legacy = test_legacy_idle_context(Some(health.clone()), false);
        let profile = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: Some(health),
                session: None,
                test_capture_attached: false,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        };

        for status in [
            idle_status_response(&legacy),
            idle_status_response(&profile),
        ] {
            assert_eq!(status.state, "faulted");
            assert_eq!(
                status.last_error.as_deref(),
                Some("PipeWire core/proxy error: server disconnected")
            );
        }
    }

    #[test]
    fn status_releases_runtime_lock_and_uses_one_stop_snapshot_before_arena_status() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut session = test_session();
        session.status_hook = Some(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }));
        let ctx = test_legacy_idle_context(None, false);
        ctx.runtime.lock().unwrap().session = Some(Arc::new(session));
        let ctx = Arc::new(ctx);
        let status_ctx = Arc::clone(&ctx);
        let status_thread = std::thread::spawn(move || idle_status_response(&status_ctx));

        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("status did not reach session status hook");
        let runtime_lock_was_available = ctx.runtime.try_lock().is_ok();
        ctx.stop.store(true, Ordering::Release);
        release_tx.send(()).unwrap();
        let status = status_thread.join().unwrap();

        assert!(
            runtime_lock_was_available,
            "session.status must run after releasing the runtime mutex"
        );
        assert_eq!(
            (status.state.as_str(), status.lifecycle.daemon_state),
            ("capturing", DaemonState::Ready),
            "old and additive status must use the same pre-stop snapshot"
        );
    }

    #[test]
    fn normal_pipewire_stop_is_not_reported_as_a_fault() {
        let health = crate::capture_pipewire::PipeWireHealth::default();
        let legacy = test_legacy_idle_context(Some(health), true);

        let status = idle_status_response(&legacy);
        assert_eq!(status.state, "stopping");
        assert_eq!(status.last_error, None);
    }

    fn test_session() -> CaptureSession {
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(1),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, _) = CaptureRuntime::build(params, 100, 1).unwrap();
        CaptureSession {
            arena: Arc::new(runtime.arena),
            workspace: Mutex::new(runtime.workspace),
            coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
                runtime.frozen_export_decision,
            )),
            sample_rate: 100,
            channel_count: 1,
            profile_name: "legacy".to_string(),
            policy: Mutex::new(test_policy(PathBuf::from("/tmp/out"))),
            configured_inputs: Vec::new(),
            resolved_live_inputs: Vec::new(),
            calibration_sample_frames: 0,
            attempt_resource_probes: Vec::new(),
            status_hook: None,
        }
    }

    fn test_policy(output_dir: PathBuf) -> ResolvedExportPolicy {
        test_policy_with_layout(
            output_dir,
            crate::export_policy::ResolvedLayout::CommandDefault,
        )
    }

    fn test_policy_with_layout(
        output_dir: PathBuf,
        layout: crate::export_policy::ResolvedLayout,
    ) -> ResolvedExportPolicy {
        ResolvedExportPolicy::new(
            output_dir,
            layout,
            crate::export_policy::ResolvedActivityPolicy {
                detector: crate::activity::ActivityDetectorKind::ExactZero,
                channels: vec![crate::export_policy::ChannelActivityPolicy {
                    name: "mic".to_string(),
                    mode: crate::activity::ChannelExportMode::Always,
                    threshold: None,
                }],
                whole_export_exact_zero_gate: true,
                trim_leading_silence: false,
            },
        )
        .unwrap()
    }

    fn test_app_context(
        root: &Path,
        output_dir: PathBuf,
        layout: crate::export_policy::ResolvedLayout,
        samples: &[f32],
    ) -> IdleDaemonContext {
        test_app_context_with_policy(root, test_policy_with_layout(output_dir, layout), samples)
    }

    fn test_app_context_with_policy(
        root: &Path,
        policy: ResolvedExportPolicy,
        samples: &[f32],
    ) -> IdleDaemonContext {
        let channel_count = u32::try_from(policy.activity.channels.len()).unwrap();
        assert!(channel_count > 0, "test policy must contain a channel");
        let channel_count_usize = usize::try_from(channel_count).unwrap();
        assert_eq!(
            samples.len() % channel_count_usize,
            0,
            "interleaved samples must contain complete frames"
        );
        let frame_count = u64::try_from(samples.len() / channel_count_usize).unwrap();
        let output_dir = policy.output_dir().to_path_buf();
        fs::create_dir_all(&output_dir).unwrap();
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(100),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, ingress) = CaptureRuntime::build(params, 100, channel_count).unwrap();
        ingress
            .try_push_interleaved(samples, channel_count)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runtime
            .arena
            .status(std::time::Duration::from_secs(1))
            .unwrap()
            .worker_written_frames
            < frame_count
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let session = Arc::new(CaptureSession {
            arena: Arc::new(runtime.arena),
            workspace: Mutex::new(runtime.workspace),
            coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
                runtime.frozen_export_decision,
            )),
            sample_rate: 100,
            channel_count,
            profile_name: "configured-profile".to_string(),
            policy: Mutex::new(policy),
            configured_inputs: Vec::new(),
            resolved_live_inputs: Vec::new(),
            calibration_sample_frames: 0,
            attempt_resource_probes: Vec::new(),
            status_hook: None,
        });
        IdleDaemonContext {
            config_path: root.join("lamb.toml"),
            control_socket_path: root.join("control.sock"),
            calibration_root: root.join("calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        }
    }

    #[test]
    fn app_status_reports_pending_frozen_epoch() {
        let params = CaptureRuntimeParams {
            seconds: 1,
            chunk_frames_override: Some(100),
            memory_max: None,
            headroom: 1.0,
            split_when_over_bytes: 3_900_000_000,
            io_buffer_bytes_per_channel: 4096,
            maximum_path_bytes: 512,
            maximum_calibration_seconds: 0,
            capture_queue_slots: 8,
            capture_worker_stack_bytes: 64 * 1024,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
        };
        let (runtime, ingress) = CaptureRuntime::build(params, 100, 1).unwrap();
        ingress.try_push_interleaved(&[0.1, 0.2, 0.3], 1).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let status = runtime
                .arena
                .status(std::time::Duration::from_secs(1))
                .unwrap();
            if status.worker_written_frames >= 3 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "capture worker did not drain"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let frozen = runtime
            .arena
            .freeze_since(None, std::time::Duration::from_secs(1))
            .unwrap()
            .expect("three frames were written");

        let session = Arc::new(CaptureSession {
            arena: Arc::new(runtime.arena),
            workspace: Mutex::new(runtime.workspace),
            coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
                runtime.frozen_export_decision,
            )),
            sample_rate: 100,
            channel_count: 1,
            profile_name: "test".to_string(),
            policy: Mutex::new(test_policy(PathBuf::from("/tmp/out"))),
            configured_inputs: Vec::new(),
            resolved_live_inputs: Vec::new(),
            calibration_sample_frames: 0,
            attempt_resource_probes: Vec::new(),
            status_hook: None,
        });
        let ctx = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        };

        let status = idle_status_response(&ctx);
        assert_eq!(status.active_export_count, 1);
        let _ = frozen;
    }

    #[test]
    fn app_recall_reports_poisoned_runtime_lock_as_internal_error() {
        let ctx = poisoned_runtime_context();

        let response = handle_app_recall(&ctx);

        assert!(!response.ok);
        assert_eq!(response.message, "runtime state lock poisoned");
    }

    #[test]
    fn app_dump_reports_poisoned_runtime_lock_as_internal_error() {
        let ctx = poisoned_runtime_context();

        let response = handle_app_dump(&ctx);

        assert!(!response.ok);
        assert_eq!(response.message, "runtime state lock poisoned");
    }

    #[test]
    fn poisoned_stop_capture_releases_ownership_backend_first_before_reporting_success() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut session = test_session();
        session.attempt_resource_probes = vec![Box::new(OrderedDropProbe {
            name: "session",
            order: Arc::clone(&order),
        })];
        let ctx = test_legacy_idle_context(None, false);
        {
            let mut runtime = ctx.runtime.lock().unwrap();
            runtime.config_family = ConfigFamily::App;
            runtime.state = "capturing".to_string();
            runtime.capture = Some(CaptureBackend::TestProbe(Box::new(OrderedDropProbe {
                name: "backend",
                order: Arc::clone(&order),
            })));
            runtime.session = Some(Arc::new(session));
            runtime.test_capture_attached = true;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _runtime = ctx.runtime.lock().unwrap();
            panic!("poison runtime lock with active capture");
        }));

        let response = handle_idle_request(&ctx, ControlRequest::StopCapture);

        assert!(response.ok, "stop response must reflect completed teardown");
        assert_eq!(response.message, "capture stopped");
        let status = response
            .status
            .expect("successful stop must include status");
        assert_eq!(status.state, "unconfigured");
        assert_eq!(status.lifecycle.daemon_state, DaemonState::Ready);
        assert_eq!(status.lifecycle.capture_state, CaptureState::Stopped);
        assert_eq!(status.lifecycle.error_class, None);
        assert_eq!(status.lifecycle.retry_policy, RetryPolicy::None);
        assert_eq!(status.lifecycle.retry_attempt, 0);
        assert_eq!(status.lifecycle.next_retry_at, None);
        assert_eq!(status.last_error, None);
        let runtime = ctx
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(runtime.capture.is_none());
        assert!(runtime.session.is_none());
        assert!(!runtime.test_capture_attached);
        drop(runtime);
        assert_eq!(*order.lock().unwrap(), vec!["backend", "session"]);
    }

    #[test]
    fn app_dump_writes_beneath_the_session_output_root() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("profile-output");
        let ctx = test_app_context(
            temp.path(),
            output_dir.clone(),
            crate::export_policy::ResolvedLayout::FlatDetailed,
            &[0.25, -0.5],
        );

        let response = handle_app_dump(&ctx);

        let (reported_directory, files) = match response.persistence_outcome {
            Some(PersistenceOutcomeResponse::Written {
                start_frame,
                end_frame,
                frames,
                export_start_frame,
                export_frames,
                output_directory,
                files,
                ..
            }) => {
                assert_eq!((start_frame, end_frame, frames), (0, 2, 2));
                assert_eq!((export_start_frame, export_frames), (0, 2));
                (output_directory, files)
            }
            outcome => panic!("app dump should write captured audio, got {outcome:?}"),
        };
        assert_eq!(reported_directory, output_dir);
        assert!(!files.is_empty());
        assert!(files
            .iter()
            .all(|path| { path.parent() == Some(output_dir.as_path()) && path.is_file() }));
    }

    #[test]
    fn app_recall_uses_explicit_timestamp_directory_layout() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("profile-output");
        let ctx = test_app_context(
            temp.path(),
            output_dir.clone(),
            crate::export_policy::ResolvedLayout::TimestampDirectory,
            &[0.25, -0.5],
        );

        let response = handle_app_recall(&ctx);

        let (reported_directory, files) = match response.persistence_outcome {
            Some(PersistenceOutcomeResponse::Written {
                start_frame,
                end_frame,
                frames,
                export_start_frame,
                export_frames,
                output_directory,
                files,
                ..
            }) => {
                assert_eq!((start_frame, end_frame, frames), (0, 2, 2));
                assert_eq!((export_start_frame, export_frames), (0, 2));
                (output_directory, files)
            }
            outcome => panic!("app recall should write captured audio, got {outcome:?}"),
        };
        assert_eq!(reported_directory.parent(), Some(output_dir.as_path()));
        let timestamp = reported_directory.file_name().unwrap().to_string_lossy();
        assert_eq!(timestamp.len(), 14);
        assert!(timestamp
            .chars()
            .all(|character| character.is_ascii_digit()));
        assert!(!files.is_empty());
        assert!(files
            .iter()
            .all(|path| { path.parent() == Some(reported_directory.as_path()) && path.is_file() }));
    }

    #[test]
    fn app_policy_and_silent_skips_succeed_consume_ranges_and_publish_no_paths() {
        let temp = tempfile::tempdir().unwrap();
        let policy_output = temp.path().join("policy-skip-output");
        let policy_ctx = test_app_context(
            temp.path(),
            policy_output.clone(),
            crate::export_policy::ResolvedLayout::FlatDetailed,
            &[0.25, -0.5],
        );
        let session = policy_ctx
            .runtime
            .lock()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .clone();
        session.policy.lock().unwrap().activity.channels[0].mode =
            crate::activity::ChannelExportMode::Never;

        let policy_response = handle_app_dump(&policy_ctx);
        assert!(policy_response.ok);
        match policy_response.persistence_outcome {
            Some(PersistenceOutcomeResponse::SkippedByPolicy {
                start_frame,
                end_frame,
                frames,
                ..
            }) => assert_eq!((start_frame, end_frame, frames), (0, 2, 2)),
            outcome => panic!("expected policy skip, got {outcome:?}"),
        }
        assert_eq!(fs::read_dir(&policy_output).unwrap().count(), 0);

        let silent_output = temp.path().join("silent-skip-output");
        let silent_ctx = test_app_context(
            temp.path(),
            silent_output.clone(),
            crate::export_policy::ResolvedLayout::TimestampDirectory,
            &[0.0, -0.0],
        );
        let silent_response = handle_app_recall(&silent_ctx);
        assert!(silent_response.ok);
        match silent_response.persistence_outcome {
            Some(PersistenceOutcomeResponse::SkippedSilent {
                start_frame,
                end_frame,
                frames,
                ..
            }) => assert_eq!((start_frame, end_frame, frames), (0, 2, 2)),
            outcome => panic!("expected silent skip, got {outcome:?}"),
        }
        assert_eq!(fs::read_dir(&silent_output).unwrap().count(), 0);
    }

    #[test]
    fn app_written_response_reports_only_existing_retained_channel_files() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("sparse-output");
        let policy = ResolvedExportPolicy::new(
            output_dir.clone(),
            crate::export_policy::ResolvedLayout::FlatDetailed,
            crate::export_policy::ResolvedActivityPolicy {
                detector: crate::activity::ActivityDetectorKind::ExactZero,
                channels: vec![
                    crate::export_policy::ChannelActivityPolicy {
                        name: "mic".to_string(),
                        mode: crate::activity::ChannelExportMode::Always,
                        threshold: None,
                    },
                    crate::export_policy::ChannelActivityPolicy {
                        name: "omitted".to_string(),
                        mode: crate::activity::ChannelExportMode::Never,
                        threshold: None,
                    },
                ],
                whole_export_exact_zero_gate: false,
                trim_leading_silence: false,
            },
        )
        .unwrap();
        let ctx = test_app_context_with_policy(temp.path(), policy, &[0.25, 0.75, -0.5, 0.5]);

        let response = handle_app_dump(&ctx);

        assert!(response.ok, "app dump failed: {}", response.message);
        let files = match response.persistence_outcome {
            Some(PersistenceOutcomeResponse::Written {
                start_frame,
                end_frame,
                frames,
                export_start_frame,
                export_frames,
                output_directory,
                files,
                ..
            }) => {
                assert_eq!((start_frame, end_frame, frames), (0, 2, 2));
                assert_eq!((export_start_frame, export_frames), (0, 2));
                assert_eq!(output_directory, output_dir);
                files
            }
            outcome => panic!("expected sparse written response, got {outcome:?}"),
        };
        assert_eq!(files.len(), 1, "only mic should be reported: {files:?}");
        let retained = &files[0];
        assert_eq!(retained.parent(), Some(output_dir.as_path()));
        assert!(retained.is_file());
        let retained_name = retained.file_name().unwrap().to_string_lossy();
        assert!(retained_name.starts_with("lamb-"), "{retained_name}");
        assert!(
            retained_name.contains("-mic-100Hz-000000000-000000002-part001.wav"),
            "{retained_name}"
        );
        assert!(!retained_name.contains("omitted"), "{retained_name}");
        let artifacts: Vec<_> = fs::read_dir(&output_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(artifacts, files);
    }

    fn poisoned_runtime_context() -> IdleDaemonContext {
        let ctx = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                config_family: ConfigFamily::App,
                prepared_legacy: None,
                resolved_capture: None,
                lifecycle: LifecycleState::ready_stopped(None),
                state: "idle".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: None,
                test_capture_attached: false,
            }),
            clock: Arc::new(SystemRetryClock::default()),
            scheduler: RetrySchedulerHandle::new(),
            operation_authority: Mutex::new(()),
            operation_epoch: AtomicU64::new(0),
            #[cfg(test)]
            final_mutation_pause: Mutex::new(None),
            stop: AtomicBool::new(false),
            first_fatal: std::sync::OnceLock::new(),
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _runtime = ctx.runtime.lock().unwrap();
            panic!("poison runtime lock");
        }));
        ctx
    }

    #[test]
    fn iso8601_compact_label_is_14_digits() {
        let label = iso8601_compact_label();
        assert_eq!(
            label.len(),
            14,
            "expected 14-digit ISO 8601 compact, got '{label}'"
        );
        assert!(
            label.chars().all(|c| c.is_ascii_digit()),
            "expected all digits, got '{label}'"
        );
    }

    #[test]
    fn iso8601_compact_label_is_monotonic() {
        let a = iso8601_compact_label();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let b = iso8601_compact_label();
        assert!(b > a, "timestamps must be monotonic: {a} then {b}");
    }

    #[test]
    fn staging_path_overflow_is_rejected_before_existing_final_path_is_touched() {
        let temp = tempfile::tempdir().unwrap();
        let mut parent = temp.path().to_path_buf();
        while parent.as_os_str().as_bytes().len() < 100 {
            parent.push("a");
        }
        fs::create_dir_all(&parent).unwrap();
        let final_path = parent.join("x");
        let final_socket = UnixDatagram::bind(&final_path).unwrap();
        let identity = SocketIdentity::from_metadata(&fs::symlink_metadata(&final_path).unwrap());

        let error = ControlSocketOwner::bind(final_path.clone()).unwrap_err();

        assert!(error.to_string().contains("sun_path"), "{error}");
        assert!(identity.matches(&fs::symlink_metadata(&final_path).unwrap()));
        drop(final_socket);
    }
}
