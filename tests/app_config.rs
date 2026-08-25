use lamb::app_config::{
    default_config_path_from_env, default_config_text, load_optional_config, parse_config_text,
    write_default_config, AppConfig, ConfigLoadState,
};
use lamb::profile;
use std::collections::BTreeMap;
use std::fs;

#[test]
fn default_config_text_parses_as_manual_unconfigured() {
    let cfg: AppConfig = toml::from_str(default_config_text()).unwrap();

    assert_eq!(cfg.daemon.start_mode, "manual");
    assert_eq!(cfg.daemon.active_profile, None);
    assert_eq!(cfg.profiles, BTreeMap::new());
}

#[test]
fn missing_config_loads_default_unconfigured_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing.toml");

    let loaded = load_optional_config(&path).unwrap();

    assert_eq!(loaded.state, ConfigLoadState::Missing);
    assert_eq!(loaded.error, None);
    assert_eq!(loaded.config.daemon.start_mode, "manual");
    assert_eq!(loaded.config.daemon.active_profile, None);
    assert!(loaded.config.profiles.is_empty());
}

#[test]
fn invalid_config_loads_default_unconfigured_state_with_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    fs::write(&path, "not = [valid\n").unwrap();

    let loaded = load_optional_config(&path).unwrap();

    assert_eq!(loaded.state, ConfigLoadState::Invalid);
    assert_eq!(loaded.config.daemon.start_mode, "manual");
    assert_eq!(loaded.config.daemon.active_profile, None);
    assert!(loaded.config.profiles.is_empty());
    assert!(loaded.error.unwrap().contains("failed to parse"));
}

#[test]
fn default_config_path_prefers_xdg_config_home() {
    let temp = tempfile::tempdir().unwrap();
    let path = default_config_path_from_env(Some(temp.path().into()), None).unwrap();

    assert_eq!(path, temp.path().join("lamb/lamb.toml"));
}

#[test]
fn default_config_path_falls_back_to_home_dot_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = default_config_path_from_env(None, Some(temp.path().into())).unwrap();

    assert_eq!(path, temp.path().join(".config/lamb/lamb.toml"));
}

#[test]
fn write_default_config_refuses_overwrite_without_force() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    fs::write(&path, "existing = true\n").unwrap();

    let err = write_default_config(&path, false).unwrap_err().to_string();

    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "existing = true\n");
}

#[test]
fn write_default_config_force_overwrites_and_creates_parent_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/lamb/lamb.toml");

    write_default_config(&path, true).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), default_config_text());
}

fn pipewire_profile_text(extra: &str) -> String {
    format!(
        r#"
[daemon]
startMode = "manual"
activeProfile = "scarlett"

[profiles.scarlett]
backend = "pipewire"

[profiles.scarlett.pipewire]
target = "studio-input"
capturePorts = [
  {{ source = "capture_AUX2", name = "percL" }},
  {{ source = "capture_AUX0", name = "mic" }},
]
{extra}

[profiles.scarlett.buffer]
seconds = 10

[profiles.scarlett.export]
outputDir = "/tmp/lamb-profile"
mode = "per-channel"
format = "wav"
"#
    )
}

fn parsed_pipewire_profile() -> AppConfig {
    parse_config_text(
        std::path::Path::new("profile.toml"),
        &pipewire_profile_text(""),
    )
    .unwrap()
}

// Production break: capturePorts must deserialize and determine resolved order and names.
#[test]
fn pipewire_capture_ports_parse_and_resolve_in_order() {
    let cfg = parsed_pipewire_profile();
    let resolved = profile::resolve_active_profile(&cfg)
        .unwrap()
        .expect("active profile");

    assert_eq!(resolved.ports[0].source, "capture_AUX2");
    assert_eq!(resolved.ports[0].name, "percL");
    assert_eq!(resolved.ports[1].source, "capture_AUX0");
    assert_eq!(resolved.ports[1].name, "mic");
    assert_eq!(
        resolved.pipewire_config.unwrap().channel_names(),
        vec!["percL".to_string(), "mic".to_string()]
    );
}

