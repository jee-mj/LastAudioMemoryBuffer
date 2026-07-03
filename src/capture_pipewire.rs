use crate::config::LambConfig;
use crate::error::{LambError, Result};
use crate::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub id: Option<u32>,
    pub name: String,
    pub description: Option<String>,
    pub channels: u32,
    pub sample_rate: u32,
    pub format: String,
}

impl ResolvedTarget {
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
    pub channels: Option<u32>,
    pub sample_rate: u32,
    pub dont_remix: bool,
    pub channel_map: Vec<String>,
    pub latency: Option<String>,
}

impl PipeWireCaptureConfig {
    pub fn from_lamb_config(cfg: &LambConfig) -> Self {
        Self {
            target: cfg.target.clone(),
            channels: cfg.channels,
            sample_rate: cfg.sample_rate,
            dont_remix: cfg.dont_remix,
            channel_map: cfg.channel_map.clone(),
            latency: cfg.latency.clone(),
        }
    }
}

pub fn make_pipewire_ring(
    seconds: u32,
    sample_rate: u32,
    channels: u32,
    max_active_snapshots: u32,
) -> Result<Arc<SampleRing>> {
    let chunk_frames = crate::math::derive_chunk_frames(sample_rate, None)?;
    let total_frames = u64::from(seconds)
        .checked_mul(u64::from(sample_rate))
        .ok_or_else(|| LambError::Validation("ring frame count overflow".to_string()))?;
    let chunk_count = total_frames.div_ceil(u64::from(chunk_frames)).max(1);
    Ok(Arc::new(SampleRing::new(RingConfig {
        channels,
        sample_rate,
        format: SampleFormat::F32Le,
        chunk_frames,
        chunk_count: u32::try_from(chunk_count)
            .map_err(|_| LambError::Validation("chunk count exceeds u32".to_string()))?,
        max_active_snapshots,
    })?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableNode {
    pub id: u32,
    pub object_type: String,
    pub media_class: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub format: Option<String>,
}

pub struct PipeWireCapture {
    resolved: ResolvedTarget,
    pub ring: Arc<SampleRing>,
    pub sample_rate: u32,
    pub channel_count: u32,
    stop_sender: pipewire::channel::Sender<PipeWireCommand>,
    join: Option<JoinHandle<()>>,
}

enum PipeWireCommand {
    Stop,
}

impl PipeWireCapture {
    pub fn start(cfg: PipeWireCaptureConfig, ring: Arc<SampleRing>) -> Result<Self> {
        let resolved = resolve_target(&cfg)?;
        Self::start_with_resolved(cfg, resolved, ring)
    }

    pub(crate) fn start_with_resolved(
        cfg: PipeWireCaptureConfig,
        resolved: ResolvedTarget,
        ring: Arc<SampleRing>,
    ) -> Result<Self> {
        let resolved_for_thread = resolved.clone();
        let ring_for_thread = Arc::clone(&ring);
        let sample_rate = resolved.sample_rate;
        let channel_count = resolved.channels;
        let (stop_sender, stop_receiver) = pipewire::channel::channel();
        let (ready_sender, ready_receiver) = mpsc::channel();

        let join = thread::spawn(move || {
            if let Err(err) = run_pipewire_stream_loop(
                cfg,
                resolved_for_thread,
                ring_for_thread,
                stop_receiver,
                ready_sender.clone(),
            ) {
                let _ = ready_sender.send(Err(err));
            }
        });

        match ready_receiver.recv().map_err(|_| {
            LambError::Capture("PipeWire capture thread exited before startup".to_string())
        })? {
            Ok(()) => Ok(Self {
                resolved,
                ring,
                sample_rate,
                channel_count,
                stop_sender,
                join: Some(join),
            }),
            Err(err) => {
                let _ = join.join();
                Err(err)
            }
        }
    }

    pub fn resolved_target(&self) -> &ResolvedTarget {
        &self.resolved
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
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

pub fn resolve_target(cfg: &PipeWireCaptureConfig) -> Result<ResolvedTarget> {
    let nodes = discover_available_nodes()?;
    resolve_target_from_nodes(cfg, &nodes)
}

pub fn resolve_target_from_nodes(
    cfg: &PipeWireCaptureConfig,
    nodes: &[AvailableNode],
) -> Result<ResolvedTarget> {
    let selected = if let Some(target) = cfg.target.as_deref() {
        let node = nodes
            .iter()
            .find(|node| node_matches_target(node, target))
            .ok_or_else(|| LambError::Capture(format!("PipeWire target not found: {target}")))?;
        if !is_input_source_node(node) {
            return Err(LambError::Capture(
                "target is not an input/source node".to_string(),
            ));
        }
        node
    } else {
        nodes
            .iter()
            .find(|node| is_input_source_node(node))
            .ok_or_else(|| LambError::Capture("no PipeWire input/source node found".to_string()))?
    };

    resolved_from_node(cfg, selected)
}

pub fn process_interleaved_f32_chunk(
    bytes: &[u8],
    offset: u32,
    size: u32,
    stride: i32,
    channels: u32,
    ring: &SampleRing,
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
    ring.write_interleaved(samples, channels)
}

fn resolved_from_node(cfg: &PipeWireCaptureConfig, node: &AvailableNode) -> Result<ResolvedTarget> {
    let channels = node.channels.or(cfg.channels).ok_or_else(|| {
        LambError::Capture("resolved PipeWire source did not report channel count".to_string())
    })?;
    if channels == 0 {
        return Err(LambError::Capture(
            "resolved PipeWire source has zero channels".to_string(),
        ));
    }
    if !cfg.channel_map.is_empty() && cfg.channel_map.len() != channels as usize {
        return Err(LambError::Capture(format!(
            "channelMap length {} must match resolved channels {channels}",
            cfg.channel_map.len()
        )));
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
    })
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

fn discover_available_nodes() -> Result<Vec<AvailableNode>> {
    use pipewire as pw;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pipewire_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pipewire_error)?;
    let core = context.connect_rc(None).map_err(pipewire_error)?;
    let registry = core.get_registry().map_err(pipewire_error)?;

    let nodes = Rc::new(RefCell::new(Vec::new()));
    let nodes_for_listener = Rc::clone(&nodes);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            nodes_for_listener
                .borrow_mut()
                .push(available_node_from_global(global));
        })
        .register();

    let done = Rc::new(Cell::new(false));
    let done_for_listener = Rc::clone(&done);
    let mainloop_for_listener = mainloop.clone();
    let pending = core.sync(0).map_err(pipewire_error)?;
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_for_listener.set(true);
                mainloop_for_listener.quit();
            }
        })
        .register();

    while !done.get() {
        mainloop.run();
    }

    let discovered = nodes.borrow().clone();
    Ok(discovered)
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
        description: string_prop(props, *pipewire::keys::NODE_DESCRIPTION),
        channels: u32_prop(props, *pipewire::keys::AUDIO_CHANNELS),
        sample_rate: u32_prop(props, "audio.rate"),
        format: string_prop(props, *pipewire::keys::AUDIO_FORMAT)
            .or_else(|| string_prop(props, *pipewire::keys::FORMAT_DSP)),
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

