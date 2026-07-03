use crate::app_config::{self, AppConfig, CapturePort, ConfigLoadState, ProfileConfig};
use crate::capture_pipewire::PipeWireCaptureConfig;
use crate::error::{io_error, LambError, Result};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub name: String,
    pub backend: String,
    pub client_name: String,
    pub ports: Vec<ResolvedCapturePort>,
    pub buffer_seconds: u32,
    pub export_output_dir: PathBuf,
    pub export_mode: String,
    pub export_format: String,
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
    let export_output_dir = validate_export_output_dir(name, profile)?;
    let (export_mode, export_format) = validate_export(name, profile)?;

    Ok(ResolvedProfile {
        name: name.to_string(),
        backend: "jack".to_string(),
        client_name,
        ports,
        buffer_seconds,
        export_output_dir,
        export_mode,
        export_format,
        pipewire_config: None,
    })
}

fn validate_pipewire_profile(name: &str, profile: &ProfileConfig) -> Result<ResolvedProfile> {
    let buffer_seconds = validate_buffer_seconds(name, profile)?;
    let export_output_dir = validate_export_output_dir(name, profile)?;
    let (export_mode, export_format) = validate_export(name, profile)?;

    let pw = &profile.pipewire;

    Ok(ResolvedProfile {
        name: name.to_string(),
        backend: "pipewire".to_string(),
        client_name: "lamb".to_string(),
        ports: pw
            .channel_map
            .iter()
            .enumerate()
            .map(|(i, name)| ResolvedCapturePort {
                source: format!("pipewire-input-ch{}", i + 1),
                name: name.clone(),
            })
            .collect(),
        buffer_seconds,
        export_output_dir,
        export_mode,
        export_format,
        pipewire_config: Some(PipeWireCaptureConfig {
            target: pw.target.clone(),
            channels: None,
            sample_rate: pw.sample_rate.unwrap_or(44100),
            dont_remix: pw.dont_remix,
            channel_map: pw.channel_map.clone(),
            latency: pw.latency.clone(),
        }),
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
