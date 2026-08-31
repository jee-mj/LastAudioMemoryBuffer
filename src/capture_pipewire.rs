use crate::calibration::LiveDeviceKeyKind;
use crate::capture_arena::CaptureIngress;
use crate::capture_runtime::{CaptureRuntime, CaptureRuntimeParams};
use crate::config::{ConfiguredCapturePort, LambConfig};
use crate::daemon_lifecycle::{RuntimeCaptureFault, RuntimeFaultSink};
use crate::error::{LambError, Result};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub id: Option<u32>,
    pub name: String,
    pub description: Option<String>,
    pub channels: u32,
    pub sample_rate: u32,
    pub format: String,
    pub source_ports: Vec<ResolvedSourcePort>,
    pub durable_live_key: Option<(LiveDeviceKeyKind, String)>,
}

impl ResolvedTarget {
    pub fn durable_live_key(&self) -> Option<(LiveDeviceKeyKind, String)> {
        self.durable_live_key.clone()
    }

    pub fn log_message(&self) -> String {
        let target = match self.id {
            Some(id) => format!("{} ({id})", self.name),
            None => self.name.clone(),
        };
        format!(
            "resolved PipeWire target: {target}, channels={}, sample_rate={}, format={}",
            self.channels, self.sample_rate, self.format
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireCaptureConfig {
    pub target: Option<String>,
    pub capture_ports: Vec<ConfiguredCapturePort>,
    pub sample_rate: u32,
    pub dont_remix: bool,
    pub latency: Option<String>,
}

impl PipeWireCaptureConfig {
    pub fn from_lamb_config(cfg: &LambConfig) -> Result<Self> {
        Ok(Self {
            target: cfg.target.clone(),
            capture_ports: cfg.resolved_capture_ports()?,
            sample_rate: cfg.sample_rate,
            dont_remix: cfg.dont_remix,
            latency: cfg.latency.clone(),
        })
    }

    pub fn channel_count(&self) -> Result<u32> {
        u32::try_from(self.capture_ports.len()).map_err(|_| {
            LambError::Validation("capturePorts exceeds supported channel count".to_string())
        })
    }

    pub fn channel_names(&self) -> Vec<String> {
        self.capture_ports
            .iter()
            .map(|port| port.name.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableNode {
    pub id: u32,
    pub object_type: String,
    pub media_class: Option<String>,
    pub name: Option<String>,
    pub hardware_serial: Option<String>,
    pub object_path: Option<String>,
    pub description: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailablePort {
    pub id: u32,
    pub object_type: String,
    pub node_id: Option<u32>,
    pub direction: Option<String>,
    pub port_id: Option<u32>,
    pub name: Option<String>,
    pub format_dsp: Option<String>,
    pub audio_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSourcePort {
    pub global_id: u32,
    pub node_id: u32,
    pub port_id: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLink {
    pub output_node_id: u32,
    pub output_port_id: u32,
    pub input_node_id: u32,
    pub input_port_id: u32,
}

#[derive(Debug, Clone, Default)]
struct AvailableGraph {
    nodes: Vec<AvailableNode>,
    ports: Vec<AvailablePort>,
    link_factory: Option<LinkFactory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkFactory {
    global_id: u32,
    name: String,
}

pub struct PipeWireCapture {
    resolved: ResolvedTarget,
    pub sample_rate: u32,
    pub channel_count: u32,
    stop_sender: pipewire::channel::Sender<PipeWireCommand>,
    health: PipeWireHealth,
    join: Option<JoinHandle<()>>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TargetResolutionError {
    #[error("PipeWire target not found: {0}")]
    TargetMissing(String),
    #[error("PipeWire capture port not found: {0}")]
    PortMissing(String),
    #[error("PipeWire target changed during startup: {0}")]
    TargetChanged(String),
    #[error("{0}")]
    InvalidSelector(String),
    #[error("{0}")]
    BackendUnavailable(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PipeWireStartupError {
    #[error(transparent)]
    Resolution(#[from] TargetResolutionError),
    #[error(transparent)]
    Capture(#[from] LambError),
}

impl PipeWireStartupError {
    pub(crate) fn into_lamb_error(self) -> LambError {
        match self {
            Self::Resolution(error) => LambError::Capture(error.to_string()),
            Self::Capture(error) => error,
        }
    }
}

/// First fatal error observed after PipeWire startup. Listener callbacks may
/// write it, while daemon status reads it; the realtime process callback never
/// touches this state.
#[derive(Default)]
struct PipeWireHealthState {
    fault: Option<RuntimeCaptureFault>,
    fault_sink: Option<RuntimeFaultSink>,
    notified_sink: Option<RuntimeFaultSink>,
    armed: bool,
    normal_stop: bool,
}

#[derive(Clone, Default)]
pub(crate) struct PipeWireHealth {
    state: Arc<Mutex<PipeWireHealthState>>,
}

impl std::fmt::Debug for PipeWireHealth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PipeWireHealth")
            .field("fault", &self.fault())
            .finish()
    }
}

impl PipeWireHealth {
    #[cfg(test)]
    pub(crate) fn with_fault_sink(fault_sink: RuntimeFaultSink) -> Self {
        let health = Self::unarmed_with_fault_sink(fault_sink);
        health.arm();
        health
    }

    pub(crate) fn unarmed_with_fault_sink(fault_sink: RuntimeFaultSink) -> Self {
        Self {
            state: Arc::new(Mutex::new(PipeWireHealthState {
                fault_sink: Some(fault_sink),
                ..PipeWireHealthState::default()
            })),
        }
    }

    pub(crate) fn record_fatal(&self, fault: impl Into<RuntimeCaptureFault>) -> bool {
        let fault = fault.into();
        let notification = {
            let Ok(mut state) = self.state.lock() else {
                return false;
            };
            if state.normal_stop || state.fault.is_some() {
                return false;
            }
            state.fault = Some(fault.clone());
            Self::pending_notification(&mut state)
        };
        if let Some((sink, fault)) = notification {
            sink.notify(fault);
        }
        true
    }

    fn pending_notification(
        state: &mut PipeWireHealthState,
    ) -> Option<(RuntimeFaultSink, RuntimeCaptureFault)> {
        if state.normal_stop || !state.armed {
            return None;
        }
        let sink = state.fault_sink.clone()?;
        if state
            .notified_sink
            .as_ref()
            .is_some_and(|notified| notified.same_binding(&sink))
        {
            return None;
        }
        let fault = state.fault.clone()?;
        state.notified_sink = Some(sink.clone());
        Some((sink, fault))
    }

    pub(crate) fn arm(&self) {
        let notification = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.normal_stop {
                return;
            }
            state.armed = true;
            Self::pending_notification(&mut state)
        };
        if let Some((sink, fault)) = notification {
            sink.notify(fault);
        }
    }

    pub(crate) fn rebind_and_arm(&self, fault_sink: RuntimeFaultSink) {
        let notification = {
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            if state.normal_stop {
                return;
            }
            state.fault_sink = Some(fault_sink);
            state.armed = true;
            Self::pending_notification(&mut state)
        };
        if let Some((sink, fault)) = notification {
            sink.notify(fault);
        }
    }

    pub(crate) fn attempt_id(&self) -> Option<crate::daemon_lifecycle::CaptureAttemptId> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.fault_sink.as_ref().map(RuntimeFaultSink::attempt_id))
    }

    pub(crate) fn mark_normal_stop(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.normal_stop = true;
        state.armed = false;
        state.fault_sink = None;
    }

    pub(crate) fn fault(&self) -> Option<String> {
        self.state.lock().ok().and_then(|state| {
            state.fault.as_ref().map(|fault| match fault {
                RuntimeCaptureFault::DeviceDisconnected(message)
                | RuntimeCaptureFault::BackendFault(message) => message.clone(),
            })
        })
    }
}

enum RuntimeFaultEvent {
    Core {
        id: u32,
        seq: i32,
        result: i32,
        message: String,
    },
    Link(String),
    StreamError(String),
    StreamUnconnected,
}

fn observe_runtime_fault(health: &PipeWireHealth, event: RuntimeFaultEvent) -> bool {
    let fault = match event {
        RuntimeFaultEvent::Core {
            id,
            seq,
            result,
            message,
        } => RuntimeCaptureFault::BackendFault(format!(
            "PipeWire core/proxy error (id={id}, seq={seq}, result={result}): {message}"
        )),
        RuntimeFaultEvent::Link(message) => {
            RuntimeCaptureFault::BackendFault(format!("PipeWire link error: {message}"))
        }
        RuntimeFaultEvent::StreamError(message) => {
            RuntimeCaptureFault::BackendFault(format!("PipeWire stream error: {message}"))
        }
        RuntimeFaultEvent::StreamUnconnected => RuntimeCaptureFault::DeviceDisconnected(
            "PipeWire stream became unconnected".to_string(),
        ),
    };
    health.record_fatal(fault)
}

#[cfg(test)]
pub(crate) fn observe_stream_unconnected_for_test(health: &PipeWireHealth) -> bool {
    observe_runtime_fault(health, RuntimeFaultEvent::StreamUnconnected)
}

enum PipeWireCommand {
    Stop,
}

fn await_pipewire_startup(
    ready_receiver: &mpsc::Receiver<std::result::Result<ResolvedTarget, PipeWireStartupError>>,
    join: &mut Option<JoinHandle<()>>,
) -> std::result::Result<ResolvedTarget, PipeWireStartupError> {
    match ready_receiver.recv() {
        Ok(Ok(resolved)) => Ok(resolved),
        Ok(Err(error)) => {
            if let Some(worker) = join.take() {
                let _ = worker.join();
            }
            Err(error)
        }
        Err(_) => {
            if let Some(worker) = join.take() {
                let _ = worker.join();
            }
            Err(PipeWireStartupError::Capture(LambError::Capture(
                "PipeWire capture thread exited before startup".to_string(),
            )))
        }
    }
}

impl PipeWireCapture {
    pub fn start(
        cfg: PipeWireCaptureConfig,
        params: CaptureRuntimeParams,
    ) -> Result<(Self, CaptureRuntime)> {
        let resolved = resolve_target(&cfg)?;
        Self::start_with_resolved(cfg, resolved, params, RuntimeFaultSink::default())
            .map_err(PipeWireStartupError::into_lamb_error)
    }

    pub(crate) fn start_with_resolved(
        cfg: PipeWireCaptureConfig,
        resolved: ResolvedTarget,
        params: CaptureRuntimeParams,
        fault_sink: RuntimeFaultSink,
    ) -> std::result::Result<(Self, CaptureRuntime), PipeWireStartupError> {
        let (runtime, ingress) =
            CaptureRuntime::build(params, resolved.sample_rate, resolved.channels)?;
        let resolved_for_thread = resolved.clone();
        let sample_rate = resolved.sample_rate;
        let channel_count = resolved.channels;
        let (stop_sender, stop_receiver) = pipewire::channel::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let health = PipeWireHealth::unarmed_with_fault_sink(fault_sink);
        let health_for_thread = health.clone();

        let mut join = Some(thread::spawn(move || {
            if let Err(err) = run_pipewire_stream_loop(
                cfg,
                resolved_for_thread,
                ingress,
                stop_receiver,
                ready_sender.clone(),
                health_for_thread,
            ) {
                let _ = ready_sender.send(Err(err));
            }
        }));

        match await_pipewire_startup(&ready_receiver, &mut join) {
            Ok(fresh_resolved) => Ok((
                Self {
                    resolved: fresh_resolved,
                    sample_rate,
                    channel_count,
                    stop_sender,
                    health,
                    join,
                },
                runtime,
            )),
            Err(error) => Err(error),
        }
    }

    pub fn resolved_target(&self) -> &ResolvedTarget {
        &self.resolved
    }

    pub(crate) fn runtime_error(&self) -> Option<String> {
        self.health.fault()
    }

    pub(crate) fn health(&self) -> PipeWireHealth {
        self.health.clone()
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.health.mark_normal_stop();
        let _ = self.stop_sender.send(PipeWireCommand::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub(crate) fn resolve_target_typed(
    cfg: &PipeWireCaptureConfig,
) -> std::result::Result<ResolvedTarget, TargetResolutionError> {
    let graph = discover_available_graph()
        .map_err(|error| TargetResolutionError::BackendUnavailable(capture_error_message(error)))?;
    resolve_target_from_graph_typed(cfg, &graph.nodes, &graph.ports)
}

pub fn resolve_target(cfg: &PipeWireCaptureConfig) -> Result<ResolvedTarget> {
    resolve_target_typed(cfg).map_err(|error| LambError::Capture(error.to_string()))
}

pub fn resolve_target_from_graph(
    cfg: &PipeWireCaptureConfig,
    nodes: &[AvailableNode],
    ports: &[AvailablePort],
) -> Result<ResolvedTarget> {
    resolve_target_from_graph_typed(cfg, nodes, ports)
        .map_err(|error| LambError::Capture(error.to_string()))
}

pub(crate) fn resolve_target_from_graph_typed(
    cfg: &PipeWireCaptureConfig,
    nodes: &[AvailableNode],
    ports: &[AvailablePort],
) -> std::result::Result<ResolvedTarget, TargetResolutionError> {
    let selected = if let Some(target) = cfg.target.as_deref() {
        let node = nodes
            .iter()
            .find(|node| node_matches_target(node, target))
            .ok_or_else(|| TargetResolutionError::TargetMissing(target.to_string()))?;
        if !is_input_source_node(node) {
            return Err(TargetResolutionError::InvalidSelector(
                "target is not an input/source node".to_string(),
            ));
        }
        node
    } else {
        nodes
            .iter()
            .find(|node| is_input_source_node(node))
            .ok_or_else(|| {
                TargetResolutionError::TargetMissing(
                    "no PipeWire input/source node found".to_string(),
                )
            })?
    };

    let source_ports = resolve_source_ports_typed(cfg, selected, nodes, ports)?;
    resolved_from_node(cfg, selected, source_ports)
        .map_err(|error| TargetResolutionError::InvalidSelector(capture_error_message(error)))
}

fn validate_startup_reresolution(
    expected: &ResolvedTarget,
    fresh: ResolvedTarget,
) -> std::result::Result<ResolvedTarget, TargetResolutionError> {
    if &fresh == expected {
        Ok(fresh)
    } else {
        Err(TargetResolutionError::TargetChanged(format!(
            "expected {expected:?}, found {fresh:?}"
        )))
    }
}

fn capture_error_message(error: LambError) -> String {
    match error {
        LambError::Capture(message) => message,
        error => error.to_string(),
    }
}

pub fn process_interleaved_f32_chunk(
    bytes: &[u8],
    offset: u32,
    size: u32,
    stride: i32,
    channels: u32,
    ingress: &CaptureIngress,
) -> Result<()> {
    if channels == 0 {
        return Err(LambError::Capture(
            "PipeWire buffer has zero channels".to_string(),
        ));
    }
    let frame_bytes = channels
        .checked_mul(std::mem::size_of::<f32>() as u32)
        .ok_or_else(|| LambError::Capture("PipeWire frame size overflow".to_string()))?;
    if stride != frame_bytes as i32 {
        return Err(LambError::Capture(format!(
            "unsupported PipeWire stride {stride}; expected {frame_bytes} for interleaved f32"
        )));
    }
    if !size.is_multiple_of(frame_bytes) {
        return Err(LambError::Capture(
            "PipeWire chunk size is not whole interleaved f32 frames".to_string(),
        ));
    }

    let start = usize::try_from(offset)
        .map_err(|_| LambError::Capture("PipeWire chunk offset overflow".to_string()))?;
    let len = usize::try_from(size)
        .map_err(|_| LambError::Capture("PipeWire chunk size overflow".to_string()))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| LambError::Capture("PipeWire chunk range overflow".to_string()))?;
    let payload = bytes.get(start..end).ok_or_else(|| {
        LambError::Capture("PipeWire chunk range exceeds mapped buffer".to_string())
    })?;
    if payload.as_ptr().align_offset(std::mem::align_of::<f32>()) != 0 {
        return Err(LambError::Capture(
            "PipeWire f32 payload is not aligned".to_string(),
        ));
    }
    if !cfg!(target_endian = "little") {
        return Err(LambError::Capture(
            "F32LE capture requires a little-endian target".to_string(),
        ));
    }

    let samples =
        unsafe { std::slice::from_raw_parts(payload.as_ptr().cast::<f32>(), payload.len() / 4) };
    ingress.try_push_interleaved(samples, channels).map(|_| ())
}

fn resolved_from_node(
    cfg: &PipeWireCaptureConfig,
    node: &AvailableNode,
    source_ports: Vec<ResolvedSourcePort>,
) -> Result<ResolvedTarget> {
    let channels = u32::try_from(source_ports.len()).map_err(|_| {
        LambError::Validation("capturePorts exceeds supported channel count".to_string())
    })?;
    if channels == 0 {
        return Err(LambError::Capture(
            "resolved PipeWire source has zero channels".to_string(),
        ));
    }
    let sample_rate = node.sample_rate.unwrap_or(cfg.sample_rate);
    if sample_rate == 0 {
        return Err(LambError::Capture(
            "resolved PipeWire source has zero sample rate".to_string(),
        ));
    }
    let format = node.format.clone().unwrap_or_else(|| "F32LE".to_string());
    if format != "F32LE" {
        return Err(LambError::Capture(format!(
            "unsupported PipeWire format {format}; expected F32LE"
        )));
    }

    Ok(ResolvedTarget {
        id: Some(node.id),
        name: node
            .name
            .clone()
            .unwrap_or_else(|| format!("node-{}", node.id)),
        description: node.description.clone(),
        channels,
        sample_rate,
        format,
        source_ports,
        durable_live_key: durable_live_key(node),
    })
}

fn durable_live_key(node: &AvailableNode) -> Option<(LiveDeviceKeyKind, String)> {
    [
        (
            LiveDeviceKeyKind::HardwareSerial,
            node.hardware_serial.as_deref(),
        ),
        (LiveDeviceKeyKind::ObjectPath, node.object_path.as_deref()),
        (LiveDeviceKeyKind::NodeName, node.name.as_deref()),
    ]
    .into_iter()
    .find_map(|(kind, value)| {
        let value = value?.trim();
        (!value.is_empty()).then(|| (kind, value.to_string()))
    })
}

fn is_audio_port(port: &AvailablePort) -> bool {
    port.format_dsp
        .as_deref()
        .map(str::trim)
        .map(|format| {
            format
                .split_whitespace()
                .last()
                .is_some_and(|word| word.eq_ignore_ascii_case("audio"))
        })
        .unwrap_or(false)
        || port
            .audio_format
            .as_deref()
            .is_some_and(|format| !format.trim().is_empty())
}

pub fn plan_explicit_links(
    sources: &[ResolvedSourcePort],
    stream_node_id: u32,
    ports: &[AvailablePort],
) -> Result<Vec<PlannedLink>> {
    let mut destinations: Vec<_> = ports
        .iter()
        .filter(|port| {
            port.node_id == Some(stream_node_id) && port.direction.as_deref() == Some("in")
        })
        .collect();

    for port in &destinations {
        if port.object_type != "PipeWire:Interface:Port" {
            return Err(LambError::Capture(format!(
                "destination port {} has object type '{}'; expected 'PipeWire:Interface:Port'",
                port.id, port.object_type
            )));
        }
        if port.direction.as_deref() != Some("in") {
            return Err(LambError::Capture(format!(
                "destination port {} has direction '{}'; expected 'in'",
                port.id,
                port.direction.as_deref().unwrap_or("<missing>")
            )));
        }
        if !is_audio_port(port) {
            return Err(LambError::Capture(format!(
                "destination port {} is not audio",
                port.id
            )));
        }
        if port.port_id.is_none() {
            return Err(LambError::Capture(format!(
                "destination port {} is missing port.id",
                port.id
            )));
        }
    }

    destinations.sort_by_key(|port| port.port_id.expect("validated destination port.id"));
    for pair in destinations.windows(2) {
        if pair[0].port_id == pair[1].port_id {
            return Err(LambError::Capture(format!(
                "duplicate destination port.id {}",
                pair[0].port_id.expect("validated destination port.id")
            )));
        }
    }
    if destinations.len() != sources.len() {
        return Err(LambError::Capture(format!(
            "destination port count {} does not match source count {}",
            destinations.len(),
            sources.len()
        )));
    }

    Ok(sources
        .iter()
        .zip(destinations)
        .map(|(source, destination)| PlannedLink {
            output_node_id: source.node_id,
            output_port_id: source.global_id,
            input_node_id: stream_node_id,
            input_port_id: destination.id,
        })
        .collect())
}

#[derive(Debug, PartialEq, Eq)]
enum DestinationReadiness {
    Pending,
    Ready,
    Failed(String),
}

fn destination_readiness(
    graph: &AvailableGraph,
    stream_node_id: u32,
    expected: usize,
) -> DestinationReadiness {
    let inputs: Vec<_> = graph
        .ports
        .iter()
        .filter(|port| {
            port.node_id == Some(stream_node_id) && port.direction.as_deref() == Some("in")
        })
        .collect();
    let mut ids = HashSet::new();
    for port in inputs {
        let Some(local_id) = port.port_id else {
            return DestinationReadiness::Failed(
                "PipeWire stream input port is missing port.id".to_string(),
            );
        };
        if port.object_type != "PipeWire:Interface:Port" || !is_audio_port(port) {
            return DestinationReadiness::Failed(
                "PipeWire stream has malformed or duplicate input ports".to_string(),
            );
        }
        if !ids.insert(local_id) {
            return DestinationReadiness::Failed(
                "PipeWire stream has malformed or duplicate input ports".to_string(),
            );
        }
    }
    if ids.len() < expected {
        return DestinationReadiness::Pending;
    }
    if ids.len() > expected {
        return DestinationReadiness::Failed(format!(
            "destination input port count {} exceeds source count {expected}",
            ids.len()
        ));
    }
    DestinationReadiness::Ready
}

fn node_display_name(nodes: &[AvailableNode], node_id: u32) -> String {
    nodes
        .iter()
        .find(|node| node.id == node_id)
        .and_then(|node| node.name.as_ref().or(node.description.as_ref()))
        .cloned()
        .unwrap_or_else(|| format!("node-{node_id}"))
}

fn resolve_source_ports_typed(
    cfg: &PipeWireCaptureConfig,
    selected: &AvailableNode,
    nodes: &[AvailableNode],
    ports: &[AvailablePort],
) -> std::result::Result<Vec<ResolvedSourcePort>, TargetResolutionError> {
    let target_name = node_display_name(nodes, selected.id);
    let mut configured_sources = HashSet::new();
    for configured in &cfg.capture_ports {
        if !configured_sources.insert(configured.source.as_str()) {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "capture source '{}' is configured more than once",
                configured.source
            )));
        }
    }

    let mut resolved = Vec::with_capacity(cfg.capture_ports.len());
    for configured in &cfg.capture_ports {
        let named_matches: Vec<_> = ports
            .iter()
            .filter(|port| port.name.as_deref() == Some(configured.source.as_str()))
            .collect();
        let selected_matches: Vec<_> = named_matches
            .iter()
            .copied()
            .filter(|port| port.node_id == Some(selected.id))
            .collect();

        if selected_matches.len() > 1 {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "source port '{}' is ambiguous on target '{}'",
                configured.source, target_name
            )));
        }

        let port = if let Some(port) = selected_matches.first().copied() {
            port
        } else if named_matches.iter().any(|port| port.node_id.is_none()) {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "source port '{}' is missing node.id",
                configured.source
            )));
        } else if let Some(other_node_id) = named_matches
            .iter()
            .filter_map(|port| port.node_id)
            .min_by_key(|node_id| (node_display_name(nodes, *node_id), *node_id))
        {
            return Err(TargetResolutionError::PortMissing(format!(
                "source port '{}' belongs to node '{}', expected '{}'",
                configured.source,
                node_display_name(nodes, other_node_id),
                target_name
            )));
        } else {
            let mut available: Vec<_> = ports
                .iter()
                .filter(|port| {
                    port.object_type == "PipeWire:Interface:Port"
                        && port.node_id == Some(selected.id)
                        && port.direction.as_deref() == Some("out")
                        && port.port_id.is_some()
                        && is_audio_port(port)
                })
                .filter_map(|port| port.name.clone())
                .collect();
            available.sort();
            available.dedup();
            let available = if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            };
            return Err(TargetResolutionError::PortMissing(format!(
                "source port '{}' was not found on target '{}'; available target audio outputs: {}",
                configured.source, target_name, available
            )));
        };

        if port.object_type != "PipeWire:Interface:Port" {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "source port '{}' has object type '{}'; expected 'PipeWire:Interface:Port'",
                configured.source, port.object_type
            )));
        }
        let node_id = port.node_id.ok_or_else(|| {
            TargetResolutionError::InvalidSelector(format!(
                "source port '{}' is missing node.id",
                configured.source
            ))
        })?;
        if port.direction.as_deref() != Some("out") {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "source port '{}' has direction '{}'; expected 'out'",
                configured.source,
                port.direction.as_deref().unwrap_or("<missing>")
            )));
        }
        if !is_audio_port(port) {
            return Err(TargetResolutionError::InvalidSelector(format!(
                "source port '{}' is not audio (format.dsp='{}')",
                configured.source,
                port.format_dsp.as_deref().unwrap_or("<missing>")
            )));
        }
        let port_id = port.port_id.ok_or_else(|| {
            TargetResolutionError::InvalidSelector(format!(
                "source port '{}' is missing port.id",
                configured.source
            ))
        })?;
        resolved.push(ResolvedSourcePort {
            global_id: port.id,
            node_id,
            port_id,
            name: configured.source.clone(),
        });
    }

    Ok(resolved)
}

