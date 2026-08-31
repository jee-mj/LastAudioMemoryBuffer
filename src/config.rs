use crate::activity::{ActivityDetectorKind, ChannelExportMode};
use crate::error::{io_error, LambError, Result};
use crate::export_policy::{
    ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
    ResolvedLayout,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturePortConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredCapturePort {
    pub source: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LambConfig {
    #[serde(rename = "configVersion")]
    pub config_version: u32,
    pub user: String,
    pub target: Option<String>,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub channels: Option<u32>,
    #[serde(
        rename = "channelMap",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub channel_map: Option<Vec<String>>,
    #[serde(
        rename = "capturePorts",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub capture_ports: Vec<CapturePortConfig>,
    pub seconds: u32,
    #[serde(rename = "sampleRate")]
    pub sample_rate: u32,
    #[serde(rename = "sampleFormat")]
    pub sample_format: String,
    pub latency: Option<String>,
    #[serde(rename = "dontRemix")]
    pub dont_remix: bool,
    #[serde(rename = "outputDir")]
    pub output_dir: PathBuf,
    pub memory: MemoryConfig,
    #[serde(rename = "maxActiveSnapshots")]
    pub max_active_snapshots: u32,
    #[serde(rename = "allowQueuedRecall")]
    pub allow_queued_recall: bool,
    #[serde(rename = "chunkFrames", default)]
    pub chunk_frames: Option<u32>,
    #[serde(rename = "controlSocketPath")]
    pub control_socket_path: PathBuf,
    #[serde(rename = "controlPermissions")]
    pub control_permissions: String,
    pub export: ExportConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryConfig {
    pub max: Option<u64>,
    pub headroom: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExportConfig {
    pub mode: String,
    pub format: String,
    #[serde(rename = "splitWhenOverBytes")]
    pub split_when_over_bytes: u64,
}

fn default_backend() -> String {
    "pipewire".to_string()
}

pub(crate) fn normalize_capture_ports<'a>(
    entries: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>,
    field: &str,
    required_error: &str,
) -> Result<Vec<ConfiguredCapturePort>> {
    let mut source_indexes = BTreeMap::<String, usize>::new();
    let mut name_indexes = BTreeMap::<String, usize>::new();
    let mut resolved = Vec::new();

    for (index, (source, name)) in entries.into_iter().enumerate() {
        let source = source
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LambError::Validation(format!("{field}[{index}].source is required")))?
            .to_string();
        let name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LambError::Validation(format!("{field}[{index}].name is required")))?
            .to_string();

        if let Some(first) = source_indexes.insert(source.clone(), index) {
            return Err(LambError::Validation(format!(
                "{field}[{index}].source duplicates {field}[{first}].source"
            )));
        }
        if let Some(first) = name_indexes.insert(name.clone(), index) {
            return Err(LambError::Validation(format!(
                "{field}[{index}].name duplicates {field}[{first}].name"
            )));
        }
        resolved.push(ConfiguredCapturePort { source, name });
    }

    if resolved.is_empty() {
        return Err(LambError::Validation(required_error.to_string()));
    }
    Ok(resolved)
}

impl LambConfig {
    pub fn resolved_capture_ports(&self) -> Result<Vec<ConfiguredCapturePort>> {
        normalize_capture_ports(
            self.capture_ports
                .iter()
                .map(|port| (port.source.as_deref(), port.name.as_deref())),
            "capturePorts",
            "capturePorts is required for pipewire backend",
        )
    }

    pub fn validate_static(&self) -> Result<()> {
        if self.config_version != 1 {
            return Err(LambError::Validation(format!(
                "configVersion {} is not supported; expected 1",
                self.config_version
            )));
        }
        if self.user.trim().is_empty() {
            return Err(LambError::Validation("user must be non-empty".to_string()));
        }
        if self.backend != "pipewire" && self.backend != "fake" {
            return Err(LambError::Validation(
                "backend must be pipewire or fake".to_string(),
            ));
        }
        if self.seconds == 0 {
            return Err(LambError::Validation("seconds must be > 0".to_string()));
        }
        if self.sample_rate == 0 {
            return Err(LambError::Validation("sampleRate must be > 0".to_string()));
        }
        if self.backend == "pipewire" {
            if self.channels.is_some() {
                return Err(LambError::Validation(
                    "channels conflicts with capturePorts for pipewire backend".to_string(),
                ));
            }
            if self.channel_map.is_some() {
                return Err(LambError::Validation(
                    "channelMap conflicts with capturePorts for pipewire backend".to_string(),
                ));
            }
            self.resolved_capture_ports()?;
        } else if let Some(channels) = self.channels {
            if channels == 0 {
                return Err(LambError::Validation("channels must be > 0".to_string()));
            }
            if let Some(channel_map) = self.channel_map.as_ref() {
                if !channel_map.is_empty() && channel_map.len() != channels as usize {
                    return Err(LambError::Validation(format!(
                        "channelMap length {} must match channels {}",
                        channel_map.len(),
                        channels
                    )));
                }
            }
        }
        if self.sample_format != "F32LE" {
            return Err(LambError::Validation(format!(
                "sampleFormat {} is unsupported in v0.2; expected F32LE",
                self.sample_format
            )));
        }
        if !self.output_dir.is_absolute() {
            return Err(LambError::Validation(
                "outputDir must be absolute in daemon config".to_string(),
            ));
        }
        if self.memory.headroom < 1.0 || !self.memory.headroom.is_finite() {
            return Err(LambError::Validation(
                "memory.headroom must be finite and >= 1.0".to_string(),
            ));
        }
        if self.max_active_snapshots == 0 {
            return Err(LambError::Validation(
                "maxActiveSnapshots must be > 0".to_string(),
            ));
        }
        if let Some(chunk_frames) = self.chunk_frames {
            if chunk_frames == 0 {
                return Err(LambError::Validation("chunkFrames must be > 0".to_string()));
            }
        }
        if self.control_permissions != "0600" {
            return Err(LambError::Validation(
                "controlPermissions must be 0600 in v0.2".to_string(),
            ));
        }
        if self.export.mode != "per-channel" {
            return Err(LambError::Validation(
                "export.mode must be per-channel".to_string(),
            ));
        }
        if self.export.format != "wav" {
            return Err(LambError::Validation(
                "export.format must be wav".to_string(),
            ));
        }
        if self.export.split_when_over_bytes == 0
            || self.export.split_when_over_bytes >= 4_000_000_000
        {
            return Err(LambError::Validation(
                "export.splitWhenOverBytes must be between 1 and 3999999999".to_string(),
            ));
        }
        Ok(())
    }

    pub fn resolved_export_policy(&self, command: ExportCommand) -> Result<ResolvedExportPolicy> {
        self.resolved_export_policy_with_layout(match command {
            ExportCommand::Recall => ResolvedLayout::FlatDetailed,
            ExportCommand::Dump => ResolvedLayout::TimestampDirectory,
        })
    }

    /// Resolves the one legacy session policy. CommandDefault retains the
    /// historical recall/dump layouts while sharing activity and output root.
    pub fn resolved_session_export_policy(&self) -> Result<ResolvedExportPolicy> {
        self.resolved_export_policy_with_layout(ResolvedLayout::CommandDefault)
    }

    fn resolved_export_policy_with_layout(
        &self,
        layout: ResolvedLayout,
    ) -> Result<ResolvedExportPolicy> {
        self.validate_static()?;
        let channel_names = if self.backend == "pipewire" {
            self.resolved_capture_ports()?
                .into_iter()
                .map(|port| port.name)
                .collect()
        } else if let Some(channel_map) = self.channel_map.as_ref().filter(|map| !map.is_empty()) {
            channel_map.clone()
        } else {
            (0..self.channels.unwrap_or(0))
                .map(|index| format!("ch{:02}", index + 1))
                .collect()
        };
        let channels = channel_names
            .into_iter()
            .map(|name| ChannelActivityPolicy {
                name,
                mode: ChannelExportMode::Always,
                threshold: None,
            })
            .collect();

        ResolvedExportPolicy::new(
            self.output_dir.clone(),
            layout,
            ResolvedActivityPolicy {
                detector: ActivityDetectorKind::ExactZero,
                channels,
                whole_export_exact_zero_gate: true,
                trim_leading_silence: false,
            },
        )
    }
}

pub fn load_config_file(path: &Path) -> Result<LambConfig> {
    let text = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
    load_config_text(path, &text)
}

pub fn parse_config_text(path: &Path, text: &str) -> Result<LambConfig> {
    toml::from_str(text)
        .map_err(|err| LambError::Config(format!("failed to parse {}: {err}", path.display())))
}

pub fn load_config_text(path: &Path, text: &str) -> Result<LambConfig> {
    let cfg = parse_config_text(path, text)?;
    cfg.validate_static()?;
    Ok(cfg)
}