fn run_pipewire_stream_loop(
    cfg: PipeWireCaptureConfig,
    resolved: ResolvedTarget,
    ring: Arc<SampleRing>,
    stop_receiver: pipewire::channel::Receiver<PipeWireCommand>,
    ready_sender: mpsc::Sender<Result<()>>,
) -> Result<()> {
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

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    if let Some(id) = resolved.id {
        props.insert("target.object", id.to_string());
    } else if let Some(target) = cfg.target.as_ref() {
        props.insert("target.object", target.clone());
    }
    if cfg.dont_remix {
        props.insert(*pw::keys::STREAM_DONT_REMIX, "true");
    }
    if let Some(latency) = cfg.latency.as_ref() {
        props.insert(*pw::keys::NODE_LATENCY, latency.clone());
    }

    let stream =
        pw::stream::StreamBox::new(&core, "lamb-capture", props).map_err(pipewire_error)?;
    let user_data = PipeWireStreamData {
        format: spa::param::audio::AudioInfoRaw::new(),
        channels: resolved.channels,
        ring,
    };
    let _stream_listener = stream
        .add_local_listener_with_user_data(user_data)
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
            // Conservative per-callback frame estimate for drop accounting.
            // PipeWire RT buffers are typically 256–1024 frames; we use 256
            // as a floor so we undercount rather than overcount.
            const DROP_FRAME_ESTIMATE: u64 = 256;

            if user_data.format.format() != spa::param::audio::AudioFormat::F32LE {
                user_data.ring.record_dropped_frames(DROP_FRAME_ESTIMATE);
                return;
            }
            if user_data.format.channels() != user_data.channels {
                user_data.ring.record_dropped_frames(DROP_FRAME_ESTIMATE);
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                user_data.ring.record_dropped_frames(DROP_FRAME_ESTIMATE);
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                user_data.ring.record_dropped_frames(DROP_FRAME_ESTIMATE);
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
                    &user_data.ring,
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
            resolved.id,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(pipewire_error)?;

    let _ = ready_sender.send(Ok(()));
    mainloop.run();
    Ok(())
}

struct PipeWireStreamData {
    format: pipewire::spa::param::audio::AudioInfoRaw,
    channels: u32,
    ring: Arc<SampleRing>,
}