fn node_matches_target(node: &AvailableNode, target: &str) -> bool {
    if target.parse::<u32>().ok() == Some(node.id) {
        return true;
    }
    node.name.as_deref() == Some(target) || node.description.as_deref() == Some(target)
}

fn is_input_source_node(node: &AvailableNode) -> bool {
    if node.object_type != "PipeWire:Interface:Node" && node.object_type != "Node" {
        return false;
    }
    let media_class = node.media_class.as_deref().unwrap_or_default();
    if media_class != "Audio/Source" && media_class != "Audio/Input" {
        return false;
    }
    let name = node
        .name
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let description = node
        .description
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    !name.ends_with(".monitor")
        && !media_class.to_ascii_lowercase().contains("monitor")
        && !description.contains("monitor")
}

fn discover_available_graph() -> Result<AvailableGraph> {
    use pipewire as pw;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pipewire_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pipewire_error)?;
    let core = context.connect_rc(None).map_err(pipewire_error)?;
    let registry = core.get_registry().map_err(pipewire_error)?;
    let startup_wake = Rc::new(RefCell::new(None));
    let (graph, _registry_listener) = register_graph_listener(&registry, &startup_wake);
    let health = PipeWireHealth::default();
    let _core_listener = register_core_error_listener(&core, &health, &startup_wake);
    synchronize_core(&core, &mainloop, &health)?;

    let discovered = graph.borrow().clone();
    Ok(discovered)
}

