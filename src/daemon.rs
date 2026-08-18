use crate::app_config::{self, ConfigLoadState};
use crate::capture_fake::FakeCapture;
use crate::capture_jack::{JackCapture, JackCaptureConfig};
use crate::capture_pipewire::{PipeWireCapture, PipeWireCaptureConfig, ResolvedTarget};
use crate::config::{self, LambConfig};
use crate::control::{ControlRequest, ControlResponse, DaemonStatus, PersistenceOutcomeResponse};
use crate::dump::{DumpCoordinator, DumpOutcome};
use crate::error::{io_error, LambError, Result};
use crate::export_wav::{publish_dump, publish_recall, DumpPublishRequest, RecallPublishRequest};
use crate::math::{derive_chunk_frames, estimate_ring_bytes};
use crate::profile;
use crate::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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
    match loaded.state {
        ConfigLoadState::Loaded => run_app_config_daemon(path, loaded.config),
        ConfigLoadState::Missing | ConfigLoadState::Invalid => {
            let reason = match loaded.state {
                ConfigLoadState::Missing => format!("config file not found: {}", path.display()),
                ConfigLoadState::Invalid => loaded
                    .error
                    .unwrap_or_else(|| format!("invalid config file: {}", path.display())),
                _ => unreachable!(),
            };
            run_idle_fallback(path, loaded.config.daemon.control_socket_path, reason)
        }
    }
}

fn run_idle_fallback(path: &Path, socket_template: String, reason: String) -> Result<()> {
    let control_socket_path = expand_control_socket_path(&socket_template)?;
    let listener = bind_control_socket(&control_socket_path)?;
    let ctx = IdleDaemonContext {
        config_path: path.to_path_buf(),
        control_socket_path,
        runtime: Mutex::new(AppRuntimeState {
            config: app_config::AppConfig::default(),
            state: "unconfigured".to_string(),
            last_error: Some(reason),
            active_profile: None,
            capture: None,
            dump_coordinator: None,
        }),
        stop: AtomicBool::new(false),
    };

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = handle_idle_stream(&ctx, stream);
            }
            Err(err) => {
                eprintln!("lamb: connection error: {err}");
            }
        }
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
    }

    let _ = fs::remove_file(&ctx.control_socket_path);
    Ok(())
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

    let mut resolved_target = None;
    let mut fake_capture = None;
    let mut pipewire_capture = None;
    let channels = if cfg.backend == "pipewire" {
        let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&cfg);
        let resolved = crate::capture_pipewire::resolve_target(&pipewire_cfg)?;
        eprintln!("lamb: {}", resolved.log_message());
        cfg.channels = Some(resolved.channels);
        cfg.sample_rate = resolved.sample_rate;
        resolved_target = Some(resolved);
        cfg.channels.unwrap()
    } else {
        cfg.channels.unwrap_or(2)
    };
    let ring = make_ring(&cfg, channels)?;
    match cfg.backend.as_str() {
        "fake" => {
            fake_capture = Some(FakeCapture::start(
                Arc::clone(&ring),
                channels,
                cfg.chunk_frames.unwrap_or(25),
            )?);
        }
        "pipewire" => {
            let resolved = resolved_target.clone().ok_or_else(|| {
                LambError::Capture("PipeWire target was not resolved".to_string())
            })?;
            let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&cfg);
            pipewire_capture = Some(PipeWireCapture::start_with_resolved(
                pipewire_cfg,
                resolved,
                Arc::clone(&ring),
            )?);
        }
        other => return Err(LambError::Capture(format!("unsupported backend {other}"))),
    }

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

    let ctx = DaemonContext {
        cfg,
        ring,
        dump_coordinator: DumpCoordinator::new(),
        resolved_target,
        stop: AtomicBool::new(false),
        last_error: Mutex::new(None),
    };

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = handle_stream(&ctx, stream) {
                    if let Ok(mut last) = ctx.last_error.lock() {
                        *last = Some(err.to_string());
                    }
                }
            }
            Err(err) => {
                if let Ok(mut last) = ctx.last_error.lock() {
                    *last = Some(err.to_string());
                }
            }
        }
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
    }

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
    ring: Arc<SampleRing>,
    dump_coordinator: DumpCoordinator,
    resolved_target: Option<ResolvedTarget>,
    stop: AtomicBool,
    last_error: Mutex<Option<String>>,
}

