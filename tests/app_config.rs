use lamb::activity::{ActivityDetectorKind, ChannelExportMode, ThresholdSource};
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
            export_mode: None,
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

#[test]
fn typed_activity_config_parses_serializes_and_resolves_by_channel_name() {
    let text = pipewire_profile_text("")
        .replace(
            "{ source = \"capture_AUX2\", name = \"percL\" }",
            "{ source = \"capture_AUX2\", name = \"percL\", exportMode = \"auto\" }",
        )
        .replace(
            "format = \"wav\"",
            r#"format = "wav"
defaultChannelMode = "never"
activityDetector = "windowed-rms-peak"

[profiles.scarlett.channels.percL.activity]
thresholdDbFS = -63.4
thresholdSource = "calibrated"
updatedAtUnixSeconds = 1787616000
inputId = "input-1"
calibrationId = "calibration-1""#,
        );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
    let raw = &cfg.profiles["scarlett"];

    assert_eq!(
        raw.channels["percL"]
            .activity
            .as_ref()
            .unwrap()
            .threshold_source,
        ThresholdSource::Calibrated
    );
    assert!(toml::to_string(raw)
        .unwrap()
        .contains("thresholdDbFS = -63.4"));

    let resolved = profile::resolve_active_profile(&cfg).unwrap().unwrap();
    assert_eq!(
        resolved.export_policy.activity.detector,
        ActivityDetectorKind::WindowedRmsPeak
    );
    assert_eq!(
        resolved.export_policy.activity.channels[0].mode,
        ChannelExportMode::Auto
    );
    assert_eq!(
        resolved.export_policy.activity.channels[1].mode,
        ChannelExportMode::Never
    );
    assert_eq!(
        resolved.export_policy.activity.channels[0]
            .threshold
            .as_ref()
            .unwrap()
            .threshold_dbfs,
        -63.4
    );
}

#[test]
fn modern_activity_omission_resolves_to_auto_windowed_detector_and_trim() {
    let resolved = profile::resolve_active_profile(&parsed_pipewire_profile())
        .unwrap()
        .unwrap();

    assert_eq!(
        resolved.export_policy.activity.detector,
        ActivityDetectorKind::WindowedRmsPeak
    );
    assert!(resolved
        .export_policy
        .activity
        .channels
        .iter()
        .all(|channel| channel.mode == ChannelExportMode::Auto));
    assert!(!resolved.export_policy.activity.whole_export_exact_zero_gate);
    assert!(resolved.export_policy.activity.trim_leading_silence);
}

#[test]
fn silence_policy_conflicts_with_profile_wide_activity_fields() {
    for (field, expected) in [
        (
            "defaultChannelMode = \"auto\"",
            "validation error: profile scarlett: export.silencePolicy conflicts with export.defaultChannelMode",
        ),
        (
            "activityDetector = \"exact-zero\"",
            "validation error: profile scarlett: export.silencePolicy conflicts with export.activityDetector",
        ),
    ] {
        let text = pipewire_profile_text("").replace(
            "format = \"wav\"",
            &format!(
                "format = \"wav\"\nsilencePolicy = \"per-channel-exact-zero\"\n{field}"
            ),
        );
        let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
        assert_eq!(
            profile::resolve_active_profile(&cfg)
                .unwrap_err()
                .to_string(),
            expected
        );
    }
}

#[test]
fn all_channels_preset_allows_per_port_override() {
    let text = pipewire_profile_text("")
        .replace(
            "{ source = \"capture_AUX2\", name = \"percL\" }",
            "{ source = \"capture_AUX2\", name = \"percL\", exportMode = \"never\" }",
        )
        .replace(
            "format = \"wav\"",
            "format = \"wav\"\nsilencePolicy = \"all-channels-exact-zero\"",
        );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
    let resolved = profile::resolve_active_profile(&cfg).unwrap().unwrap();

    assert!(resolved.export_policy.activity.whole_export_exact_zero_gate);
    assert!(!resolved.export_policy.activity.trim_leading_silence);
    assert_eq!(
        resolved.export_policy.activity.detector,
        ActivityDetectorKind::ExactZero
    );
    assert_eq!(
        resolved.export_policy.activity.channels[0].mode,
        ChannelExportMode::Never
    );
    assert_eq!(
        resolved.export_policy.activity.channels[1].mode,
        ChannelExportMode::Always
    );
}

#[test]
fn per_channel_exact_zero_preset_resolves_auto_detector_and_trim() {
    let text = pipewire_profile_text("").replace(
        "format = \"wav\"",
        "format = \"wav\"\nsilencePolicy = \"per-channel-exact-zero\"",
    );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
    let resolved = profile::resolve_active_profile(&cfg).unwrap().unwrap();

    assert_eq!(
        resolved.export_policy.activity.detector,
        ActivityDetectorKind::ExactZero
    );
    assert!(!resolved.export_policy.activity.whole_export_exact_zero_gate);
    assert!(resolved.export_policy.activity.trim_leading_silence);
    assert!(resolved
        .export_policy
        .activity
        .channels
        .iter()
        .all(|channel| channel.mode == ChannelExportMode::Auto));
}

#[test]
fn reserved_activity_detectors_are_rejected_with_stable_errors() {
    for (detector, expected) in [
        (
            "fixed-level",
            "validation error: profile scarlett: export.activityDetector fixed-level is reserved and not supported",
        ),
        (
            "calibrated-noise-floor",
            "validation error: profile scarlett: export.activityDetector calibrated-noise-floor is reserved and not supported",
        ),
    ] {
        let text = pipewire_profile_text("").replace(
            "format = \"wav\"",
            &format!("format = \"wav\"\nactivityDetector = \"{detector}\""),
        );
        let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
        assert_eq!(
            profile::resolve_active_profile(&cfg)
                .unwrap_err()
                .to_string(),
            expected
        );
    }
}

#[test]
fn activity_threshold_must_be_finite_and_in_dbfs_range() {
    for threshold in ["nan", "-120.1", "0.1"] {
        let text = pipewire_profile_text("").replace(
            "format = \"wav\"",
            &format!(
                r#"format = "wav"

[profiles.scarlett.channels.percL.activity]
thresholdDbFS = {threshold}
thresholdSource = "manual"
updatedAtUnixSeconds = 1
inputId = "input-1""#
            ),
        );
        let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();
        assert_eq!(
            profile::resolve_active_profile(&cfg)
                .unwrap_err()
                .to_string(),
            "validation error: profile scarlett: channels.percL.activity.thresholdDbFS must be finite and within [-120.0, 0.0]"
        );
    }
}

#[test]
fn activity_channel_keys_must_match_one_configured_port_exactly() {
    let text = pipewire_profile_text("").replace(
        "format = \"wav\"",
        r#"format = "wav"

[profiles.scarlett.channels.PERCL.activity]
thresholdDbFS = -60.0
thresholdSource = "manual"
updatedAtUnixSeconds = 1
inputId = "input-1""#,
    );
    let cfg = parse_config_text(std::path::Path::new("profile.toml"), &text).unwrap();

    assert_eq!(
        profile::resolve_active_profile(&cfg)
            .unwrap_err()
            .to_string(),
        "validation error: profile scarlett: channels.PERCL does not match exactly one configured port name"
    );
}
