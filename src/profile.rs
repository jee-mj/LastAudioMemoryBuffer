use crate::activity::{ActivityDetectorKind, ChannelExportMode, SilencePolicyPreset};
use crate::app_config::{
    self, AppConfig, CapturePort, ConfigLoadState, ExportLayoutKind, ProfileConfig,
};
use crate::capture_pipewire::PipeWireCaptureConfig;
use crate::config::{normalize_capture_ports, ConfiguredCapturePort};
use crate::error::{io_error, LambError, Result};
use crate::export_policy::{
    ChannelActivityPolicy, ResolvedActivityPolicy, ResolvedExportPolicy, ResolvedLayout,
};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProfile {
    pub name: String,
    pub backend: String,
    pub client_name: String,
    pub ports: Vec<ResolvedCapturePort>,
    pub buffer_seconds: u32,
    pub export_policy: ResolvedExportPolicy,
    pub pipewire_config: Option<PipeWireCaptureConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCapturePort {
    pub source: String,
    pub name: String,
}

pub fn validate_profile(name: &str, profile: &ProfileConfig) -> Result<ResolvedProfile> {
    let backend = required_string("backend", profile.backend.as_deref())?;
    match backend.as_str() {
        "jack" => validate_jack_profile(name, profile),
        "pipewire" => validate_pipewire_profile(name, profile),
        other => Err(LambError::Validation(format!(
            "profile {name}: backend must be jack or pipewire, got {other}"
        ))),
    }
}

fn validate_jack_profile(name: &str, profile: &ProfileConfig) -> Result<ResolvedProfile> {
    let client_name = required_string("clientName", profile.client_name.as_deref())?;
    let ports = resolve_capture_ports(name, profile)?;
    let buffer_seconds = validate_buffer_seconds(name, profile)?;
    let export_policy = resolve_export_policy(name, profile, &ports)?;

    Ok(ResolvedProfile {
        name: name.to_string(),
        backend: "jack".to_string(),
        client_name,
        ports,
        buffer_seconds,
        export_policy,
        pipewire_config: None,
    })
}

fn validate_pipewire_profile(name: &str, profile: &ProfileConfig) -> Result<ResolvedProfile> {
    let pw = &profile.pipewire;
    if pw.channel_map.is_some() {
        return Err(LambError::Validation(format!(
            "profile {name}: pipewire.channelMap conflicts with pipewire.capturePorts"
        )));
    }
    if !profile.capture.ports.is_empty() || !profile.capture.sources.is_empty() {
        return Err(LambError::Validation(format!(
            "profile {name}: capture.ports and capture.sources are only valid for jack profiles"
        )));
    }
    let ports = resolve_pipewire_capture_ports(name, profile)?;

    let buffer_seconds = validate_buffer_seconds(name, profile)?;
    let export_policy = resolve_export_policy(name, profile, &ports)?;

    Ok(ResolvedProfile {
        name: name.to_string(),
        backend: "pipewire".to_string(),
        client_name: "lamb".to_string(),
        ports: ports.clone(),
        buffer_seconds,
        export_policy,
        pipewire_config: Some(PipeWireCaptureConfig {
            target: pw.target.clone(),
            capture_ports: ports
                .iter()
                .map(|port| ConfiguredCapturePort {
                    source: port.source.clone(),
                    name: port.name.clone(),
                })
                .collect(),
            sample_rate: pw.sample_rate.unwrap_or(44100),
            dont_remix: pw.dont_remix,
            latency: pw.latency.clone(),
        }),
    })
}

fn resolve_pipewire_capture_ports(
    profile_name: &str,
    profile: &ProfileConfig,
) -> Result<Vec<ResolvedCapturePort>> {
    normalize_capture_ports(
        profile
            .pipewire
            .capture_ports
            .iter()
            .map(|port| (port.source.as_deref(), port.name.as_deref())),
        "pipewire.capturePorts",
        &format!("profile {profile_name}: pipewire.capturePorts is required"),
    )
    .map(|ports| {
        ports
            .into_iter()
            .map(|port| ResolvedCapturePort {
                source: port.source,
                name: port.name,
            })
            .collect()
    })
}

fn validate_buffer_seconds(name: &str, profile: &ProfileConfig) -> Result<u32> {
    let seconds = profile.buffer.seconds.ok_or_else(|| {
        LambError::Validation(format!("profile {name}: buffer.seconds is required"))
    })?;
    if seconds == 0 {
        return Err(LambError::Validation(format!(
            "profile {name}: buffer.seconds must be > 0"
        )));
    }
    Ok(seconds)
}

fn validate_export_output_dir(name: &str, profile: &ProfileConfig) -> Result<PathBuf> {
    let dir = profile.export.output_dir.clone().ok_or_else(|| {
        LambError::Validation(format!("profile {name}: export.outputDir is required"))
    })?;
    if !dir.is_absolute() {
        return Err(LambError::Validation(format!(
            "profile {name}: export.outputDir must be absolute"
        )));
    }
    Ok(dir)
}

fn validate_export(name: &str, profile: &ProfileConfig) -> Result<(String, String)> {
    let mode = required_string("export.mode", profile.export.mode.as_deref())?;
    if mode != "per-channel" {
        return Err(LambError::Validation(format!(
            "profile {name}: export.mode must be per-channel, got {mode}"
        )));
    }
    let format = required_string("export.format", profile.export.format.as_deref())?;
    if format != "wav" {
        return Err(LambError::Validation(format!(
            "profile {name}: export.format must be wav, got {format}"
        )));
    }
    Ok((mode, format))
}

fn resolve_export_policy(
    name: &str,
    profile: &ProfileConfig,
    ports: &[ResolvedCapturePort],
) -> Result<ResolvedExportPolicy> {
    let output_dir = validate_export_output_dir(name, profile)?;
    validate_export(name, profile)?;

    if profile.export.silence_policy.is_some() && profile.export.default_channel_mode.is_some() {
        return Err(LambError::Validation(format!(
            "profile {name}: export.silencePolicy conflicts with export.defaultChannelMode"
        )));
    }
    if profile.export.silence_policy.is_some() && profile.export.activity_detector.is_some() {
        return Err(LambError::Validation(format!(
            "profile {name}: export.silencePolicy conflicts with export.activityDetector"
        )));
    }

    let (default_mode, detector, whole_export_exact_zero_gate, trim_leading_silence) =
        match profile.export.silence_policy {
            Some(SilencePolicyPreset::AllChannelsExactZero) => (
                ChannelExportMode::Always,
                ActivityDetectorKind::ExactZero,
                true,
                false,
            ),
            Some(SilencePolicyPreset::PerChannelExactZero) => (
                ChannelExportMode::Auto,
                ActivityDetectorKind::ExactZero,
                false,
                true,
            ),
            None => (
                profile
                    .export
                    .default_channel_mode
                    .unwrap_or(ChannelExportMode::Auto),
                profile
                    .export
                    .activity_detector
                    .unwrap_or(ActivityDetectorKind::WindowedRmsPeak),
                false,
                true,
            ),
        };

    let reserved_name = match detector {
        ActivityDetectorKind::FixedLevel => Some("fixed-level"),
        ActivityDetectorKind::CalibratedNoiseFloor => Some("calibrated-noise-floor"),
        ActivityDetectorKind::ExactZero | ActivityDetectorKind::WindowedRmsPeak => None,
    };
    if let Some(detector_name) = reserved_name {
        return Err(LambError::Validation(format!(
            "profile {name}: export.activityDetector {detector_name} is reserved and not supported"
        )));
    }

    for (channel_name, channel) in &profile.channels {
        if ports
            .iter()
            .filter(|port| port.name == *channel_name)
            .count()
            != 1
        {
            return Err(LambError::Validation(format!(
                "profile {name}: channels.{channel_name} does not match exactly one configured port name"
            )));
        }
        if let Some(activity) = &channel.activity {
            if !activity.threshold_dbfs.is_finite()
                || !(-120.0..=0.0).contains(&activity.threshold_dbfs)
            {
                return Err(LambError::Validation(format!(
                    "profile {name}: channels.{channel_name}.activity.thresholdDbFS must be finite and within [-120.0, 0.0]"
                )));
            }
            if activity.input_id.trim().is_empty() {
                return Err(LambError::Validation(format!(
                    "profile {name}: channels.{channel_name}.activity.inputId must be non-empty"
                )));
            }
            if activity
                .calibration_id
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(LambError::Validation(format!(
                    "profile {name}: channels.{channel_name}.activity.calibrationId must be non-empty when present"
                )));
            }
        }
    }

    let channels = ports
        .iter()
        .map(|port| ChannelActivityPolicy {
            name: port.name.clone(),
            mode: capture_port_export_mode(profile, &port.name).unwrap_or(default_mode),
            threshold: profile
                .channels
                .get(&port.name)
                .and_then(|channel| channel.activity.clone()),
        })
        .collect();

    let layout = match profile.export.layout {
        None => ResolvedLayout::CommandDefault,
        Some(ExportLayoutKind::FlatDetailed) => ResolvedLayout::FlatDetailed,
        Some(ExportLayoutKind::TimestampDirectory) => ResolvedLayout::TimestampDirectory,
        Some(ExportLayoutKind::Custom) => ResolvedLayout::Custom {
            directory_pattern: profile.export.directory_pattern.clone().ok_or_else(|| {
                LambError::Validation(format!(
                    "profile {name}: export.directoryPattern is required for custom layout"
                ))
            })?,
            filename_pattern: profile.export.filename_pattern.clone().ok_or_else(|| {
                LambError::Validation(format!(
                    "profile {name}: export.filenamePattern is required for custom layout"
                ))
            })?,
        },
    };

    Ok(ResolvedExportPolicy {
        output_dir,
        layout,
        activity: ResolvedActivityPolicy {
            detector,
            channels,
            whole_export_exact_zero_gate,
            trim_leading_silence,
        },
    })
}

