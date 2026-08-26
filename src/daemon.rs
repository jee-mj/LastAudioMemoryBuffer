use crate::app_config::{self, ConfigLoadState};
use crate::calibration::{ConfiguredInputIdentity, ResolvedLiveInputIdentity};
use crate::capture_arena::CaptureArenaStatus;
use crate::capture_fake::FakeCapture;
use crate::capture_jack::{JackCapture, JackCaptureConfig};
use crate::capture_pipewire::{
    PipeWireCapture, PipeWireCaptureConfig, PipeWireHealth, ResolvedTarget,
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
    write_persistence_response, CalibrationEvaluation, CalibrationReportStatus,
    ConfiguredInputReport, ControlRequest, ControlResponse, DaemonStatus, StoredThresholdReport,
    ThresholdChannelReport, ThresholdReport, ThresholdRequest,
};
use crate::control_server::{spawn_operation_worker, EnqueueError, OperationLane};
#[cfg(test)]
use crate::dump::DumpOutcome;
use crate::dump::{CommittedPersistenceRef, DumpCoordinator, PolicyPersistenceRequest};
use crate::error::{io_error, LambError, Result};
use crate::export_policy::{ExportCommand, ResolvedExportPolicy};
use crate::persistence_workspace::PersistenceWorkspace;
use crate::profile;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PERSIST_TIMEOUT: Duration = Duration::from_secs(60);
const APP_STAGING_ROOT: &str = "/tmp/LAMB/staging";

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
    fn from_app_runtime(
        runtime: CaptureRuntime,
        profile: &profile::ResolvedProfile,
        sample_rate: u32,
        resolved_live_inputs: Vec<Option<ResolvedLiveInputIdentity>>,
    ) -> Result<Self> {
        let configured_inputs = configured_identities(profile)?;
        let channel_count = u32::try_from(profile.ports.len())
            .map_err(|_| LambError::Capture("profile channel count exceeds u32".to_string()))?;
        if configured_inputs.len() != resolved_live_inputs.len()
            || configured_inputs.len() != profile.ports.len()
            || channel_count == 0
            || configured_inputs
                .iter()
                .zip(&resolved_live_inputs)
                .any(|(configured, live)| !session_input_is_coherent(configured, live.as_ref()))
        {
            return Err(LambError::Capture(
                "configured and resolved session input ordering is incoherent".to_string(),
            ));
        }
        let calibration_sample_frames = runtime.calibration_sample_frames();
        Ok(Self {
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
        })
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

pub fn run_from_config_path(path: &Path) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(text) if is_legacy_runtime_config(&text) => {
            let cfg = expand_runtime_paths(config::load_config_text(path, &text)?)?;
            run_capture_config(cfg)
        }
        Ok(_) => run_app_config_idle_from_path(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            run_app_config_idle_from_path(path)
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn run_app_config_idle_from_path(path: &Path) -> Result<()> {
    let loaded = app_config::load_optional_config(path)?;
    let calibration_root = crate::calibration::default_state_root()?;
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
    match loaded.state {
        ConfigLoadState::Loaded => run_app_config_daemon(path, loaded.config, calibration_root),
        ConfigLoadState::Missing | ConfigLoadState::Invalid => {
            let reason = match loaded.state {
                ConfigLoadState::Missing => format!("config file not found: {}", path.display()),
                ConfigLoadState::Invalid => loaded
                    .error
                    .unwrap_or_else(|| format!("invalid config file: {}", path.display())),
                _ => unreachable!(),
            };
            run_idle_fallback(
                path,
                loaded.config.daemon.control_socket_path,
                reason,
                calibration_root,
            )
        }
    }
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

fn run_idle_fallback(
    path: &Path,
    socket_template: String,
    reason: String,
    calibration_root: PathBuf,
) -> Result<()> {
    let control_socket_path = expand_control_socket_path(&socket_template)?;
    let listener = bind_control_socket(&control_socket_path)?;
    run_idle_fallback_on_listener(
        path.to_path_buf(),
        control_socket_path,
        reason,
        calibration_root,
        listener,
        |_| {},
    )
}

fn run_idle_fallback_on_listener<F>(
    config_path: PathBuf,
    control_socket_path: PathBuf,
    reason: String,
    calibration_root: PathBuf,
    listener: UnixListener,
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
        stop: AtomicBool::new(false),
    });

    run_idle_listener_with_hook(ctx, listener, before_operation)
}

fn is_legacy_runtime_config(text: &str) -> bool {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .as_table()
                .map(|table| table.contains_key("configVersion"))
        })
        .unwrap_or_else(|| {
            text.lines()
                .any(|line| line.trim_start().starts_with("configVersion"))
        })
}