type StartupWake = Rc<RefCell<Option<pipewire::main_loop::MainLoopRc>>>;

fn wake_startup_wait(startup_wake: &StartupWake) {
    if let Some(mainloop) = startup_wake.borrow().clone() {
        mainloop.quit();
    }
}

fn register_graph_listener(
    registry: &pipewire::registry::RegistryBox<'_>,
    startup_wake: &StartupWake,
) -> (Rc<RefCell<AvailableGraph>>, pipewire::registry::Listener) {
    let graph = Rc::new(RefCell::new(AvailableGraph::default()));
    let graph_for_global = Rc::clone(&graph);
    let graph_for_remove = Rc::clone(&graph);
    let wake_for_global = Rc::clone(startup_wake);
    let wake_for_remove = Rc::clone(startup_wake);
    let listener = registry
        .add_listener_local()
        .global(move |global| {
            let mut graph = graph_for_global.borrow_mut();
            match global.type_.to_str() {
                "PipeWire:Interface:Node" => graph.nodes.push(available_node_from_global(global)),
                "PipeWire:Interface:Port" => graph.ports.push(available_port_from_global(global)),
                "PipeWire:Interface:Factory" => {
                    let props = global.props.as_ref().map(|props| props.as_ref());
                    if string_prop(props, "factory.type.name").as_deref()
                        == Some(pipewire::types::ObjectType::Link.to_str())
                    {
                        if let Some(name) = string_prop(props, "factory.name") {
                            record_link_factory(&mut graph, global.id, name);
                        }
                    }
                }
                _ => {}
            }
            drop(graph);
            wake_startup_wait(&wake_for_global);
        })
        .global_remove(move |id| {
            let mut graph = graph_for_remove.borrow_mut();
            remove_graph_global(&mut graph, id);
            drop(graph);
            wake_startup_wait(&wake_for_remove);
        })
        .register();
    (graph, listener)
}