fn capture_port_export_mode(
    profile: &ProfileConfig,
    channel_name: &str,
) -> Option<ChannelExportMode> {
    profile
        .capture
        .ports
        .iter()
        .chain(profile.pipewire.capture_ports.iter())
        .find(|port| port.name.as_deref().map(str::trim) == Some(channel_name))
        .and_then(|port| port.export_mode)
}

pub fn resolve_active_profile(cfg: &AppConfig) -> Result<Option<ResolvedProfile>> {
    let Some(name) = cfg
        .daemon
        .active_profile
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return Ok(None);
    };
    let profile = cfg
        .profiles
        .get(name)
        .ok_or_else(|| LambError::Validation(format!("active profile {name} is not defined")))?;
    validate_profile(name, profile).map(Some)
}

pub fn load_config_for_mutation(path: &Path) -> Result<AppConfig> {
    let loaded = app_config::load_optional_config(path)?;
    match loaded.state {
        ConfigLoadState::Missing | ConfigLoadState::Loaded => Ok(loaded.config),
        ConfigLoadState::Invalid => {
            Err(LambError::Config(loaded.error.unwrap_or_else(|| {
                format!("invalid config file: {}", path.display())
            })))
        }
    }
}

pub fn save_config(path: &Path, cfg: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    let mut text = toml::to_string_pretty(cfg)
        .map_err(|err| LambError::Config(format!("failed to serialize app config: {err}")))?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    fs::write(path, text).map_err(|source| io_error(path, source))
}