fn run_capture_config(mut cfg: LambConfig) -> Result<()> {
    if std::env::var_os("LAMB_SKIP_RUNTIME_VALIDATION").is_none() {
        validate_runtime_environment(&cfg)?;
    }
    let params = legacy_runtime_params(&cfg);

    let mut resolved_target = None;
    let mut fake_capture = None;
    let mut pipewire_capture = None;
    let (sample_rate, _channel_names, runtime) = match cfg.backend.as_str() {
        "fake" => {
            let channels = cfg.channels.unwrap_or(2);
            let (runtime, ingress) = CaptureRuntime::build(params, cfg.sample_rate, channels)?;
            fake_capture = Some(FakeCapture::start(
                ingress,
                channels,
                cfg.chunk_frames.unwrap_or(25),
            )?);
            let channel_names = cfg.channel_map.clone().unwrap_or_default();
            (cfg.sample_rate, channel_names, runtime)
        }
        "pipewire" => {
            let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&cfg)?;
            let channel_names = pipewire_cfg.channel_names();
            let resolved = crate::capture_pipewire::resolve_target(&pipewire_cfg)?;
            eprintln!("lamb: {}", resolved.log_message());
            cfg.channels = Some(resolved.channels);
            cfg.sample_rate = resolved.sample_rate;
            resolved_target = Some(resolved.clone());
            let (capture, runtime) =
                PipeWireCapture::start_with_resolved(pipewire_cfg, resolved, params)?;
            pipewire_capture = Some(capture);
            (cfg.sample_rate, channel_names, runtime)
        }
        other => return Err(LambError::Capture(format!("unsupported backend {other}"))),
    };

    let parent = cfg
        .control_socket_path
        .parent()
        .ok_or_else(|| LambError::Control("control socket path has no parent".to_string()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    if cfg.control_socket_path.exists() {
        fs::remove_file(&cfg.control_socket_path)
            .map_err(|source| io_error(&cfg.control_socket_path, source))?;
    }
    let listener = UnixListener::bind(&cfg.control_socket_path)
        .map_err(|source| io_error(&cfg.control_socket_path, source))?;
    fs::set_permissions(&cfg.control_socket_path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(&cfg.control_socket_path, source))?;

    let policy = cfg.resolved_session_export_policy()?;
    let session = CaptureSession {
        arena: Arc::new(runtime.arena),
        workspace: Mutex::new(runtime.workspace),
        coordinator: Arc::new(DumpCoordinator::with_frozen_decision(
            runtime.frozen_export_decision,
        )),
        sample_rate,
        channel_count: cfg.channels.unwrap_or(2),
        profile_name: "legacy".to_string(),
        policy: Mutex::new(policy),
        configured_inputs: Vec::new(),
        resolved_live_inputs: Vec::new(),
        calibration_sample_frames: 0,
    };
    let _recovery = session.recover_startup(Path::new(APP_STAGING_ROOT));
    let ctx = Arc::new(DaemonContext {
        cfg,
        session,
        resolved_target,
        stop: AtomicBool::new(false),
        last_error: Mutex::new(None),
        capture_health: pipewire_capture.as_ref().map(PipeWireCapture::health),
    });
    let lane = Arc::new(OperationLane::new(DEFAULT_CONTROL_QUEUE_CAPACITY as usize)?);
    let worker = spawn_operation_worker(
        Arc::clone(&lane),
        DEFAULT_WORKER_STACK_BYTES as usize,
        {
            let ctx = Arc::clone(&ctx);
            move |(request, stream): (ControlRequest, UnixStream)| match request {
                ControlRequest::Recall => {
                    legacy_persistence_delivery(&ctx, ExportCommand::Recall, stream)
                }
                ControlRequest::Dump => {
                    legacy_persistence_delivery(&ctx, ExportCommand::Dump, stream)
                }
                request => {
                    let response = handle_request(&ctx, request);
                    let _ = write_response(stream, &response);
                }
            }
        },
        {
            let ctx = Arc::clone(&ctx);
            move |(_request, stream): (ControlRequest, UnixStream)| {
                let response = ControlResponse {
                    ok: false,
                    message: "shutting down".to_string(),
                    status: Some(status_response(&ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                };
                let _ = write_response(stream, &response);
            }
        },
    );
    listener
        .set_nonblocking(true)
        .map_err(|source| io_error(&ctx.cfg.control_socket_path, source))?;

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(err) = route_legacy_stream(&ctx, &lane, stream) {
                    if let Ok(mut last) = ctx.last_error.lock() {
                        *last = Some(err.to_string());
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) => {
                if let Ok(mut last) = ctx.last_error.lock() {
                    *last = Some(err.to_string());
                }
            }
        }
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    lane.close();
    let _ = worker.join();

    if let Some(capture) = fake_capture {
        capture.stop();
    }
    if let Some(capture) = pipewire_capture {
        capture.stop();
    }
    let _ = fs::remove_file(&ctx.cfg.control_socket_path);
    Ok(())
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

struct DaemonContext {
    cfg: LambConfig,
    session: CaptureSession,
    resolved_target: Option<ResolvedTarget>,
    stop: AtomicBool,
    last_error: Mutex<Option<String>>,
    capture_health: Option<PipeWireHealth>,
}

struct IdleDaemonContext {
    config_path: PathBuf,
    control_socket_path: PathBuf,
    calibration_root: PathBuf,
    runtime: Mutex<AppRuntimeState>,
    stop: AtomicBool,
}

struct AppRuntimeState {
    config: app_config::AppConfig,
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
            Some(CaptureBackend::Jack(capture, _)) => {
                target.active_profile.backend == "jack" && capture.sample_rate == target.sample_rate
            }
            Some(CaptureBackend::PipeWire(capture, _)) => {
                target.active_profile.backend == "pipewire"
                    && capture.sample_rate == target.sample_rate
            }
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
    Jack(JackCapture, Vec<String>),
    PipeWire(PipeWireCapture, Vec<String>),
}

impl CaptureBackend {
    fn runtime_error(&self) -> Option<String> {
        match self {
            Self::Jack(_, _) => None,
            Self::PipeWire(capture, _) => capture.runtime_error(),
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            CaptureBackend::Jack(c, _) => c.sample_rate,
            CaptureBackend::PipeWire(c, _) => c.sample_rate,
        }
    }
}

fn run_app_config_daemon(
    path: &Path,
    config: app_config::AppConfig,
    calibration_root: PathBuf,
) -> Result<()> {
    let control_socket_path = expand_control_socket_path(&config.daemon.control_socket_path)?;
    let listener = bind_control_socket(&control_socket_path)?;

    let mut state = AppRuntimeState {
        config,
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

    match reload_app_config_inner(&mut state, path, &calibration_root) {
        Ok(()) => {}
        Err(err) => {
            state.last_error = Some(err.to_string());
        }
    }

    let ctx = Arc::new(IdleDaemonContext {
        config_path: path.to_path_buf(),
        control_socket_path,
        calibration_root,
        runtime: Mutex::new(state),
        stop: AtomicBool::new(false),
    });
    run_idle_listener(ctx, listener)
}

fn run_idle_listener(ctx: Arc<IdleDaemonContext>, listener: UnixListener) -> Result<()> {
    run_idle_listener_with_hook(ctx, listener, |_| {})
}

fn run_idle_listener_with_hook<F>(
    ctx: Arc<IdleDaemonContext>,
    listener: UnixListener,
    before_operation: F,
) -> Result<()>
where
    F: Fn(&ControlRequest) + Send + 'static,
{
    let lane = Arc::new(OperationLane::new(DEFAULT_CONTROL_QUEUE_CAPACITY as usize)?);
    let worker = spawn_operation_worker(
        Arc::clone(&lane),
        DEFAULT_WORKER_STACK_BYTES as usize,
        {
            let ctx = Arc::clone(&ctx);
            move |(request, stream): (ControlRequest, UnixStream)| {
                before_operation(&request);
                match request {
                    ControlRequest::Recall => {
                        app_persistence_delivery(&ctx, ExportCommand::Recall, stream)
                    }
                    ControlRequest::Dump => {
                        app_persistence_delivery(&ctx, ExportCommand::Dump, stream)
                    }
                    request => {
                        let response = handle_idle_request(&ctx, request);
                        let _ = write_response(stream, &response);
                    }
                }
            }
        },
        {
            let ctx = Arc::clone(&ctx);
            move |(_request, stream): (ControlRequest, UnixStream)| {
                let response = ControlResponse {
                    ok: false,
                    message: "shutting down".to_string(),
                    status: Some(idle_status_response(&ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                };
                let _ = write_response(stream, &response);
            }
        },
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
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
    lane.close();
    worker
        .join()
        .map_err(|_| LambError::Control("operation worker panicked".to_string()))?;

    let _ = fs::remove_file(&ctx.control_socket_path);
    Ok(())
}

fn expand_control_socket_path(socket_path: &str) -> Result<PathBuf> {
    if socket_path.contains("%t") {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(|_| LambError::Validation("XDG_RUNTIME_DIR does not exist".to_string()))?;
        return Ok(PathBuf::from(socket_path.replace("%t", &runtime_dir)));
    }
    Ok(PathBuf::from(socket_path))
}

fn bind_control_socket(path: &Path) -> Result<UnixListener> {
    let parent = path
        .parent()
        .ok_or_else(|| LambError::Control("control socket path has no parent".to_string()))?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    if path.exists() {
        fs::remove_file(path).map_err(|source| io_error(path, source))?;
    }
    let listener = UnixListener::bind(path).map_err(|source| io_error(path, source))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))?;
    Ok(listener)
}

fn read_request(stream: UnixStream) -> Result<(ControlRequest, UnixStream)> {
    stream
        .set_read_timeout(Some(PERSIST_TIMEOUT))
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

/// Publishes shutdown before direct-handler cancellation/status work that may
/// need a runtime-owned lock. Routed Stop also closes admission first through
/// `publish_stop_and_close_before`.
fn publish_stop_before<F>(stop: &AtomicBool, lock_dependent_work: F)
where
    F: FnOnce(),
{
    stop.store(true, Ordering::Release);
    lock_dependent_work();
}

fn publish_stop_and_close_before<T, F>(
    stop: &AtomicBool,
    lane: &OperationLane<T>,
    lock_dependent_work: F,
) where
    F: FnOnce(),
{
    stop.store(true, Ordering::Release);
    lane.close();
    lock_dependent_work();
}

/// Routes one legacy control connection: status is answered directly (so it
/// stays responsive during persistence), stop sets the stop flag, and mutating
/// requests are transferred to the operation lane.
fn route_legacy_stream(
    ctx: &DaemonContext,
    lane: &OperationLane<(ControlRequest, UnixStream)>,
    stream: UnixStream,
) -> Result<()> {
    let (request, stream) = read_request(stream)?;
    match request {
        ControlRequest::Status => {
            let response = ControlResponse {
                ok: true,
                message: "status".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        ControlRequest::Stop => {
            publish_stop_and_close_before(&ctx.stop, lane, || {
                ctx.session.arena.cancel_calibration();
            });
            let response = ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        request => match lane.try_enqueue((request, stream)) {
            Ok(()) => Ok(()),
            Err((EnqueueError::Full | EnqueueError::Closed, (_, stream))) => {
                let response = ControlResponse {
                    ok: false,
                    message: "operation queue is busy or shutting down".to_string(),
                    status: Some(status_response(ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                };
                write_response(stream, &response)
            }
        },
    }
}

fn route_idle_stream(
    ctx: &IdleDaemonContext,
    lane: &OperationLane<(ControlRequest, UnixStream)>,
    stream: UnixStream,
) -> Result<()> {
    let (request, stream) = read_request(stream)?;
    match request {
        ControlRequest::Status => {
            let response = ControlResponse {
                ok: true,
                message: "status".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        ControlRequest::Stop => {
            publish_stop_and_close_before(&ctx.stop, lane, || cancel_active_calibration(ctx));
            let response = ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            };
            write_response(stream, &response)
        }
        ControlRequest::StopCapture => {
            cancel_active_calibration(ctx);
            match lane.try_enqueue((ControlRequest::StopCapture, stream)) {
                Ok(()) => Ok(()),
                Err((EnqueueError::Full | EnqueueError::Closed, (_, stream))) => {
                    let response = ControlResponse {
                        ok: false,
                        message: "operation queue is busy or shutting down".to_string(),
                        status: Some(idle_status_response(ctx)),
                        persistence_outcome: None,
                        threshold_report: None,
                    };
                    write_response(stream, &response)
                }
            }
        }
        request => match lane.try_enqueue((request, stream)) {
            Ok(()) => Ok(()),
            Err((EnqueueError::Full | EnqueueError::Closed, (_, stream))) => {
                let response = ControlResponse {
                    ok: false,
                    message: "operation queue is busy or shutting down".to_string(),
                    status: Some(idle_status_response(ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                };
                write_response(stream, &response)
            }
        },
    }
}

fn handle_idle_request(ctx: &IdleDaemonContext, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Status => ControlResponse {
            ok: true,
            message: "status".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
            threshold_report: None,
        },
        ControlRequest::Stop => {
            publish_stop_before(&ctx.stop, || cancel_active_calibration(ctx));
            ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
        ControlRequest::StartCapture { profile, activate } => {
            if let Some(response) = config_load_failure_response(ctx) {
                return response;
            }
            match start_app_capture(ctx, profile, activate) {
                Ok(message) => ControlResponse {
                    ok: true,
                    message,
                    status: Some(idle_status_response(ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                },
                Err(err) => {
                    set_app_last_error(ctx, err.to_string());
                    ControlResponse {
                        ok: false,
                        message: err.to_string(),
                        status: Some(idle_status_response(ctx)),
                        persistence_outcome: None,
                        threshold_report: None,
                    }
                }
            }
        }
        ControlRequest::StopCapture => {
            cancel_active_calibration(ctx);
            stop_app_capture(ctx);
            ControlResponse {
                ok: true,
                message: "capture stopped".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
        ControlRequest::Recall => persistence_delivery_only_response(idle_status_response(ctx)),
        ControlRequest::Clear => handle_app_clear(ctx),
        ControlRequest::Dump => persistence_delivery_only_response(idle_status_response(ctx)),
        ControlRequest::Reload => match reload_app_config(ctx) {
            Ok(()) => ControlResponse {
                ok: true,
                message: "config reloaded".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            },
            Err(err) => {
                set_app_last_error(ctx, err.to_string());
                ControlResponse {
                    ok: false,
                    message: err.to_string(),
                    status: Some(idle_status_response(ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                }
            }
        },
        ControlRequest::Threshold { request } => handle_app_threshold(ctx, request),
    }
}

fn cancel_active_calibration(ctx: &IdleDaemonContext) {
    if let Ok(runtime) = ctx.runtime.lock() {
        if let Some(session) = runtime.session.as_ref() {
            session.arena.cancel_calibration();
        }
    }
}

fn idle_status_response(ctx: &IdleDaemonContext) -> DaemonStatus {
    let runtime = ctx.runtime.lock().ok();
    let (
        state,
        last_error,
        resolved_target,
        sample_rate,
        channel_count,
        format,
        buffer_capacity,
        retained,
        dropped,
        frozen_pending,
    ) = if let Some(ref runtime) = runtime {
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
        let state = if ctx.stop.load(Ordering::Acquire) {
            "stopping".to_string()
        } else if capture_fault.is_some() {
            "faulted".to_string()
        } else {
            runtime.state.clone()
        };
        let last_error = capture_fault
            .or_else(|| runtime.config_load_error.clone())
            .or_else(|| runtime.last_error.clone());
        if let Some(session) = runtime.session.as_ref() {
            let (capacity, retained, dropped, frozen_pending) = match session.status() {
                Ok(status) => (
                    status.capacity_frames,
                    status.retained_frames,
                    status.dropped_frames,
                    status.frozen_pending,
                ),
                Err(_) => (0, 0, 0, false),
            };
            let capacity = capacity as f64 / f64::from(session.sample_rate);
            let retained = retained as f64 / f64::from(session.sample_rate);
            let resolved = runtime.active_profile.as_ref().map(|p| p.name.clone());
            (
                state,
                last_error,
                resolved,
                session.sample_rate,
                session.channel_count,
                "F32LE".to_string(),
                capacity,
                retained,
                dropped,
                frozen_pending,
            )
        } else {
            let resolved = runtime.active_profile.as_ref().map(|p| p.name.clone());
            (
                state,
                last_error,
                resolved,
                0,
                0,
                "".to_string(),
                0.0,
                0.0,
                0,
                false,
            )
        }
    } else {
        (
            "poisoned".to_string(),
            None,
            None,
            0,
            0,
            "".to_string(),
            0.0,
            0.0,
            0,
            false,
        )
    };
    DaemonStatus {
        state,
        active_export_count: u32::from(frozen_pending),
        pending_recall_count: 0,
        buffer_capacity_seconds: buffer_capacity,
        retained_seconds: retained,
        dropped_frames: dropped,
        target: Some(ctx.config_path.display().to_string()),
        resolved_target,
        sample_rate,
        channel_count,
        format,
        last_error,
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

fn handle_request(ctx: &DaemonContext, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Recall => persistence_delivery_only_response(status_response(ctx)),
        ControlRequest::Clear => match ctx.session.clear() {
            Ok(()) => ControlResponse {
                ok: true,
                message: "cleared".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            },
            Err(err) => {
                set_last_error(ctx, err.to_string());
                ControlResponse {
                    ok: false,
                    message: err.to_string(),
                    status: Some(status_response(ctx)),
                    persistence_outcome: None,
                    threshold_report: None,
                }
            }
        },
        ControlRequest::Dump => persistence_delivery_only_response(status_response(ctx)),
        ControlRequest::Status => ControlResponse {
            ok: true,
            message: "status".to_string(),
            status: Some(status_response(ctx)),
            persistence_outcome: None,
            threshold_report: None,
        },
        ControlRequest::Stop => {
            publish_stop_before(&ctx.stop, || {
                ctx.session.arena.cancel_calibration();
            });
            ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
        ControlRequest::StartCapture { .. }
        | ControlRequest::StopCapture
        | ControlRequest::Reload => ControlResponse {
            ok: false,
            message: "command not available in legacy runtime config mode".to_string(),
            status: Some(status_response(ctx)),
            persistence_outcome: None,
            threshold_report: None,
        },
        ControlRequest::Threshold { .. } => ControlResponse {
            ok: false,
            message: "profile threshold commands are unsupported for legacy configuration"
                .to_string(),
            status: Some(status_response(ctx)),
            persistence_outcome: None,
            threshold_report: None,
        },
    }
}

fn status_response(ctx: &DaemonContext) -> DaemonStatus {
    let (capacity, retained, dropped, frozen_pending) = match ctx.session.status() {
        Ok(status) => (
            status.capacity_frames,
            status.retained_frames,
            status.dropped_frames,
            status.frozen_pending,
        ),
        Err(_) => (0, 0, 0, false),
    };
    let sample_rate = ctx.session.sample_rate;
    DaemonStatus {
        state: if ctx.stop.load(Ordering::Acquire) {
            "stopping".to_string()
        } else if ctx
            .capture_health
            .as_ref()
            .and_then(PipeWireHealth::fault)
            .is_some()
        {
            "faulted".to_string()
        } else {
            "capturing".to_string()
        },
        active_export_count: u32::from(frozen_pending),
        pending_recall_count: 0,
        buffer_capacity_seconds: capacity as f64 / f64::from(sample_rate),
        retained_seconds: retained as f64 / f64::from(sample_rate),
        dropped_frames: dropped,
        target: ctx.cfg.target.clone(),
        resolved_target: status_resolved_target(ctx),
        sample_rate,
        channel_count: ctx.cfg.channels.unwrap_or_else(|| {
            ctx.resolved_target
                .as_ref()
                .map(|target| target.channels)
                .unwrap_or(2)
        }),
        format: ctx.cfg.sample_format.clone(),
        last_error: ctx
            .capture_health
            .as_ref()
            .and_then(PipeWireHealth::fault)
            .or_else(|| ctx.last_error.lock().ok().and_then(|last| last.clone())),
    }
}

fn persistence_delivery_only_response(status: DaemonStatus) -> ControlResponse {
    ControlResponse {
        ok: false,
        message: "persistence commands require operation-worker delivery".to_string(),
        status: Some(status),
        persistence_outcome: None,
        threshold_report: None,
    }
}

fn status_resolved_target(ctx: &DaemonContext) -> Option<String> {
    if let Some(target) = ctx.resolved_target.as_ref() {
        return Some(match target.id {
            Some(id) => format!("{} ({id})", target.name),
            None => target.name.clone(),
        });
    }
    Some(ctx.cfg.backend.clone())
}

fn set_last_error(ctx: &DaemonContext, message: String) {
    if let Ok(mut last) = ctx.last_error.lock() {
        *last = Some(message);
    }
}

fn legacy_persistence_delivery(
    ctx: &DaemonContext,
    command: ExportCommand,
    mut stream: UnixStream,
) {
    let timestamp = iso8601_compact_label();
    let mut entered = false;
    let result = ctx
        .session
        .persist_with_delivery(command, &timestamp, |outcome| {
            entered = true;
            let status = status_response(ctx);
            let message = persistence_message_from_committed(&outcome);
            write_persistence_response(
                &mut stream,
                true,
                &message,
                &status,
                ctx.session.sample_rate,
                outcome,
            )
        });
    if let Err(error) = result {
        set_last_error(ctx, error.to_string());
        if !entered {
            let response = ControlResponse {
                ok: false,
                message: error.to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            };
            let _ = write_response(stream, &response);
        }
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

#[cfg(test)]
#[allow(dead_code)]
fn handle_dump(ctx: &DaemonContext) -> ControlResponse {
    let timestamp = iso8601_compact_label();
    let result = ctx.session.persist(ExportCommand::Dump, &timestamp);
    legacy_persistence_response(ctx, result)
}

#[cfg(test)]
#[allow(dead_code)]
fn handle_recall(ctx: &DaemonContext) -> ControlResponse {
    let timestamp = iso8601_compact_label();
    let result = ctx.session.persist(ExportCommand::Recall, &timestamp);
    legacy_persistence_response(ctx, result)
}

#[cfg(test)]
#[allow(dead_code)]
fn legacy_persistence_response(
    ctx: &DaemonContext,
    result: Result<DumpOutcome>,
) -> ControlResponse {
    match result {
        Ok(outcome) => persistence_response(outcome, ctx.session.sample_rate, status_response(ctx)),
        Err(err) => {
            set_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
                threshold_report: None,
            }
        }
    }
}

fn start_app_capture(
    ctx: &IdleDaemonContext,
    requested_profile: Option<String>,
    activate: bool,
) -> Result<String> {
    let mut cfg = profile::load_config_for_mutation(&ctx.config_path)?;
    let profile_name = requested_profile
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| cfg.daemon.active_profile.clone())
        .ok_or_else(|| LambError::Validation("no active profile configured".to_string()))?;
    let profile_config = cfg
        .profiles
        .get(&profile_name)
        .ok_or_else(|| LambError::Config(format!("profile {profile_name} does not exist")))?;
    let resolved = profile::validate_profile(&profile_name, profile_config)?;
    if activate {
        cfg.daemon.active_profile = Some(profile_name.clone());
        profile::save_config(&ctx.config_path, &cfg)?;
    }

    let old_capture = {
        let mut runtime = ctx
            .runtime
            .lock()
            .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
        runtime.capture.take()
    };
    drop(old_capture);

    let channel_names: Vec<String> = resolved.ports.iter().map(|p| p.name.clone()).collect();
    let resolved_for_runtime = resolved.clone();
    let params = app_runtime_params(&resolved);

    let (backend, runtime_session) = match resolved.backend.as_str() {
        "jack" => {
            let jack_cfg = JackCaptureConfig::from_profile(&resolved);
            let (capture, runtime) = JackCapture::start(jack_cfg, params).inspect_err(|err| {
                set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
            })?;
            (CaptureBackend::Jack(capture, channel_names), runtime)
        }
        "pipewire" => {
            let pw_cfg = resolved.pipewire_config.clone().ok_or_else(|| {
                let err =
                    LambError::Validation("pipewire profile missing pipewire config".to_string());
                set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
                err
            })?;
            let resolved_target =
                crate::capture_pipewire::resolve_target(&pw_cfg).inspect_err(|err| {
                    set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
                })?;
            eprintln!("lamb: {}", resolved_target.log_message());
            let (capture, runtime) =
                PipeWireCapture::start_with_resolved(pw_cfg, resolved_target, params).inspect_err(
                    |err| {
                        set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
                    },
                )?;
            (CaptureBackend::PipeWire(capture, channel_names), runtime)
        }
        other => unreachable!("backend validated as jack or pipewire, got {other}"),
    };

    let session = Arc::new(CaptureSession::from_app_runtime(
        runtime_session,
        &resolved_for_runtime,
        backend.sample_rate(),
        match &backend {
            CaptureBackend::Jack(_, _) => jack_live_identities(&resolved_for_runtime)?,
            CaptureBackend::PipeWire(capture, _) => {
                pipewire_live_identities(&resolved_for_runtime, capture.resolved_target())?
            }
        },
    )?);
    install_effective_session_activity_policy(
        &cfg,
        &resolved_for_runtime,
        &session,
        &ctx.calibration_root,
        unix_now(),
    )?;
    let _ = session.recover_startup(Path::new(APP_STAGING_ROOT));

    let mut runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    runtime.config = cfg;
    runtime.state = "capturing".to_string();
    runtime.last_error = None;
    runtime.active_profile = Some(resolved_for_runtime);
    runtime.capture_health = match &backend {
        CaptureBackend::PipeWire(capture, _) => Some(capture.health()),
        CaptureBackend::Jack(_, _) => None,
    };
    runtime.capture = Some(backend);
    runtime.session = Some(session);
    Ok(format!("capturing {profile_name}"))
}

fn stop_app_capture(ctx: &IdleDaemonContext) {
    if let Ok(mut runtime) = ctx.runtime.lock() {
        let capture = runtime.capture.take();
        runtime.capture_health = None;
        runtime.session = None;
        runtime.state = if runtime.active_profile.is_some() {
            "idle".to_string()
        } else {
            "unconfigured".to_string()
        };
        runtime.last_error = None;
        drop(capture);
    }
}

fn app_lock_error_response(ctx: &IdleDaemonContext) -> ControlResponse {
    ControlResponse {
        ok: false,
        message: "runtime state lock poisoned".to_string(),
        status: Some(idle_status_response(ctx)),
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
            persistence_outcome: None,
            threshold_report: Some(report),
        },
        Err(error) => {
            set_app_last_error(ctx, error.to_string());
            ControlResponse {
                ok: false,
                message: error.to_string(),
                status: Some(idle_status_response(ctx)),
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
            persistence_outcome: None,
            threshold_report: None,
        };
    };
    match session.clear() {
        Ok(()) => ControlResponse {
            ok: true,
            message: "cleared".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
            threshold_report: None,
        },
        Err(err) => {
            set_app_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(idle_status_response(ctx)),
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

fn set_app_fault(
    ctx: &IdleDaemonContext,
    cfg: &app_config::AppConfig,
    resolved: Option<profile::ResolvedProfile>,
    error: String,
) {
    if let Ok(mut runtime) = ctx.runtime.lock() {
        runtime.config = cfg.clone();
        runtime.state = "faulted".to_string();
        runtime.last_error = Some(error);
        runtime.active_profile = resolved;
        runtime.capture = None;
        runtime.capture_health = None;
        runtime.session = None;
    }
}

fn set_app_last_error(ctx: &IdleDaemonContext, error: String) {
    if let Ok(mut runtime) = ctx.runtime.lock() {
        runtime.last_error = Some(error);
    }
}

fn reload_app_config(ctx: &IdleDaemonContext) -> Result<()> {
    let mut runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    reload_app_config_inner(&mut runtime, &ctx.config_path, &ctx.calibration_root)
}

fn reload_app_config_inner(
    state: &mut AppRuntimeState,
    path: &Path,
    calibration_root: &Path,
) -> Result<()> {
    let loaded = app_config::load_optional_config(path)?;
    match loaded.state {
        ConfigLoadState::Loaded => {
            state.config = loaded.config.clone();
            let active_profile = profile::resolve_active_profile(&loaded.config)?;
            if let Some(profile) = active_profile {
                state.active_profile = Some(profile.clone());
                if state.config.daemon.start_mode == "auto" {
                    state.capture.take();
                    state.capture_health = None;
                    state.session = None;
                    let channel_names: Vec<String> =
                        profile.ports.iter().map(|p| p.name.clone()).collect();
                    let params = app_runtime_params(&profile);
                    let build = match profile.backend.as_str() {
                        "jack" => {
                            JackCapture::start(JackCaptureConfig::from_profile(&profile), params)
                                .map(|(capture, runtime)| {
                                    (CaptureBackend::Jack(capture, channel_names), runtime)
                                })
                        }
                        "pipewire" => {
                            if let Some(pw_cfg) = profile.pipewire_config.clone() {
                                match crate::capture_pipewire::resolve_target(&pw_cfg) {
                                    Ok(resolved_target) => {
                                        eprintln!("lamb: {}", resolved_target.log_message());
                                        PipeWireCapture::start_with_resolved(
                                            pw_cfg,
                                            resolved_target,
                                            params,
                                        )
                                        .map(
                                            |(capture, runtime)| {
                                                (
                                                    CaptureBackend::PipeWire(
                                                        capture,
                                                        channel_names,
                                                    ),
                                                    runtime,
                                                )
                                            },
                                        )
                                    }
                                    Err(err) => Err(err),
                                }
                            } else {
                                Err(LambError::Validation(
                                    "pipewire profile has no pipewire config".to_string(),
                                ))
                            }
                        }
                        other => Err(LambError::Validation(format!("unknown backend: {other}"))),
                    };
                    match build {
                        Ok((backend, runtime)) => {
                            let sample_rate = backend.sample_rate();
                            let session = Arc::new(CaptureSession::from_app_runtime(
                                runtime,
                                &profile,
                                sample_rate,
                                match &backend {
                                    CaptureBackend::Jack(_, _) => jack_live_identities(&profile)?,
                                    CaptureBackend::PipeWire(capture, _) => {
                                        pipewire_live_identities(
                                            &profile,
                                            capture.resolved_target(),
                                        )?
                                    }
                                },
                            )?);
                            install_effective_session_activity_policy(
                                &loaded.config,
                                &profile,
                                &session,
                                calibration_root,
                                unix_now(),
                            )?;
                            let _ = session.recover_startup(Path::new(APP_STAGING_ROOT));
                            state.session = Some(session);
                            state.state = "capturing".to_string();
                            state.last_error = None;
                            state.capture_health = match &backend {
                                CaptureBackend::PipeWire(capture, _) => Some(capture.health()),
                                CaptureBackend::Jack(_, _) => None,
                            };
                            state.capture = Some(backend);
                        }
                        Err(err) => {
                            state.state = "faulted".to_string();
                            state.last_error = Some(err.to_string());
                            state.capture_health = None;
                        }
                    }
                } else {
                    state.state = "idle".to_string();
                    state.last_error = None;
                    state.capture = None;
                    state.capture_health = None;
                    state.session = None;
                }
            } else {
                state.state = "unconfigured".to_string();
                state.last_error = Some("no active profile configured".to_string());
                state.active_profile = None;
                state.capture = None;
                state.capture_health = None;
                state.session = None;
            }
            state.config_load_error = None;
            Ok(())
        }
        ConfigLoadState::Missing => {
            let error = format!("config file not found: {}", path.display());
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = Some(error.clone());
            state.config_load_error = Some(error);
            state.active_profile = None;
            state.capture = None;
            state.capture_health = None;
            state.session = None;
            Ok(())
        }
        ConfigLoadState::Invalid => {
            let error = loaded
                .error
                .unwrap_or_else(|| format!("invalid config file: {}", path.display()));
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = Some(error.clone());
            state.config_load_error = Some(error);
            state.active_profile = None;
            state.capture = None;
            state.capture_health = None;
            state.session = None;
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
    use std::sync::mpsc;

    const ROUTE_TEST_TIMEOUT: Duration = Duration::from_secs(2);

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
        let listener = UnixListener::bind(&socket_path).unwrap();
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
                listener,
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
        assert_eq!(active.message, diagnostic);
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
        lane: &OperationLane<(ControlRequest, UnixStream)>,
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
            stop: AtomicBool::new(false),
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

        // Both external-backend production construction paths invoke this exact
        // validated installer. This source assertion avoids starting JACK/PipeWire.
        let source = include_str!("daemon.rs");
        let explicit = &source[source.find("fn start_app_capture(").unwrap()
            ..source.find("fn stop_app_capture(").unwrap()];
        let automatic = &source[source.find("fn reload_app_config_inner(").unwrap()
            ..source.find("fn iso8601_compact_label(").unwrap()];
        assert_eq!(
            explicit
                .matches("install_effective_session_activity_policy(")
                .count(),
            1
        );
        assert_eq!(
            automatic
                .matches("install_effective_session_activity_policy(")
                .count(),
            1
        );
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
    fn admitted_stop_capture_cancels_calibration_before_occupied_lane_runs_it() {
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
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: Some(profile),
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            stop: AtomicBool::new(false),
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
            move |(request, stream): (ControlRequest, UnixStream)| {
                if worker_jobs.fetch_add(1, Ordering::AcqRel) == 0 {
                    entered.wait();
                    release.wait();
                } else {
                    let response = handle_idle_request(&worker_ctx, request);
                    write_response(stream, &response).unwrap();
                    finished.wait();
                }
            },
            |_job: (ControlRequest, UnixStream)| {},
        );
        let (occupied_stream, _occupied_peer) = UnixStream::pair().unwrap();
        lane.try_enqueue((ControlRequest::Reload, occupied_stream))
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
            cancelled_before_worker_release,
            "StopCapture admission must cancel before its queued handler runs"
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
                    state: "capturing".to_string(),
                    last_error: None,
                    config_load_error: None,
                    active_profile: Some(profile),
                    capture: None,
                    capture_health: None,
                    session: Some(session),
                    test_capture_attached: true,
                }),
                stop: AtomicBool::new(false),
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
    fn direct_stop_publishes_and_closes_lane_before_lock_dependent_cancellation_callback() {
        let stop = AtomicBool::new(false);
        let lane = OperationLane::<()>::new(1).unwrap();
        let callback_observed_ordering = std::cell::Cell::new(false);

        publish_stop_and_close_before(&stop, &lane, || {
            callback_observed_ordering.set(stop.load(Ordering::Acquire) && lane.is_closed());
        });

        assert!(callback_observed_ordering.get());
        assert!(stop.load(Ordering::Acquire));
        assert!(lane.is_closed());
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
            move |(request, stream): (ControlRequest, UnixStream)| {
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
                        persistence_outcome: None,
                        threshold_report: None,
                    },
                )
                .unwrap();
            },
            |_job: (ControlRequest, UnixStream)| {},
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
            move |(_request, _stream): (ControlRequest, UnixStream)| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(ROUTE_TEST_TIMEOUT).unwrap();
            },
            |_job: (ControlRequest, UnixStream)| {},
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

    #[test]
    fn every_legacy_threshold_operation_returns_the_exact_unsupported_response() {
        let ctx = DaemonContext {
            cfg: test_legacy_config(),
            session: test_session(),
            resolved_target: None,
            stop: AtomicBool::new(false),
            last_error: Mutex::new(None),
            capture_health: None,
        };
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
            let response = handle_request(&ctx, ControlRequest::Threshold { request });
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

        let legacy = DaemonContext {
            cfg: test_legacy_config(),
            session: test_session(),
            resolved_target: None,
            stop: AtomicBool::new(false),
            last_error: Mutex::new(None),
            capture_health: Some(health.clone()),
        };
        let profile = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: Some(health),
                session: None,
                test_capture_attached: false,
            }),
            stop: AtomicBool::new(false),
        };

        for status in [status_response(&legacy), idle_status_response(&profile)] {
            assert_eq!(status.state, "faulted");
            assert_eq!(
                status.last_error.as_deref(),
                Some("PipeWire core/proxy error: server disconnected")
            );
        }
    }

    #[test]
    fn normal_pipewire_stop_is_not_reported_as_a_fault() {
        let health = crate::capture_pipewire::PipeWireHealth::default();
        let legacy = DaemonContext {
            cfg: test_legacy_config(),
            session: test_session(),
            resolved_target: None,
            stop: AtomicBool::new(true),
            last_error: Mutex::new(None),
            capture_health: Some(health),
        };

        let status = status_response(&legacy);
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
        }
    }

    fn test_legacy_config() -> LambConfig {
        LambConfig {
            config_version: 1,
            user: "test".to_string(),
            target: None,
            backend: "pipewire".to_string(),
            channels: Some(1),
            channel_map: None,
            capture_ports: Vec::new(),
            seconds: 1,
            sample_rate: 100,
            sample_format: "F32LE".to_string(),
            latency: None,
            dont_remix: true,
            output_dir: PathBuf::from("/tmp/out"),
            memory: config::MemoryConfig {
                max: None,
                headroom: 1.0,
            },
            max_active_snapshots: 1,
            allow_queued_recall: false,
            chunk_frames: Some(1),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            control_permissions: "0600".to_string(),
            export: config::ExportConfig {
                mode: "per-channel".to_string(),
                format: "wav".to_string(),
                split_when_over_bytes: 3_900_000_000,
            },
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
        });
        IdleDaemonContext {
            config_path: root.join("lamb.toml"),
            control_socket_path: root.join("control.sock"),
            calibration_root: root.join("calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            stop: AtomicBool::new(false),
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
        });
        let ctx = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            calibration_root: PathBuf::from("/tmp/lamb-test-calibration"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                state: "capturing".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: Some(session),
                test_capture_attached: true,
            }),
            stop: AtomicBool::new(false),
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
                state: "idle".to_string(),
                last_error: None,
                config_load_error: None,
                active_profile: None,
                capture: None,
                capture_health: None,
                session: None,
                test_capture_attached: false,
            }),
            stop: AtomicBool::new(false),
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
}
