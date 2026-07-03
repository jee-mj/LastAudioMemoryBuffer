use crate::error::{LambError, Result};
use crate::math::derive_chunk_frames;
use crate::profile::ResolvedProfile;
use crate::sample_ring::{RingConfig, SampleFormat, SampleRing};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JackCaptureConfig {
    pub client_name: String,
    pub source_ports: Vec<String>,
    pub channel_names: Vec<String>,
    pub channels: u32,
}

pub struct JackCapture {
    active: Option<jack::AsyncClient<(), JackProcessHandler>>,
    pub ring: Arc<SampleRing>,
    pub sample_rate: u32,
    pub channel_count: u32,
}

struct JackProcessHandler {
    ports: Vec<jack::Port<jack::AudioIn>>,
    ring: Arc<SampleRing>,
    scratch: Vec<f32>,
}

impl JackCaptureConfig {
    pub fn from_profile(profile: &ResolvedProfile) -> Self {
        Self {
            client_name: profile.client_name.clone(),
            source_ports: profile
                .ports
                .iter()
                .map(|port| port.source.clone())
                .collect(),
            channel_names: profile.ports.iter().map(|port| port.name.clone()).collect(),
            channels: profile.ports.len() as u32,
        }
    }
}

impl JackCapture {
    pub fn start(cfg: JackCaptureConfig, buffer_seconds: u32) -> Result<Self> {
        if cfg.channels == 0 {
            return Err(LambError::Capture(
                "JACK capture requires at least one input port".to_string(),
            ));
        }
        let (client, _status) =
            jack::Client::new(&cfg.client_name, jack::ClientOptions::NO_START_SERVER)
                .map_err(|err| jack_error("open client", err))?;

        for source in &cfg.source_ports {
            if client.port_by_name(source).is_none() {
                return Err(LambError::Capture(format!(
                    "JACK source port not found: {source}"
                )));
            }
        }

        let sample_rate = client.sample_rate();
        let ring = make_jack_ring(buffer_seconds, sample_rate, cfg.channels)?;
        let mut ports = Vec::with_capacity(cfg.channel_names.len());
        for name in &cfg.channel_names {
            ports.push(
                client
                    .register_port(name, jack::AudioIn::default())
                    .map_err(|err| jack_error("register input port", err))?,
            );
        }
        let destination_ports = ports
            .iter()
            .map(|port| {
                port.name()
                    .map_err(|err| jack_error("read input port name", err))
            })
            .collect::<Result<Vec<_>>>()?;
        let handler = JackProcessHandler {
            ports,
            ring: Arc::clone(&ring),
            scratch: Vec::new(),
        };
        let active = client
            .activate_async((), handler)
            .map_err(|err| jack_error("activate client", err))?;
        for (source, destination) in cfg.source_ports.iter().zip(destination_ports.iter()) {
            active
                .as_client()
                .connect_ports_by_name(source, destination)
                .map_err(|err| {
                    LambError::Capture(format!(
                        "JACK connect {source} -> {destination} failed: {err}"
                    ))
                })?;
        }
        Ok(Self {
            active: Some(active),
            ring,
            sample_rate,
            channel_count: cfg.channels,
        })
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }
    fn stop_inner(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = active.deactivate();
        }
    }
}

impl Drop for JackCapture {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl jack::ProcessHandler for JackProcessHandler {
    fn process(&mut self, _: &jack::Client, process_scope: &jack::ProcessScope) -> jack::Control {
        let Some(first) = self.ports.first() else {
            return jack::Control::Quit;
        };
        let frame_count = first.as_slice(process_scope).len();
        let channel_count = self.ports.len();
        self.scratch.clear();
        let sample_count = frame_count.saturating_mul(channel_count);
        if self.scratch.capacity() < sample_count {
            self.scratch.reserve(sample_count - self.scratch.capacity());
        }
        for frame in 0..frame_count {
            for port in &self.ports {
                self.scratch.push(port.as_slice(process_scope)[frame]);
            }
        }
        match self
            .ring
            .write_interleaved(&self.scratch, channel_count as u32)
        {
            Ok(()) => jack::Control::Continue,
            Err(_) => jack::Control::Quit,
        }
    }
    fn buffer_size(&mut self, _: &jack::Client, size: jack::Frames) -> jack::Control {
        let sample_count = (size as usize).saturating_mul(self.ports.len());
        if self.scratch.capacity() < sample_count {
            self.scratch.reserve(sample_count - self.scratch.capacity());
        }
        jack::Control::Continue
    }
}

pub fn interleave_input_buffers(inputs: &[&[f32]]) -> Result<Vec<f32>> {
    let Some(first) = inputs.first() else {
        return Err(LambError::Capture(
            "JACK input buffers are required".to_string(),
        ));
    };
    let frame_count = first.len();
    if inputs.iter().any(|input| input.len() != frame_count) {
        return Err(LambError::Capture(
            "JACK input buffers must have the same frame count".to_string(),
        ));
    }
    let mut interleaved = Vec::with_capacity(frame_count * inputs.len());
    for frame in 0..frame_count {
        for input in inputs {
            interleaved.push(input[frame]);
        }
    }
    Ok(interleaved)
}

fn make_jack_ring(seconds: u32, sample_rate: u32, channels: u32) -> Result<Arc<SampleRing>> {
    let chunk_frames = derive_chunk_frames(sample_rate, None)?;
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
        max_active_snapshots: 1,
    })?))
}

fn jack_error(action: &str, err: jack::Error) -> LambError {
    LambError::Capture(format!("JACK {action} failed: {err}"))
}