fn record_link_factory(graph: &mut AvailableGraph, global_id: u32, name: String) {
    graph.link_factory = Some(LinkFactory { global_id, name });
}

fn remove_graph_global(graph: &mut AvailableGraph, id: u32) {
    graph.nodes.retain(|node| node.id != id);
    graph.ports.retain(|port| port.id != id);
    if graph
        .link_factory
        .as_ref()
        .is_some_and(|factory| factory.global_id == id)
    {
        graph.link_factory = None;
    }
}

fn register_core_error_listener(
    core: &pipewire::core::Core,
    health: &PipeWireHealth,
    startup_wake: &StartupWake,
) -> pipewire::core::Listener {
    let health = health.clone();
    let startup_wake = Rc::clone(startup_wake);
    core.add_listener_local()
        .error(move |id, seq, res, message| {
            observe_runtime_fault(
                &health,
                RuntimeFaultEvent::Core {
                    id,
                    seq,
                    result: res,
                    message: message.to_string(),
                },
            );
            wake_startup_wait(&startup_wake);
        })
        .register()
}

fn synchronize_core(
    core: &pipewire::core::Core,
    mainloop: &pipewire::main_loop::MainLoopRc,
    health: &PipeWireHealth,
) -> Result<()> {
    if let Some(error) = health.fault() {
        return Err(LambError::Capture(error));
    }
    let done = Rc::new(Cell::new(false));
    let pending = Rc::new(Cell::new(None));
    let done_for_listener = Rc::clone(&done);
    let pending_for_listener = Rc::clone(&pending);
    let mainloop_for_listener = mainloop.clone();
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pipewire::core::PW_ID_CORE && pending_for_listener.get() == Some(seq) {
                done_for_listener.set(true);
                mainloop_for_listener.quit();
            }
        })
        .register();
    pending.set(Some(core.sync(0).map_err(pipewire_error)?));
    while !done.get() {
        mainloop.run();
    }
    if let Some(error) = health.fault() {
        return Err(LambError::Capture(error));
    }
    Ok(())
}