// Production break: an omitted PipeWire capturePorts array must be rejected for its profile.
#[test]
fn pipewire_profile_rejects_omitted_capture_ports() {
    let text = pipewire_profile_text("").replace(
        "capturePorts = [\n  { source = \"capture_AUX2\", name = \"percL\" },\n  { source = \"capture_AUX0\", name = \"mic\" },\n]\n",
        "",
    );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();

    assert_eq!(
        profile::resolve_active_profile(&cfg)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: pipewire.capturePorts is required"
    );
}

// Production break: an explicitly empty PipeWire capturePorts array must be rejected.
#[test]
fn pipewire_profile_rejects_empty_capture_ports() {
    let text = pipewire_profile_text("").replace(
        "capturePorts = [\n  { source = \"capture_AUX2\", name = \"percL\" },\n  { source = \"capture_AUX0\", name = \"mic\" },\n]",
        "capturePorts = []",
    );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();

    assert_eq!(
        profile::resolve_active_profile(&cfg)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: pipewire.capturePorts is required"
    );
}

// Production break: a missing indexed source must retain its exact field path.
#[test]
fn pipewire_profile_rejects_missing_capture_port_source() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[0].source = None;

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[0].source is required"
    );
}

// Production break: a whitespace-only source must be rejected after trimming.
#[test]
fn pipewire_profile_rejects_blank_capture_port_source() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[0].source = Some("  ".to_string());

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[0].source is required"
    );
}

// Production break: a missing indexed name must retain its exact field path.
#[test]
fn pipewire_profile_rejects_missing_capture_port_name() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[0].name = None;

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[0].name is required"
    );
}

// Production break: a whitespace-only name must be rejected after trimming.
#[test]
fn pipewire_profile_rejects_blank_capture_port_name() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[0].name = Some("  ".to_string());

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[0].name is required"
    );
}

// Production break: duplicate sources must be detected after normalization.
#[test]
fn pipewire_profile_rejects_normalized_duplicate_capture_port_source() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[1].source = Some(" capture_AUX2 ".to_string());

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[1].source duplicates pipewire.capturePorts[0].source"
    );
}

// Production break: duplicate names must be detected after normalization.
#[test]
fn pipewire_profile_rejects_normalized_duplicate_capture_port_name() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.capture_ports[1].name = Some(" percL ".to_string());

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: pipewire.capturePorts[1].name duplicates pipewire.capturePorts[0].name"
    );
}

// Production break: legacy channelMap presence must conflict even when the array is empty.
#[test]
fn pipewire_profile_rejects_empty_legacy_channel_map() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.pipewire.channel_map = Some(Vec::new());

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: pipewire.channelMap conflicts with pipewire.capturePorts"
    );
}

// Production break: a populated legacy channelMap must conflict with capturePorts.
#[test]
fn pipewire_profile_rejects_populated_legacy_channel_map() {
    let cfg = parse_config_text(
        std::path::Path::new("profile.toml"),
        &pipewire_profile_text("channelMap = [\"legacy\"]"),
    )
    .unwrap();

    assert_eq!(
        profile::resolve_active_profile(&cfg).unwrap_err().to_string(),
        "validation error: profile scarlett: pipewire.channelMap conflicts with pipewire.capturePorts"
    );
}

// Production break: generic capture.ports is JACK-only for PipeWire profiles.
#[test]
fn pipewire_profile_rejects_generic_capture_ports() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config
        .capture
        .ports
        .push(lamb::app_config::CapturePort {
            source: Some("system:capture_1".to_string()),
            name: Some("legacy".to_string()),
        });

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: capture.ports and capture.sources are only valid for jack profiles"
    );
}

// Production break: generic capture.sources is JACK-only for PipeWire profiles.
#[test]
fn pipewire_profile_rejects_generic_capture_sources() {
    let cfg = parsed_pipewire_profile();
    let mut profile_config = cfg.profiles.get("scarlett").unwrap().clone();
    profile_config.capture.sources = vec!["system:capture_1".to_string()];

    assert_eq!(
        profile::validate_profile("scarlett", &profile_config)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: capture.ports and capture.sources are only valid for jack profiles"
    );
}