struct IdleDaemonContext {
    config_path: PathBuf,
    control_socket_path: PathBuf,
    runtime: Mutex<AppRuntimeState>,
    stop: AtomicBool,
}

struct AppRuntimeState {
    config: app_config::AppConfig,
    state: String,
    last_error: Option<String>,
    active_profile: Option<profile::ResolvedProfile>,
    capture: Option<CaptureBackend>,
    dump_coordinator: Option<Arc<DumpCoordinator>>,
}

enum CaptureBackend {
    Jack(JackCapture, Vec<String>),
    PipeWire(PipeWireCapture, Vec<String>),
}

impl CaptureBackend {
    fn ring(&self) -> &Arc<SampleRing> {
        match self {
            CaptureBackend::Jack(c, _) => &c.ring,
            CaptureBackend::PipeWire(c, _) => &c.ring,
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            CaptureBackend::Jack(c, _) => c.sample_rate,
            CaptureBackend::PipeWire(c, _) => c.sample_rate,
        }
    }

    fn channel_count(&self) -> u32 {
        match self {
            CaptureBackend::Jack(c, _) => c.channel_count,
            CaptureBackend::PipeWire(c, _) => c.channel_count,
        }
    }

    fn channel_names(&self) -> &[String] {
        match self {
            CaptureBackend::Jack(_, names) => names,
            CaptureBackend::PipeWire(_, names) => names,
        }
    }
}

fn run_app_config_daemon(path: &Path, config: app_config::AppConfig) -> Result<()> {
    let control_socket_path = expand_control_socket_path(&config.daemon.control_socket_path)?;
    let listener = bind_control_socket(&control_socket_path)?;

    let mut state = AppRuntimeState {
        config,
        state: "unconfigured".to_string(),
        last_error: None,
        active_profile: None,
        capture: None,
        dump_coordinator: None,
    };

    match reload_app_config_inner(&mut state, path) {
        Ok(()) => {}
        Err(err) => {
            state.last_error = Some(err.to_string());
        }
    }

    let ctx = IdleDaemonContext {
        config_path: path.to_path_buf(),
        control_socket_path,
        runtime: Mutex::new(state),
        stop: AtomicBool::new(false),
    };

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = handle_idle_stream(&ctx, stream);
            }
            Err(err) => {
                eprintln!("lamb: connection error: {err}");
            }
        }
        if ctx.stop.load(Ordering::Acquire) {
            break;
        }
    }

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

fn handle_stream(ctx: &DaemonContext, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| LambError::Control(source.to_string()))?;
    let request: ControlRequest = serde_json::from_str(&line)
        .map_err(|err| LambError::Control(format!("invalid control request: {err}")))?;
    let response = handle_request(ctx, request);
    let mut stream = reader.into_inner();
    let body =
        serde_json::to_string(&response).map_err(|err| LambError::Control(err.to_string()))?;
    writeln!(stream, "{body}").map_err(|source| LambError::Control(source.to_string()))?;
    Ok(())
}

fn handle_idle_stream(ctx: &IdleDaemonContext, stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| LambError::Control(source.to_string()))?;
    let request: ControlRequest = serde_json::from_str(&line)
        .map_err(|err| LambError::Control(format!("invalid control request: {err}")))?;
    let response = handle_idle_request(ctx, request);
    let mut stream = reader.into_inner();
    let body =
        serde_json::to_string(&response).map_err(|err| LambError::Control(err.to_string()))?;
    writeln!(stream, "{body}").map_err(|source| LambError::Control(source.to_string()))?;
    Ok(())
}