fn available_node_from_global(
    global: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>,
) -> AvailableNode {
    let props = global.props.as_ref().map(|props| props.as_ref());
    AvailableNode {
        id: global.id,
        object_type: global.type_.to_string(),
        media_class: string_prop(props, *pipewire::keys::MEDIA_CLASS),
        name: string_prop(props, *pipewire::keys::NODE_NAME),
        hardware_serial: string_prop(props, "device.serial"),
        object_path: string_prop(props, "object.path"),
        description: string_prop(props, *pipewire::keys::NODE_DESCRIPTION),
        channels: u32_prop(props, *pipewire::keys::AUDIO_CHANNELS),
        sample_rate: u32_prop(props, "audio.rate"),
        format: string_prop(props, *pipewire::keys::AUDIO_FORMAT)
            .or_else(|| string_prop(props, *pipewire::keys::FORMAT_DSP)),
    }
}

fn available_port_from_global(
    global: &pipewire::registry::GlobalObject<&pipewire::spa::utils::dict::DictRef>,
) -> AvailablePort {
    let props = global.props.as_ref().map(|props| props.as_ref());
    AvailablePort {
        id: global.id,
        object_type: global.type_.to_string(),
        node_id: u32_prop(props, *pipewire::keys::NODE_ID),
        direction: string_prop(props, *pipewire::keys::PORT_DIRECTION),
        port_id: u32_prop(props, *pipewire::keys::PORT_ID),
        name: string_prop(props, *pipewire::keys::PORT_NAME),
        format_dsp: string_prop(props, *pipewire::keys::FORMAT_DSP),
        audio_format: string_prop(props, *pipewire::keys::AUDIO_FORMAT),
    }
}

fn string_prop(props: Option<&pipewire::spa::utils::dict::DictRef>, key: &str) -> Option<String> {
    props.and_then(|props| props.get(key)).map(str::to_string)
}

fn u32_prop(props: Option<&pipewire::spa::utils::dict::DictRef>, key: &str) -> Option<u32> {
    props
        .and_then(|props| props.get(key))
        .and_then(|value| value.parse::<u32>().ok())
}