pub fn create_profile(cfg: &mut AppConfig, name: &str, backend: &str) -> Result<()> {
    require_non_empty("profile name", name)?;
    if backend != "jack" && backend != "pipewire" {
        return Err(LambError::Validation(format!(
            "profile {name}: backend must be jack or pipewire, got {backend}"
        )));
    }
    if cfg.profiles.contains_key(name) {
        return Err(LambError::Config(format!("profile {name} already exists")));
    }
    cfg.profiles.insert(
        name.to_string(),
        ProfileConfig {
            backend: Some(backend.to_string()),
            ..ProfileConfig::default()
        },
    );
    Ok(())
}

pub fn set_profile_field(cfg: &mut AppConfig, name: &str, field: &str, value: &str) -> Result<()> {
    let profile = profile_mut(cfg, name)?;
    match field {
        "backend" => {
            if value != "jack" && value != "pipewire" {
                return Err(LambError::Validation(format!(
                    "profile {name}: backend must be jack or pipewire, got {value}"
                )));
            }
            profile.backend = Some(value.to_string());
        }
        "clientName" => profile.client_name = Some(non_empty_value(field, value)?),
        "buffer.seconds" => {
            let seconds = value.parse::<u32>().map_err(|_| {
                LambError::Validation(format!("profile {name}: buffer.seconds must be an integer"))
            })?;
            profile.buffer.seconds = Some(seconds);
        }
        "export.outputDir" => profile.export.output_dir = Some(PathBuf::from(value)),
        "export.mode" => profile.export.mode = Some(non_empty_value(field, value)?),
        "export.format" => profile.export.format = Some(non_empty_value(field, value)?),
        other => {
            return Err(LambError::Validation(format!(
                "unknown profile field {other}"
            )))
        }
    }
    Ok(())
}