fn handle_idle_request(ctx: &IdleDaemonContext, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Status => ControlResponse {
            ok: true,
            message: "status".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
        },
        ControlRequest::Stop => {
            ctx.stop.store(true, Ordering::Release);
            ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
            }
        }
        ControlRequest::StartCapture { profile, activate } => {
            match start_app_capture(ctx, profile, activate) {
                Ok(message) => ControlResponse {
                    ok: true,
                    message,
                    status: Some(idle_status_response(ctx)),
                    persistence_outcome: None,
                },
                Err(err) => {
                    set_app_last_error(ctx, err.to_string());
                    ControlResponse {
                        ok: false,
                        message: err.to_string(),
                        status: Some(idle_status_response(ctx)),
                        persistence_outcome: None,
                    }
                }
            }
        }
        ControlRequest::StopCapture => {
            stop_app_capture(ctx);
            ControlResponse {
                ok: true,
                message: "capture stopped".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
            }
        }
        ControlRequest::Recall => handle_app_recall(ctx),
        ControlRequest::Clear => handle_app_clear(ctx),
        ControlRequest::Dump => handle_app_dump(ctx),
        ControlRequest::Reload => match reload_app_config(ctx) {
            Ok(()) => ControlResponse {
                ok: true,
                message: "config reloaded".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
            },
            Err(err) => {
                set_app_last_error(ctx, err.to_string());
                ControlResponse {
                    ok: false,
                    message: err.to_string(),
                    status: Some(idle_status_response(ctx)),
                    persistence_outcome: None,
                }
            }
        },
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
    ) = if let Some(ref runtime) = runtime {
        let state = if ctx.stop.load(Ordering::Acquire) {
            "stopping".to_string()
        } else {
            runtime.state.clone()
        };
        let last_error = runtime.last_error.clone();
        if let Some(backend) = runtime.capture.as_ref() {
            let ring_status = backend.ring().status();
            let capacity = ring_status.capacity_frames as f64 / f64::from(backend.sample_rate());
            let retained = ring_status.retained_frames as f64 / f64::from(backend.sample_rate());
            let resolved = runtime.active_profile.as_ref().map(|p| p.name.clone());
            (
                state,
                last_error,
                resolved,
                backend.sample_rate(),
                backend.channel_count(),
                "F32LE".to_string(),
                capacity,
                retained,
                ring_status.dropped_frames,
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
        )
    };
    DaemonStatus {
        state,
        active_export_count: 0,
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

fn make_ring(cfg: &LambConfig, channels: u32) -> Result<Arc<SampleRing>> {
    let chunk_frames = derive_chunk_frames(cfg.sample_rate, cfg.chunk_frames)?;
    let total_frames = u64::from(cfg.seconds)
        .checked_mul(u64::from(cfg.sample_rate))
        .ok_or_else(|| LambError::Validation("ring frame count overflow".to_string()))?;
    let chunk_count = total_frames.div_ceil(u64::from(chunk_frames)).max(1);
    let required = estimate_ring_bytes(
        cfg.seconds,
        cfg.sample_rate,
        channels,
        4,
        cfg.memory.headroom,
    )?;
    if let Some(max) = cfg.memory.max {
        if required > max {
            return Err(LambError::Validation(format!(
                "required memory {required} exceeds configured memory.max {max}"
            )));
        }
    }
    Ok(Arc::new(SampleRing::new(RingConfig {
        channels,
        sample_rate: cfg.sample_rate,
        format: SampleFormat::F32Le,
        chunk_frames,
        chunk_count: u32::try_from(chunk_count)
            .map_err(|_| LambError::Validation("chunk count exceeds u32".to_string()))?,
        max_active_snapshots: cfg.max_active_snapshots,
    })?))
}

fn handle_request(ctx: &DaemonContext, request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Recall => handle_recall(ctx),
        ControlRequest::Clear => match ctx.ring.clear() {
            Ok(()) => ControlResponse {
                ok: true,
                message: "cleared".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
            },
            Err(err) => {
                set_last_error(ctx, err.to_string());
                ControlResponse {
                    ok: false,
                    message: err.to_string(),
                    status: Some(status_response(ctx)),
                    persistence_outcome: None,
                }
            }
        },
        ControlRequest::Dump => handle_dump(ctx),
        ControlRequest::Status => ControlResponse {
            ok: true,
            message: "status".to_string(),
            status: Some(status_response(ctx)),
            persistence_outcome: None,
        },
        ControlRequest::Stop => {
            ctx.stop.store(true, Ordering::Release);
            ControlResponse {
                ok: true,
                message: "stopping".to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
            }
        }
        ControlRequest::StartCapture { .. }
        | ControlRequest::StopCapture
        | ControlRequest::Reload => ControlResponse {
            ok: false,
            message: "command not available in legacy runtime config mode".to_string(),
            status: Some(status_response(ctx)),
            persistence_outcome: None,
        },
    }
}

fn status_response(ctx: &DaemonContext) -> DaemonStatus {
    let ring_status = ctx.ring.status();
    DaemonStatus {
        state: if ctx.stop.load(Ordering::Acquire) {
            "stopping".to_string()
        } else {
            "capturing".to_string()
        },
        active_export_count: ring_status.active_snapshots,
        pending_recall_count: 0,
        buffer_capacity_seconds: ring_status.capacity_frames as f64
            / f64::from(ctx.cfg.sample_rate),
        retained_seconds: ring_status.retained_frames as f64 / f64::from(ctx.cfg.sample_rate),
        dropped_frames: ring_status.dropped_frames,
        target: ctx.cfg.target.clone(),
        resolved_target: status_resolved_target(ctx),
        sample_rate: ctx.cfg.sample_rate,
        channel_count: ctx.cfg.channels.unwrap_or_else(|| {
            ctx.resolved_target
                .as_ref()
                .map(|target| target.channels)
                .unwrap_or(2)
        }),
        format: ctx.cfg.sample_format.clone(),
        last_error: ctx.last_error.lock().ok().and_then(|last| last.clone()),
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

fn handle_dump(ctx: &DaemonContext) -> ControlResponse {
    let timestamp = iso8601_compact_label();
    let result = ctx.dump_coordinator.dump(&ctx.ring, |snapshot| {
        publish_dump(DumpPublishRequest {
            snapshot,
            output_parent: &ctx.cfg.output_dir,
            timestamp: &timestamp,
            split_when_over_bytes: ctx.cfg.export.split_when_over_bytes,
            channel_names: &ctx.cfg.channel_map,
        })
    });
    legacy_persistence_response(ctx, result)
}

fn handle_recall(ctx: &DaemonContext) -> ControlResponse {
    let timestamp = iso8601_compact_label();
    let result = ctx.dump_coordinator.dump(&ctx.ring, |snapshot| {
        publish_recall(RecallPublishRequest {
            snapshot,
            output_dir: &ctx.cfg.output_dir,
            staging_root: Path::new("/tmp/LAMB/staging"),
            timestamp: &timestamp,
            split_when_over_bytes: ctx.cfg.export.split_when_over_bytes,
            channel_names: &ctx.cfg.channel_map,
        })
    });
    legacy_persistence_response(ctx, result)
}

fn legacy_persistence_response(
    ctx: &DaemonContext,
    result: Result<DumpOutcome>,
) -> ControlResponse {
    match result {
        Ok(outcome) => persistence_response(outcome, ctx.cfg.sample_rate, status_response(ctx)),
        Err(err) => {
            set_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(status_response(ctx)),
                persistence_outcome: None,
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
        runtime.dump_coordinator = None;
        runtime.capture.take()
    };
    drop(old_capture);

    let channel_names: Vec<String> = resolved.ports.iter().map(|p| p.name.clone()).collect();
    let resolved_for_runtime = resolved.clone();

    let backend: CaptureBackend = match resolved.backend.as_str() {
        "jack" => {
            let jack_cfg = JackCaptureConfig::from_profile(&resolved);
            let capture =
                JackCapture::start(jack_cfg, resolved.buffer_seconds).inspect_err(|err| {
                    set_app_fault(ctx, &cfg, Some(resolved), err.to_string());
                })?;
            CaptureBackend::Jack(capture, channel_names)
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
            let ring = crate::capture_pipewire::make_pipewire_ring(
                resolved.buffer_seconds,
                resolved_target.sample_rate,
                resolved_target.channels,
                1, // conservative: app-config profiles don't expose max_active_snapshots yet
            )
            .inspect_err(|err| {
                set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
            })?;
            let capture = PipeWireCapture::start_with_resolved(pw_cfg, resolved_target, ring)
                .inspect_err(|err| {
                    set_app_fault(ctx, &cfg, Some(resolved.clone()), err.to_string());
                })?;
            CaptureBackend::PipeWire(capture, channel_names)
        }
        other => unreachable!("backend validated as jack or pipewire, got {other}"),
    };

    let mut runtime = ctx
        .runtime
        .lock()
        .map_err(|_| LambError::Control("runtime state lock poisoned".to_string()))?;
    runtime.config = cfg;
    runtime.state = "capturing".to_string();
    runtime.last_error = None;
    runtime.active_profile = Some(resolved_for_runtime);
    runtime.capture = Some(backend);
    runtime.dump_coordinator = Some(Arc::new(DumpCoordinator::new()));
    Ok(format!("capturing {profile_name}"))
}

fn stop_app_capture(ctx: &IdleDaemonContext) {
    if let Ok(mut runtime) = ctx.runtime.lock() {
        let capture = runtime.capture.take();
        runtime.dump_coordinator = None;
        runtime.state = if runtime.active_profile.is_some() {
            "idle".to_string()
        } else {
            "unconfigured".to_string()
        };
        runtime.last_error = None;
        drop(capture);
    }
}

fn handle_app_recall(ctx: &IdleDaemonContext) -> ControlResponse {
    let capture = {
        let runtime = match ctx.runtime.lock() {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(error.into_inner());
                return app_runtime_lock_error_response(ctx);
            }
        };
        (|| {
            let backend = runtime.capture.as_ref()?;
            let profile = runtime.active_profile.clone()?;
            let coordinator = Arc::clone(runtime.dump_coordinator.as_ref()?);
            Some((
                Arc::clone(backend.ring()),
                coordinator,
                backend.sample_rate(),
                profile.export_output_dir,
                backend.channel_names().to_vec(),
            ))
        })()
    };
    let Some((ring, coordinator, sample_rate, output_dir, channel_names)) = capture else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
        };
    };

    let timestamp = iso8601_compact_label();
    let result = coordinator.dump(&ring, |snapshot| {
        publish_recall(RecallPublishRequest {
            snapshot,
            output_dir: &output_dir,
            staging_root: Path::new("/tmp/LAMB/staging"),
            timestamp: &timestamp,
            split_when_over_bytes: crate::math::WAV_SPLIT_DEFAULT_BYTES,
            channel_names: &channel_names,
        })
    });
    app_persistence_response(ctx, result, sample_rate)
}

fn handle_app_clear(ctx: &IdleDaemonContext) -> ControlResponse {
    let ring = ctx.runtime.lock().ok().and_then(|runtime| {
        runtime
            .capture
            .as_ref()
            .map(|backend| Arc::clone(backend.ring()))
    });
    let Some(ring) = ring else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
        };
    };
    match ring.clear() {
        Ok(()) => ControlResponse {
            ok: true,
            message: "cleared".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
        },
        Err(err) => {
            set_app_last_error(ctx, err.to_string());
            ControlResponse {
                ok: false,
                message: err.to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
            }
        }
    }
}

