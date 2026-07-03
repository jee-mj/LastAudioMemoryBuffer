use crate::error::{io_error, LambError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LambConfig {
    #[serde(rename = "configVersion")]
    pub config_version: u32,
    pub user: String,
    pub target: Option<String>,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub channels: Option<u32>,
    #[serde(rename = "channelMap", default)]
    pub channel_map: Vec<String>,
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

impl LambConfig {
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
        if let Some(channels) = self.channels {
            if channels == 0 {
                return Err(LambError::Validation("channels must be > 0".to_string()));
            }
            if !self.channel_map.is_empty() && self.channel_map.len() != channels as usize {
                return Err(LambError::Validation(format!(
                    "channelMap length {} must match channels {}",
                    self.channel_map.len(),
                    channels
                )));
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
}

pub fn load_config_file(path: &Path) -> Result<LambConfig> {
    let text = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
    load_config_text(path, &text)
}

pub fn load_config_text(path: &Path, text: &str) -> Result<LambConfig> {
    let cfg: LambConfig = toml::from_str(text)
        .map_err(|err| LambError::Config(format!("failed to parse {}: {err}", path.display())))?;
    cfg.validate_static()?;
    Ok(cfg)
}