pub fn add_capture_port(cfg: &mut AppConfig, name: &str, source: &str, label: &str) -> Result<()> {
    let profile = profile_mut(cfg, name)?;
    if !profile.capture.sources.is_empty() {
        return Err(LambError::Validation(format!(
            "profile {name}: cannot add capture.ports while capture.sources is set"
        )));
    }
    profile.capture.ports.push(CapturePort {
        source: Some(non_empty_value("source", source)?),
        name: Some(non_empty_value("name", label)?),
        export_mode: None,
    });
    Ok(())
}

fn resolve_capture_ports(name: &str, profile: &ProfileConfig) -> Result<Vec<ResolvedCapturePort>> {
    let has_ports = !profile.capture.ports.is_empty();
    let has_sources = !profile.capture.sources.is_empty();
    if has_ports && has_sources {
        return Err(LambError::Validation(format!(
            "profile {name}: must not specify both capture.ports and capture.sources"
        )));
    }
    if has_ports {
        return profile
            .capture
            .ports
            .iter()
            .enumerate()
            .map(|(index, port)| {
                Ok(ResolvedCapturePort {
                    source: required_string(
                        &format!("capture.ports[{index}].source"),
                        port.source.as_deref(),
                    )?,
                    name: required_string(
                        &format!("capture.ports[{index}].name"),
                        port.name.as_deref(),
                    )?,
                })
            })
            .collect();
    }
    if has_sources {
        return profile
            .capture
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                Ok(ResolvedCapturePort {
                    source: non_empty_value(&format!("capture.sources[{index}]"), source)?,
                    name: format!("ch{:02}", index + 1),
                })
            })
            .collect();
    }
    Err(LambError::Validation(format!(
        "profile {name}: capture.ports or capture.sources is required"
    )))
}

fn profile_mut<'a>(cfg: &'a mut AppConfig, name: &str) -> Result<&'a mut ProfileConfig> {
    cfg.profiles
        .get_mut(name)
        .ok_or_else(|| LambError::Config(format!("profile {name} does not exist")))
}

fn required_string(field: &str, value: Option<&str>) -> Result<String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Ok(v.to_string()),
        None => Err(LambError::Validation(format!("{field} is required"))),
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<()> {
    non_empty_value(field, value).map(|_| ())
}

fn non_empty_value(field: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(LambError::Validation(format!("{field} must be non-empty")));
    }
    Ok(value.to_string())
}