fn handle_app_dump(ctx: &IdleDaemonContext) -> ControlResponse {
    let capture = {
        let runtime = match ctx.runtime.lock() {
            Ok(runtime) => runtime,
            Err(error) => {
                drop(error.into_inner());
                return app_runtime_lock_error_response(ctx);
            }
        };
        (|| {
            let backend = runtime.capture.as_ref()?;
            let coordinator = Arc::clone(runtime.dump_coordinator.as_ref()?);
            Some((
                Arc::clone(backend.ring()),
                coordinator,
                backend.sample_rate(),
                backend.channel_names().to_vec(),
            ))
        })()
    };
    let Some((ring, coordinator, sample_rate, channel_names)) = capture else {
        return ControlResponse {
            ok: false,
            message: "capture is not running".to_string(),
            status: Some(idle_status_response(ctx)),
            persistence_outcome: None,
        };
    };

    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => {
            return ControlResponse {
                ok: false,
                message: "HOME not set, cannot resolve dump output path".to_string(),
                status: Some(idle_status_response(ctx)),
                persistence_outcome: None,
            }
        }
    };
    let dump_dir = PathBuf::from(home).join(".cache/lamb/out");

    let timestamp = iso8601_compact_label();
    let result = coordinator.dump(&ring, |snapshot| {
        publish_dump(DumpPublishRequest {
            snapshot,
            output_parent: &dump_dir,
            timestamp: &timestamp,
            split_when_over_bytes: crate::math::WAV_SPLIT_DEFAULT_BYTES,
            channel_names: &channel_names,
        })
    });
    app_persistence_response(ctx, result, sample_rate)
}

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
            }
        }
    }
}