fn pipewire_error(err: pipewire::Error) -> LambError {
    LambError::Capture(format!("PipeWire error: {err}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum StreamBindingReadiness {
    #[default]
    Pending,
    Ready(u32),
    Failed(String),
}

enum StreamBindingEvent {
    Paused,
    Error(String),
    Unconnected,
}

impl StreamBindingReadiness {
    fn observe(&mut self, event: StreamBindingEvent, node_id: u32) -> bool {
        let previous = self.clone();
        match event {
            StreamBindingEvent::Error(message) if !matches!(self, Self::Failed(_)) => {
                *self = Self::Failed(format!("PipeWire stream error during startup: {message}"));
            }
            StreamBindingEvent::Unconnected if !matches!(self, Self::Failed(_)) => {
                *self =
                    Self::Failed("PipeWire stream became unconnected during startup".to_string());
            }
            StreamBindingEvent::Paused
                if matches!(self, Self::Pending) && node_id != pipewire::constants::ID_ANY =>
            {
                *self = Self::Ready(node_id);
            }
            _ => {}
        }
        *self != previous
    }

    fn timeout(&mut self) {
        if matches!(self, Self::Pending) {
            *self = Self::Failed("PipeWire stream binding timed out after 5 seconds".to_string());
        }
    }

    fn into_result(self) -> std::result::Result<u32, String> {
        match self {
            Self::Ready(node_id) => Ok(node_id),
            Self::Failed(error) => Err(error),
            Self::Pending => Err("PipeWire stream binding is still pending".to_string()),
        }
    }
}

fn ensure_stream_startup_ready(readiness: &StreamBindingReadiness) -> Result<u32> {
    readiness.clone().into_result().map_err(LambError::Capture)
}

fn evaluate_destination_startup(
    graph: &AvailableGraph,
    stream_node_id: u32,
    expected: usize,
    stream_readiness: &StreamBindingReadiness,
    startup_error: Option<String>,
    timed_out: bool,
) -> Result<DestinationReadiness> {
    if let Some(error) = startup_error {
        return Err(LambError::Capture(error));
    }
    ensure_stream_startup_ready(stream_readiness)?;
    if timed_out {
        return Err(LambError::Capture(
            "PipeWire destination ports timed out after 5 seconds".to_string(),
        ));
    }
    Ok(destination_readiness(graph, stream_node_id, expected))
}

fn run_pipewire_stream_loop(
    cfg: PipeWireCaptureConfig,
    resolved: ResolvedTarget,
    ingress: CaptureIngress,
    stop_receiver: pipewire::channel::Receiver<PipeWireCommand>,
    ready_sender: mpsc::Sender<std::result::Result<ResolvedTarget, PipeWireStartupError>>,
    health: PipeWireHealth,
) -> std::result::Result<(), PipeWireStartupError> {
    use pipewire as pw;
    use pw::properties::properties;
    use pw::spa;
    use spa::param::format::{MediaSubtype, MediaType};
    use spa::param::format_utils;
    use spa::pod::Pod;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pipewire_error)?;
    let mainloop_for_stop = mainloop.clone();
    let _stop_listener = stop_receiver.attach(mainloop.loop_(), move |command| match command {
        PipeWireCommand::Stop => mainloop_for_stop.quit(),
    });
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pipewire_error)?;
    let core = context.connect_rc(None).map_err(pipewire_error)?;
    let registry = core.get_registry().map_err(pipewire_error)?;
    let startup_wake = Rc::new(RefCell::new(None));
    let runtime_wake = Rc::new(RefCell::new(Some(mainloop.clone())));
    let (graph, _registry_listener) = register_graph_listener(&registry, &startup_wake);
    let _core_listener = register_core_error_listener(&core, &health, &runtime_wake);
    synchronize_core(&core, &mainloop, &health)?;

    let graph_snapshot = graph.borrow().clone();
    let fresh_resolved =
        resolve_target_from_graph_typed(&cfg, &graph_snapshot.nodes, &graph_snapshot.ports)?;
    let fresh_resolved = validate_startup_reresolution(&resolved, fresh_resolved)?;
    let link_factory = graph_snapshot
        .link_factory
        .map(|factory| factory.name)
        .ok_or_else(|| {
            LambError::Capture(
                "PipeWire link factory was not advertised by the registry".to_string(),
            )
        })?;

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    if cfg.dont_remix {
        props.insert(*pw::keys::STREAM_DONT_REMIX, "true");
    }
    if let Some(latency) = cfg.latency.as_ref() {
        props.insert(*pw::keys::NODE_LATENCY, latency.clone());
    }

    let stream =
        pw::stream::StreamBox::new(&core, "lamb-capture", props).map_err(pipewire_error)?;
    let stream_readiness = Rc::new(RefCell::new(StreamBindingReadiness::default()));
    let readiness_for_state_change = Rc::clone(&stream_readiness);
    let mainloop_for_state_change = mainloop.clone();
    let health_for_state_change = health.clone();
    let user_data = PipeWireStreamData {
        format: spa::param::audio::AudioInfoRaw::new(),
        channels: resolved.channels,
        ingress,
    };
    let _stream_listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |stream, _, _, state| {
            let (event, fault) = match state {
                pw::stream::StreamState::Paused => (StreamBindingEvent::Paused, None),
                pw::stream::StreamState::Error(message) => (
                    StreamBindingEvent::Error(message.clone()),
                    Some(RuntimeFaultEvent::StreamError(message)),
                ),
                pw::stream::StreamState::Unconnected => (
                    StreamBindingEvent::Unconnected,
                    Some(RuntimeFaultEvent::StreamUnconnected),
                ),
                _ => return,
            };
            let mut readiness = readiness_for_state_change.borrow_mut();
            let changed = readiness.observe(event, stream.node_id());
            drop(readiness);
            let fatal = fault.is_some();
            if let Some(fault) = fault {
                observe_runtime_fault(&health_for_state_change, fault);
            }
            if changed || fatal {
                mainloop_for_state_change.quit();
            }
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            let _ = user_data.format.parse(param);
        })
        .process(|stream, user_data| {
            if user_data.format.format() != spa::param::audio::AudioFormat::F32LE {
                return;
            }
            if user_data.format.channels() != user_data.channels {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let chunk = data.chunk();
            let offset = chunk.offset();
            let size = chunk.size();
            let stride = chunk.stride();
            if let Some(bytes) = data.data() {
                let _ = process_interleaved_f32_chunk(
                    bytes,
                    offset,
                    size,
                    stride,
                    user_data.channels,
                    &user_data.ingress,
                );
            }
        })
        .register();

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(resolved.sample_rate);
    audio_info.set_channels(resolved.channels);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|err| LambError::Capture(format!("failed to serialize PipeWire format pod: {err:?}")))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values)
        .ok_or_else(|| LambError::Capture("failed to build PipeWire format pod".to_string()))?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::INACTIVE
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(pipewire_error)?;

    let stream_node_id = {
        let readiness_for_timeout = Rc::clone(&stream_readiness);
        let mainloop_for_timeout = mainloop.clone();
        let startup_timer = mainloop.loop_().add_timer(move |_| {
            readiness_for_timeout.borrow_mut().timeout();
            mainloop_for_timeout.quit();
        });
        startup_timer
            .update_timer(Some(Duration::from_secs(5)), None)
            .into_result()
            .map_err(|err| {
                LambError::Capture(format!("failed to start PipeWire binding timer: {err}"))
            })?;
        *startup_wake.borrow_mut() = Some(mainloop.clone());
        while matches!(*stream_readiness.borrow(), StreamBindingReadiness::Pending) {
            if let Some(error) = health.fault() {
                *startup_wake.borrow_mut() = None;
                return Err(LambError::Capture(error).into());
            }
            mainloop.run();
        }
        *startup_wake.borrow_mut() = None;
        if let Some(error) = health.fault() {
            return Err(LambError::Capture(error).into());
        }
        ensure_stream_startup_ready(&stream_readiness.borrow())?
    };

    synchronize_core(&core, &mainloop, &health)?;
    ensure_stream_startup_ready(&stream_readiness.borrow())?;
    {
        *startup_wake.borrow_mut() = Some(mainloop.clone());
        let destination_timed_out = Rc::new(Cell::new(false));
        let timed_out_for_timer = Rc::clone(&destination_timed_out);
        let mainloop_for_timer = mainloop.clone();
        let destination_timer = mainloop.loop_().add_timer(move |_| {
            timed_out_for_timer.set(true);
            mainloop_for_timer.quit();
        });
        destination_timer
            .update_timer(Some(Duration::from_secs(5)), None)
            .into_result()
            .map_err(|err| {
                LambError::Capture(format!("failed to start PipeWire destination timer: {err}"))
            })?;

        loop {
            let readiness = {
                let graph = graph.borrow();
                evaluate_destination_startup(
                    &graph,
                    stream_node_id,
                    fresh_resolved.source_ports.len(),
                    &stream_readiness.borrow(),
                    health.fault(),
                    destination_timed_out.get(),
                )
            };
            match readiness {
                Ok(DestinationReadiness::Ready) => break,
                Ok(DestinationReadiness::Failed(error)) => {
                    return Err(LambError::Capture(error).into());
                }
                Ok(DestinationReadiness::Pending) => mainloop.run(),
                Err(error) => return Err(error.into()),
            }
        }
        *startup_wake.borrow_mut() = None;
    }
    synchronize_core(&core, &mainloop, &health)?;
    ensure_stream_startup_ready(&stream_readiness.borrow())?;
    let plans = {
        let graph = graph.borrow();
        plan_explicit_links(&fresh_resolved.source_ports, stream_node_id, &graph.ports)?
    };
    let mut links = Vec::with_capacity(plans.len());
    let mut link_listeners = Vec::with_capacity(plans.len());
    for plan in plans {
        let properties = properties! {
            *pw::keys::LINK_OUTPUT_NODE => plan.output_node_id.to_string(),
            *pw::keys::LINK_OUTPUT_PORT => plan.output_port_id.to_string(),
            *pw::keys::LINK_INPUT_NODE => plan.input_node_id.to_string(),
            *pw::keys::LINK_INPUT_PORT => plan.input_port_id.to_string(),
        };
        let link = core
            .create_object::<pw::link::Link>(&link_factory, &properties)
            .map_err(pipewire_error)?;
        let link_health = health.clone();
        let mainloop_for_link_error = mainloop.clone();
        let listener = link
            .add_listener_local()
            .info(move |info| {
                if let pw::link::LinkState::Error(message) = info.state() {
                    observe_runtime_fault(
                        &link_health,
                        RuntimeFaultEvent::Link(message.to_string()),
                    );
                    mainloop_for_link_error.quit();
                }
            })
            .register();
        links.push(link);
        link_listeners.push(listener);
    }

    synchronize_core(&core, &mainloop, &health)?;
    ensure_stream_startup_ready(&stream_readiness.borrow())?;
    stream.set_active(true).map_err(pipewire_error)?;
    synchronize_core(&core, &mainloop, &health)?;
    ensure_stream_startup_ready(&stream_readiness.borrow())?;
    let _ = ready_sender.send(Ok(fresh_resolved));
    mainloop.run();
    drop((link_listeners, links));
    if let Some(error) = health.fault() {
        Err(LambError::Capture(error).into())
    } else {
        Ok(())
    }
}

struct PipeWireStreamData {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    channels: u32,
    ingress: CaptureIngress,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed_resolution_config(target: &str) -> PipeWireCaptureConfig {
        PipeWireCaptureConfig {
            target: Some(target.to_string()),
            capture_ports: vec![ConfiguredCapturePort {
                source: "capture_FL".to_string(),
                name: "left".to_string(),
            }],
            sample_rate: 48_000,
            dont_remix: true,
            latency: None,
        }
    }

    fn typed_resolution_source(media_class: &str) -> AvailableNode {
        AvailableNode {
            id: 7,
            object_type: "PipeWire:Interface:Node".to_string(),
            media_class: Some(media_class.to_string()),
            name: Some("studio-input".to_string()),
            hardware_serial: None,
            object_path: None,
            description: None,
            channels: Some(1),
            sample_rate: Some(48_000),
            format: Some("F32LE".to_string()),
        }
    }

    #[test]
    fn missing_target_is_typed_transient_absence() {
        let error = resolve_target_from_graph_typed(
            &typed_resolution_config("missing"),
            &[typed_resolution_source("Audio/Source")],
            &[],
        )
        .unwrap_err();

        assert!(matches!(error, TargetResolutionError::TargetMissing(_)));
    }

    #[test]
    fn invalid_target_selector_is_typed_permanent() {
        let error = resolve_target_from_graph_typed(
            &typed_resolution_config("studio-input"),
            &[typed_resolution_source("Audio/Sink")],
            &[],
        )
        .unwrap_err();

        assert!(matches!(error, TargetResolutionError::InvalidSelector(_)));
    }

