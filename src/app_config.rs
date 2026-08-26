use crate::activity::{
    ActivityDetectorKind, ChannelExportMode, SilencePolicyPreset, ThresholdSource,
};
use crate::error::{io_error, LambError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonConfig {
    #[serde(rename = "startMode", default = "default_start_mode")]
    pub start_mode: String,
    #[serde(rename = "activeProfile", default)]
    pub active_profile: Option<String>,
    #[serde(rename = "controlSocketPath", default = "default_control_socket_path")]
    pub control_socket_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(
        rename = "clientName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_name: Option<String>,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub buffer: BufferConfig,
    #[serde(default)]
    pub export: ProfileExportConfig,
    #[serde(default)]
    pub pipewire: PipewireProfileConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub channels: BTreeMap<String, ProfileChannelConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PipewireProfileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(
        rename = "sampleRate",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sample_rate: Option<u32>,
    #[serde(rename = "dontRemix", default)]
    pub dont_remix: bool,
    #[serde(
        rename = "capturePorts",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub capture_ports: Vec<CapturePort>,
    #[serde(
        rename = "channelMap",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub channel_map: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<CapturePort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturePort {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        rename = "exportMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub export_mode: Option<ChannelExportMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BufferConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileExportConfig {
    #[serde(rename = "outputDir", default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<ExportLayoutKind>,
    #[serde(
        rename = "directoryPattern",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub directory_pattern: Option<String>,
    #[serde(
        rename = "filenamePattern",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub filename_pattern: Option<String>,
    #[serde(
        rename = "defaultChannelMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub default_channel_mode: Option<ChannelExportMode>,
    #[serde(
        rename = "activityDetector",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub activity_detector: Option<ActivityDetectorKind>,
    #[serde(
        rename = "silencePolicy",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub silence_policy: Option<SilencePolicyPreset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportLayoutKind {
    FlatDetailed,
    TimestampDirectory,
    Custom,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfileChannelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityThresholdConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityThresholdConfig {
    #[serde(rename = "thresholdDbFS")]
    pub threshold_dbfs: f64,
    #[serde(rename = "thresholdSource")]
    pub threshold_source: ThresholdSource,
    #[serde(rename = "updatedAtUnixSeconds")]
    pub updated_at_unix_seconds: u64,
    #[serde(rename = "inputId")]
    pub input_id: String,
    #[serde(
        rename = "calibrationId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub calibration_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLoadState {
    Missing,
    Loaded,
    Invalid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedAppConfig {
    pub config: AppConfig,
    pub state: ConfigLoadState,
    pub error: Option<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            start_mode: default_start_mode(),
            active_profile: None,
            control_socket_path: default_control_socket_path(),
        }
    }
}

pub fn default_start_mode() -> String {
    "manual".to_string()
}

pub fn default_control_socket_path() -> String {
    "%t/lamb/control.sock".to_string()
}

pub fn default_config_text() -> &'static str {
    "[daemon]\nstartMode = \"manual\"\n\n[profiles]\n"
}

pub fn default_config_path() -> Result<PathBuf> {
    default_config_path_from_env(
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
    )
}

pub fn default_config_path_from_env(
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(base) = xdg_config_home {
        return Ok(base.join("lamb/lamb.toml"));
    }
    if let Some(home) = home {
        return Ok(home.join(".config/lamb/lamb.toml"));
    }
    Err(LambError::Config(
        "cannot determine LAMB config path: set XDG_CONFIG_HOME or HOME".to_string(),
    ))
}

pub fn parse_config_text(path: &Path, text: &str) -> Result<AppConfig> {
    let cfg: AppConfig = toml::from_str(text)
        .map_err(|err| LambError::Config(format!("failed to parse {}: {err}", path.display())))?;
    validate_app_config(&cfg)?;
    Ok(cfg)
}

pub fn load_optional_config(path: &Path) -> Result<LoadedAppConfig> {
    match fs::read_to_string(path) {
        Ok(text) => match parse_config_text(path, &text) {
            Ok(config) => Ok(LoadedAppConfig {
                config,
                state: ConfigLoadState::Loaded,
                error: None,
            }),
            Err(err) => Ok(LoadedAppConfig {
                config: AppConfig::default(),
                state: ConfigLoadState::Invalid,
                error: Some(err.to_string()),
            }),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LoadedAppConfig {
            config: AppConfig::default(),
            state: ConfigLoadState::Missing,
            error: None,
        }),
        Err(source) => Err(io_error(path, source)),
    }
}

pub fn write_default_config(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(LambError::Config(format!(
            "{} already exists; pass --force to overwrite",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    fs::write(path, default_config_text()).map_err(|source| io_error(path, source))
}

pub(crate) fn validate_app_config(cfg: &AppConfig) -> Result<()> {
    match cfg.daemon.start_mode.as_str() {
        "manual" | "auto" => Ok(()),
        other => Err(LambError::Validation(format!(
            "daemon.startMode must be manual or auto, got {other}"
        ))),
    }
}

pub(crate) fn is_valid_input_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn is_valid_calibration_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(crate) fn validate_persisted_config(cfg: &AppConfig) -> Result<()> {
    validate_app_config(cfg)?;
    for (profile_name, profile) in &cfg.profiles {
        for (channel_name, channel) in &profile.channels {
            let Some(activity) = &channel.activity else {
                continue;
            };
            let field = format!("profile {profile_name}: channels.{channel_name}.activity");
            if !activity.threshold_dbfs.is_finite()
                || !(-120.0..=0.0).contains(&activity.threshold_dbfs)
            {
                return Err(LambError::Validation(format!(
                    "{field}.thresholdDbFS must be finite and within [-120.0, 0.0]"
                )));
            }
            if !is_valid_input_id(&activity.input_id) {
                return Err(LambError::Validation(format!(
                    "{field}.inputId must be exactly 64 lowercase hexadecimal characters"
                )));
            }
            if activity
                .calibration_id
                .as_deref()
                .is_some_and(|id| !is_valid_calibration_id(id))
            {
                return Err(LambError::Validation(format!(
                    "{field}.calibrationId must be 1 to 128 ASCII alphanumeric, '-' or '_' characters when present"
                )));
            }
            if activity.threshold_source == ThresholdSource::Calibrated
                && activity.calibration_id.is_none()
            {
                return Err(LambError::Validation(format!(
                    "{field}.calibrationId is required for calibrated thresholds"
                )));
            }
        }
    }
    Ok(())
}