fn app_runtime_lock_error_response(ctx: &IdleDaemonContext) -> ControlResponse {
    ControlResponse {
        ok: false,
        message: "runtime state lock poisoned".to_string(),
        status: Some(idle_status_response(ctx)),
        persistence_outcome: None,
    }
}

fn persistence_response(
    outcome: DumpOutcome,
    sample_rate: u32,
    status: DaemonStatus,
) -> ControlResponse {
    let (message, persistence_outcome) = match outcome {
        DumpOutcome::Written {
            range,
            frames,
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
        runtime.dump_coordinator = None;
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
    reload_app_config_inner(&mut runtime, &ctx.config_path)
}

fn reload_app_config_inner(state: &mut AppRuntimeState, path: &Path) -> Result<()> {
    let loaded = app_config::load_optional_config(path)?;
    match loaded.state {
        ConfigLoadState::Loaded => {
            state.config = loaded.config.clone();
            let active_profile = profile::resolve_active_profile(&loaded.config)?;
            if let Some(profile) = active_profile {
                state.active_profile = Some(profile.clone());
                if state.config.daemon.start_mode == "auto" {
                    state.capture.take();
                    state.dump_coordinator = None;
                    let channel_names: Vec<String> =
                        profile.ports.iter().map(|p| p.name.clone()).collect();
                    match profile.backend.as_str() {
                        "jack" => {
                            match JackCapture::start(
                                JackCaptureConfig::from_profile(&profile),
                                profile.buffer_seconds,
                            ) {
                                Ok(capture) => {
                                    state.state = "capturing".to_string();
                                    state.last_error = None;
                                    state.capture =
                                        Some(CaptureBackend::Jack(capture, channel_names));
                                    state.dump_coordinator = Some(Arc::new(DumpCoordinator::new()));
                                }
                                Err(err) => {
                                    state.state = "faulted".to_string();
                                    state.last_error = Some(err.to_string());
                                }
                            }
                        }
                        "pipewire" => {
                            if let Some(pw_cfg) = profile.pipewire_config.clone() {
                                match crate::capture_pipewire::resolve_target(&pw_cfg) {
                                    Ok(resolved_target) => {
                                        eprintln!("lamb: {}", resolved_target.log_message());
                                        match crate::capture_pipewire::make_pipewire_ring(
                                            profile.buffer_seconds,
                                            resolved_target.sample_rate,
                                            resolved_target.channels,
                                            1, // conservative default for app-config path
                                        ) {
                                            Ok(ring) => {
                                                match PipeWireCapture::start_with_resolved(
                                                    pw_cfg,
                                                    resolved_target,
                                                    ring,
                                                ) {
                                                    Ok(capture) => {
                                                        state.state = "capturing".to_string();
                                                        state.last_error = None;
                                                        state.capture =
                                                            Some(CaptureBackend::PipeWire(
                                                                capture,
                                                                channel_names,
                                                            ));
                                                        state.dump_coordinator =
                                                            Some(Arc::new(DumpCoordinator::new()));
                                                    }
                                                    Err(err) => {
                                                        state.state = "faulted".to_string();
                                                        state.last_error = Some(err.to_string());
                                                    }
                                                }
                                            }
                                            Err(err) => {
                                                state.state = "faulted".to_string();
                                                state.last_error = Some(err.to_string());
                                            }
                                        }
                                    }
                                    Err(err) => {
                                        state.state = "faulted".to_string();
                                        state.last_error = Some(err.to_string());
                                    }
                                }
                            } else {
                                state.state = "faulted".to_string();
                                state.last_error =
                                    Some("pipewire profile has no pipewire config".to_string());
                            }
                        }
                        other => {
                            state.state = "faulted".to_string();
                            state.last_error = Some(format!("unknown backend: {other}"));
                        }
                    }
                } else {
                    state.state = "idle".to_string();
                    state.last_error = None;
                    state.capture = None;
                    state.dump_coordinator = None;
                }
            } else {
                state.state = "unconfigured".to_string();
                state.last_error = Some("no active profile configured".to_string());
                state.active_profile = None;
                state.capture = None;
                state.dump_coordinator = None;
            }
            Ok(())
        }
        ConfigLoadState::Missing => {
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = Some(format!("config file not found: {}", path.display()));
            state.active_profile = None;
            state.capture = None;
            state.dump_coordinator = None;
            Ok(())
        }
        ConfigLoadState::Invalid => {
            state.config = loaded.config;
            state.state = "unconfigured".to_string();
            state.last_error = loaded.error;
            state.active_profile = None;
            state.capture = None;
            state.dump_coordinator = None;
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

    fn poisoned_runtime_context() -> IdleDaemonContext {
        let ctx = IdleDaemonContext {
            config_path: PathBuf::from("/tmp/lamb-test-config.toml"),
            control_socket_path: PathBuf::from("/tmp/lamb-test-control.sock"),
            runtime: Mutex::new(AppRuntimeState {
                config: app_config::AppConfig::default(),
                state: "idle".to_string(),
                last_error: None,
                active_profile: None,
                capture: None,
                dump_coordinator: None,
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