    #[test]
    fn startup_reresolution_rejects_every_changed_live_identity_field() {
        let expected = ResolvedTarget {
            id: Some(7),
            name: "studio-input".to_string(),
            description: Some("Studio Input".to_string()),
            channels: 1,
            sample_rate: 48_000,
            format: "F32LE".to_string(),
            source_ports: vec![ResolvedSourcePort {
                global_id: 11,
                node_id: 7,
                port_id: 0,
                name: "capture_FL".to_string(),
            }],
            durable_live_key: Some((LiveDeviceKeyKind::HardwareSerial, "serial-1".to_string())),
        };
        let mut changed = Vec::new();
        let mut target = expected.clone();
        target.id = Some(8);
        changed.push(target);
        let mut target = expected.clone();
        target.durable_live_key = Some((LiveDeviceKeyKind::HardwareSerial, "serial-2".to_string()));
        changed.push(target);
        let mut target = expected.clone();
        target.source_ports[0].global_id = 12;
        changed.push(target);
        let mut target = expected.clone();
        target.source_ports[0].node_id = 8;
        changed.push(target);
        let mut target = expected.clone();
        target.source_ports[0].port_id = 1;
        changed.push(target);
        let mut target = expected.clone();
        target.source_ports[0].name = "capture_FR".to_string();
        changed.push(target);
        let mut target = expected.clone();
        target.sample_rate = 44_100;
        changed.push(target);
        let mut target = expected.clone();
        target.format = "S16LE".to_string();
        changed.push(target);
        let mut target = expected.clone();
        target.channels = 2;
        changed.push(target);

        for fresh in changed {
            let error = validate_startup_reresolution(&expected, fresh).unwrap_err();
            assert!(matches!(error, TargetResolutionError::TargetChanged(_)));
        }
        assert_eq!(
            validate_startup_reresolution(&expected, expected.clone()).unwrap(),
            expected
        );
    }

    #[test]
    fn unarmed_runtime_fault_replays_once_after_arm() {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&notifications);
        let sink = crate::daemon_lifecycle::RuntimeFaultSink::new(move |fault| {
            recorded.lock().unwrap().push(fault)
        });
        let health = PipeWireHealth::unarmed_with_fault_sink(sink);

        health.record_fatal(
            crate::daemon_lifecycle::RuntimeCaptureFault::DeviceDisconnected("gone".to_string()),
        );
        assert!(notifications.lock().unwrap().is_empty());

        health.arm();
        health.record_fatal(crate::daemon_lifecycle::RuntimeCaptureFault::BackendFault(
            "later".to_string(),
        ));

        assert_eq!(notifications.lock().unwrap().len(), 1);
    }

    #[test]
    fn normal_stop_emits_no_runtime_fault() {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&notifications);
        let sink = crate::daemon_lifecycle::RuntimeFaultSink::new(move |fault| {
            recorded.lock().unwrap().push(fault)
        });
        let health = PipeWireHealth::unarmed_with_fault_sink(sink);

        health.mark_normal_stop();
        health.arm();
        health.record_fatal(crate::daemon_lifecycle::RuntimeCaptureFault::BackendFault(
            "late callback".to_string(),
        ));

        assert!(notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn rebind_replays_recorded_fault_once_for_new_authority() {
        let old_notifications = Arc::new(Mutex::new(Vec::new()));
        let recorded_old = Arc::clone(&old_notifications);
        let old_sink = crate::daemon_lifecycle::RuntimeFaultSink::new(move |fault| {
            recorded_old.lock().unwrap().push(fault)
        });
        let new_notifications = Arc::new(Mutex::new(Vec::new()));
        let recorded_new = Arc::clone(&new_notifications);
        let new_sink = crate::daemon_lifecycle::RuntimeFaultSink::new(move |fault| {
            recorded_new.lock().unwrap().push(fault)
        });
        let health = PipeWireHealth::unarmed_with_fault_sink(old_sink);
        health.arm();
        health.record_fatal(crate::daemon_lifecycle::RuntimeCaptureFault::BackendFault(
            "raced fault".to_string(),
        ));

        health.rebind_and_arm(new_sink.clone());
        health.rebind_and_arm(new_sink);

        assert_eq!(old_notifications.lock().unwrap().len(), 1);
        assert_eq!(new_notifications.lock().unwrap().len(), 1);
    }

    #[test]
    fn startup_handshake_disconnect_joins_worker_before_returning_error() {
        struct WorkerLease(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for WorkerLease {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let live = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let worker_live = Arc::clone(&live);
        let (sender, receiver) =
            mpsc::channel::<std::result::Result<ResolvedTarget, PipeWireStartupError>>();
        let mut join = Some(thread::spawn(move || {
            let _lease = WorkerLease(worker_live);
            drop(sender);
        }));

        let error = await_pipewire_startup(&receiver, &mut join).unwrap_err();

        assert!(error.to_string().contains("exited before startup"));
        assert!(join.is_none());
        assert_eq!(live.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn unpublished_startup_callback_fault_joins_without_runtime_notification() {
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&notifications);
        let health = PipeWireHealth::unarmed_with_fault_sink(RuntimeFaultSink::new(move |fault| {
            recorded.lock().unwrap().push(fault);
        }));
        let worker_health = health.clone();
        let (sender, receiver) = mpsc::channel();
        let mut join = Some(thread::spawn(move || {
            worker_health.record_fatal(RuntimeCaptureFault::BackendFault(
                "callback during startup".to_string(),
            ));
            sender
                .send(Err(PipeWireStartupError::Capture(LambError::Capture(
                    "later startup failure".to_string(),
                ))))
                .unwrap();
        }));

        let error = await_pipewire_startup(&receiver, &mut join).unwrap_err();

        assert!(matches!(error, PipeWireStartupError::Capture(_)));
        assert!(join.is_none());
        assert!(notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn startup_handshake_preserves_typed_second_resolution_error() {
        let (sender, receiver) = mpsc::channel();
        let mut join = Some(thread::spawn(move || {
            sender
                .send(Err(PipeWireStartupError::Resolution(
                    TargetResolutionError::TargetMissing("vanished".to_string()),
                )))
                .unwrap();
        }));

        let error = await_pipewire_startup(&receiver, &mut join).unwrap_err();

        assert!(matches!(
            error,
            PipeWireStartupError::Resolution(TargetResolutionError::TargetMissing(message))
                if message == "vanished"
        ));
        assert!(join.is_none());
    }

    #[test]
    fn fatal_core_link_and_stream_events_share_the_first_fault_and_wake_path() {
        for (first, second, expected) in [
            (
                RuntimeFaultEvent::Core {
                    id: 1,
                    seq: 2,
                    result: -32,
                    message: "core lost".to_string(),
                },
                RuntimeFaultEvent::Link("link lost".to_string()),
                "PipeWire core/proxy error (id=1, seq=2, result=-32): core lost",
            ),
            (
                RuntimeFaultEvent::Link("link lost".to_string()),
                RuntimeFaultEvent::StreamUnconnected,
                "PipeWire link error: link lost",
            ),
            (
                RuntimeFaultEvent::StreamError("stream lost".to_string()),
                RuntimeFaultEvent::Core {
                    id: 1,
                    seq: 2,
                    result: -32,
                    message: "core lost".to_string(),
                },
                "PipeWire stream error: stream lost",
            ),
        ] {
            let health = PipeWireHealth::default();
            assert!(observe_runtime_fault(&health, first));
            assert!(!observe_runtime_fault(&health, second));
            assert_eq!(health.fault().as_deref(), Some(expected));
        }
    }

    #[test]
    fn removing_advertised_link_factory_clears_only_matching_factory() {
        let mut graph = AvailableGraph {
            link_factory: Some(LinkFactory {
                global_id: 44,
                name: "link-factory".to_string(),
            }),
            ..AvailableGraph::default()
        };

        remove_graph_global(&mut graph, 43);
        assert_eq!(
            graph.link_factory.as_ref().map(|factory| factory.global_id),
            Some(44)
        );

        record_link_factory(&mut graph, 45, "replacement-link-factory".to_string());
        assert_eq!(
            graph.link_factory.as_ref().map(|factory| factory.global_id),
            Some(45)
        );

        remove_graph_global(&mut graph, 44);
        assert_eq!(
            graph.link_factory.as_ref().map(|factory| factory.global_id),
            Some(45)
        );

        remove_graph_global(&mut graph, 45);
        assert_eq!(graph.link_factory, None);
    }

    #[test]
    fn stream_binding_readiness_stays_pending_without_a_terminal_stream_state() {
        assert_eq!(
            StreamBindingReadiness::default(),
            StreamBindingReadiness::Pending
        );
    }

    #[test]
    fn stream_binding_readiness_requires_a_real_node_id_when_paused() {
        let mut readiness = StreamBindingReadiness::default();
        readiness.observe(StreamBindingEvent::Paused, pipewire::constants::ID_ANY);
        assert_eq!(readiness, StreamBindingReadiness::Pending);

        readiness.observe(StreamBindingEvent::Paused, 903);
        assert_eq!(readiness, StreamBindingReadiness::Ready(903));
    }

    #[test]
    fn stream_binding_readiness_reports_stream_errors_and_disconnection() {
        let mut readiness = StreamBindingReadiness::default();
        readiness.observe(
            StreamBindingEvent::Error("permission denied".to_string()),
            pipewire::constants::ID_ANY,
        );
        assert_eq!(
            readiness.into_result(),
            Err("PipeWire stream error during startup: permission denied".to_string())
        );

        let mut readiness = StreamBindingReadiness::default();
        readiness.observe(StreamBindingEvent::Unconnected, pipewire::constants::ID_ANY);
        assert_eq!(
            readiness.into_result(),
            Err("PipeWire stream became unconnected during startup".to_string())
        );
    }

    #[test]
    fn stream_binding_readiness_reports_timeout() {
        let mut readiness = StreamBindingReadiness::default();
        readiness.timeout();
        assert_eq!(
            readiness.into_result(),
            Err("PipeWire stream binding timed out after 5 seconds".to_string())
        );
    }

    #[test]
    fn stream_binding_readiness_replaces_ready_with_terminal_error() {
        let mut readiness = StreamBindingReadiness::Ready(903);
        readiness.observe(
            StreamBindingEvent::Error("server lost".to_string()),
            pipewire::constants::ID_ANY,
        );
        assert_eq!(
            readiness.into_result(),
            Err("PipeWire stream error during startup: server lost".to_string())
        );
    }

    #[test]
    fn stream_binding_readiness_replaces_ready_with_unconnected() {
        let mut readiness = StreamBindingReadiness::Ready(903);
        readiness.observe(StreamBindingEvent::Unconnected, pipewire::constants::ID_ANY);
        assert_eq!(
            readiness.into_result(),
            Err("PipeWire stream became unconnected during startup".to_string())
        );
    }

    #[test]
    fn explicit_link_plan_ignores_owned_monitor_outputs() {
        let sources = vec![ResolvedSourcePort {
            global_id: 10,
            node_id: 1,
            port_id: 0,
            name: "aux".to_string(),
        }];
        let ports = vec![
            AvailablePort {
                id: 20,
                object_type: "PipeWire:Interface:Port".to_string(),
                node_id: Some(9),
                direction: Some("in".to_string()),
                port_id: Some(0),
                name: Some("input_1".to_string()),
                format_dsp: Some("32 bit float mono audio".to_string()),
                audio_format: None,
            },
            AvailablePort {
                id: 21,
                object_type: "PipeWire:Interface:Port".to_string(),
                node_id: Some(9),
                direction: Some("out".to_string()),
                port_id: Some(0),
                name: Some("monitor_1".to_string()),
                format_dsp: Some("32 bit float mono audio".to_string()),
                audio_format: None,
            },
        ];
        assert_eq!(
            plan_explicit_links(&sources, 9, &ports).unwrap(),
            vec![PlannedLink {
                output_node_id: 1,
                output_port_id: 10,
                input_node_id: 9,
                input_port_id: 20
            }]
        );
    }

    #[test]
    fn destination_readiness_waits_for_exact_valid_owned_inputs() {
        let graph = AvailableGraph::default();
        assert_eq!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Pending
        );
    }

    fn destination_port(id: u32, local_id: u32, direction: &str) -> AvailablePort {
        AvailablePort {
            id,
            object_type: "PipeWire:Interface:Port".to_string(),
            node_id: Some(9),
            direction: Some(direction.to_string()),
            port_id: Some(local_id),
            name: Some(format!("port-{id}")),
            format_dsp: Some("32 bit float mono audio".to_string()),
            audio_format: None,
        }
    }

    #[test]
    fn destination_readiness_stays_pending_for_zero_or_partial_inputs() {
        let mut graph = AvailableGraph::default();
        assert_eq!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Pending
        );

        graph.ports = vec![
            destination_port(100, 0, "in"),
            destination_port(101, 1, "in"),
        ];
        assert_eq!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Pending
        );
    }

    #[test]
    fn destination_readiness_accepts_delayed_inputs_and_ignores_monitor_outputs() {
        let mut graph = AvailableGraph {
            ports: (0..4)
                .map(|id| destination_port(100 + id, id, "out"))
                .collect(),
            ..AvailableGraph::default()
        };
        assert_eq!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Pending
        );

        graph
            .ports
            .extend((0..4).rev().map(|id| destination_port(200 + id, id, "in")));
        assert_eq!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Ready
        );
    }

    #[test]
    fn destination_readiness_rejects_malformed_owned_input_even_before_count_is_met() {
        let mut graph = AvailableGraph {
            ports: vec![destination_port(100, 0, "in")],
            ..AvailableGraph::default()
        };
        graph.ports[0].format_dsp = Some("8 bit raw midi".to_string());

        assert!(matches!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Failed(_)
        ));
    }

    #[test]
    fn destination_readiness_rejects_duplicate_or_excess_owned_inputs() {
        let mut graph = AvailableGraph {
            ports: (0..4)
                .map(|id| destination_port(100 + id, id, "in"))
                .collect(),
            ..AvailableGraph::default()
        };
        graph.ports.push(destination_port(104, 3, "in"));
        assert!(matches!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Failed(_)
        ));

        graph.ports[4] = destination_port(104, 4, "in");
        assert!(matches!(
            destination_readiness(&graph, 9, 4),
            DestinationReadiness::Failed(_)
        ));
    }

    #[test]
    fn destination_startup_reports_timeout_stream_and_core_errors() {
        let graph = AvailableGraph::default();
        let ready = StreamBindingReadiness::Ready(9);
        assert!(
            evaluate_destination_startup(&graph, 9, 4, &ready, None, true)
                .unwrap_err()
                .to_string()
                .contains("destination ports timed out")
        );

        let failed = StreamBindingReadiness::Failed("stream lost".to_string());
        assert!(
            evaluate_destination_startup(&graph, 9, 4, &failed, None, false)
                .unwrap_err()
                .to_string()
                .contains("stream lost")
        );

        assert!(evaluate_destination_startup(
            &graph,
            9,
            4,
            &ready,
            Some("core lost".to_string()),
            false,
        )
        .unwrap_err()
        .to_string()
        .contains("core lost"));
    }

    #[test]
    fn stream_binding_readiness_only_wakes_on_state_transitions() {
        let mut readiness = StreamBindingReadiness::default();
        assert!(!readiness.observe(StreamBindingEvent::Paused, pipewire::constants::ID_ANY));
        assert!(readiness.observe(StreamBindingEvent::Paused, 9));
        assert!(!readiness.observe(StreamBindingEvent::Paused, 9));
        assert!(readiness.observe(StreamBindingEvent::Unconnected, pipewire::constants::ID_ANY));
        assert!(!readiness.observe(StreamBindingEvent::Unconnected, pipewire::constants::ID_ANY));
    }
}
