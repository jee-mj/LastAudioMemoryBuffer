use lamb::activity::{ActivityDetectorKind, ChannelExportMode};
use lamb::config::{load_config_file, CapturePortConfig, ExportConfig, LambConfig, MemoryConfig};
use lamb::export_policy::{ExportCommand, ResolvedLayout};
use lamb::math::{estimate_ring_bytes, wav_parts_for_channel};
use std::{fs, path::PathBuf};

fn valid_config() -> LambConfig {
    LambConfig {
        config_version: 1,
        user: "<USERNAME>".to_string(),
        target: None,
        backend: "fake".to_string(),
        channels: Some(4),
        channel_map: Some(Vec::new()),
        capture_ports: Vec::new(),
        seconds: 10,
        sample_rate: 44_100,
        sample_format: "F32LE".to_string(),
        latency: None,
        dont_remix: true,
        output_dir: PathBuf::from("/tmp/lamb-test"),
        memory: MemoryConfig {
            max: None,
            headroom: 1.25,
        },
        max_active_snapshots: 1,
        allow_queued_recall: false,
        chunk_frames: None,
        control_socket_path: PathBuf::from("%t/lamb/control.sock"),
        control_permissions: "0600".to_string(),
        export: ExportConfig {
            mode: "per-channel".to_string(),
            format: "wav".to_string(),
            split_when_over_bytes: 3_900_000_000,
        },
    }
}

fn capture_port(source: &str, name: &str) -> CapturePortConfig {
    CapturePortConfig {
        source: Some(source.to_string()),
        name: Some(name.to_string()),
    }
}

fn valid_pipewire_config() -> LambConfig {
    let mut cfg = valid_config();
    cfg.backend = "pipewire".to_string();
    cfg.target = Some("studio-input".to_string());
    cfg.channels = None;
    cfg.channel_map = None;
    cfg.capture_ports = vec![
        capture_port("capture_AUX0", "mic"),
        capture_port("capture_AUX1", "gtr"),
    ];
    cfg
}

#[test]
fn valid_config_passes_static_validation() {
    valid_config().validate_static().unwrap();
}

#[test]
fn pipewire_requires_capture_ports() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports.clear();
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("capturePorts is required for pipewire backend"),
        "{err}"
    );
}

#[test]
fn pipewire_rejects_missing_port_source() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports[0].source = None;
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(err.contains("capturePorts[0].source is required"), "{err}");
}

#[test]
fn pipewire_rejects_blank_port_name() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports[0].name = Some("  ".to_string());
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(err.contains("capturePorts[0].name is required"), "{err}");
}

#[test]
fn pipewire_rejects_duplicate_port_source_after_trimming() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports[1].source = Some(" capture_AUX0 ".to_string());
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("capturePorts[1].source duplicates capturePorts[0].source"),
        "{err}"
    );
}

#[test]
fn pipewire_rejects_duplicate_port_name_after_trimming() {
    let mut cfg = valid_pipewire_config();
    cfg.capture_ports[1].name = Some(" mic ".to_string());
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("capturePorts[1].name duplicates capturePorts[0].name"),
        "{err}"
    );
}

#[test]
fn pipewire_rejects_channels_field_presence() {
    let mut cfg = valid_pipewire_config();
    cfg.channels = Some(2);
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("channels conflicts with capturePorts"),
        "{err}"
    );
}

#[test]
fn pipewire_rejects_empty_channel_map_field_presence() {
    let mut cfg = valid_pipewire_config();
    cfg.channel_map = Some(Vec::new());
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("channelMap conflicts with capturePorts"),
        "{err}"
    );
}

#[test]
fn pipewire_ports_derive_ordered_channels_and_names() {
    let cfg = valid_pipewire_config();
    let ports = cfg.resolved_capture_ports().unwrap();
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].source, "capture_AUX0");
    assert_eq!(ports[0].name, "mic");
    assert_eq!(ports[1].source, "capture_AUX1");
    assert_eq!(ports[1].name, "gtr");
}

#[test]
fn parse_retains_socket_before_static_conflict_is_reported() {
    let text = r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
channels = 4
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "/tmp/lamb-out"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "/tmp/lamb-invalid.sock"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#;
    let path = std::path::Path::new("legacy.toml");
    let parsed = lamb::config::parse_config_text(path, text).unwrap();

    assert_eq!(
        parsed.control_socket_path,
        std::path::PathBuf::from("/tmp/lamb-invalid.sock")
    );
    assert!(parsed
        .validate_static()
        .unwrap_err()
        .to_string()
        .contains("channels conflicts with capturePorts"));
    assert!(lamb::config::load_config_text(path, text).is_err());
}

#[test]
fn toml_config_without_consent_loads() {
    let temp = tempfile::tempdir().unwrap();
    let output_dir = temp.path().join("out");
    fs::create_dir_all(&output_dir).unwrap();
    let socket = temp.path().join("lamb/control.sock");
    let config_path = temp.path().join("lamb.toml");

    fs::write(
        &config_path,
        format!(
            r#"
configVersion = 1
user = "<USERNAME>"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            output_dir.display(),
            socket.display()
        ),
    )
    .unwrap();

    let cfg = load_config_file(&config_path).unwrap();
    assert_eq!(cfg.user, "<USERNAME>");
    assert_eq!(cfg.backend, "fake");
    assert_eq!(cfg.channels, Some(2));
}

#[test]
fn channel_map_must_match_explicit_channels() {
    let mut cfg = valid_config();
    cfg.channels = Some(2);
    cfg.channel_map = Some(vec![
        "in1".to_string(),
        "in2".to_string(),
        "in3".to_string(),
    ]);
    let err = cfg.validate_static().unwrap_err().to_string();
    assert!(
        err.contains("channelMap length 3 must match channels 2"),
        "{err}"
    );
}

#[test]
fn checked_memory_estimate_for_current_target() {
    let bytes = estimate_ring_bytes(1_800, 44_100, 4, 4, 1.25).unwrap();
    assert!(bytes > 1_500_000_000);
    assert!(bytes < 1_700_000_000);
}

#[test]
fn wav_split_counts_parts_on_frame_boundaries() {
    let parts = wav_parts_for_channel(44_100 * 1_800, 3, 390_000).unwrap();
    assert!(parts.len() > 10);
    assert_eq!(parts[0].start_frame, 0);
    assert!(parts[0].frame_count > 0);
    assert_eq!(parts[1].start_frame, parts[0].frame_count);
}

#[test]
fn legacy_config_resolves_historical_export_policy_per_command() {
    let cfg = valid_config();

    let recall = cfg.resolved_export_policy(ExportCommand::Recall).unwrap();
    assert_eq!(recall.activity.detector, ActivityDetectorKind::ExactZero);
    assert!(recall.activity.whole_export_exact_zero_gate);
    assert!(!recall.activity.trim_leading_silence);
    assert!(recall
        .activity
        .channels
        .iter()
        .all(|channel| channel.mode == ChannelExportMode::Always));
    assert_eq!(recall.layout(), &ResolvedLayout::FlatDetailed);

    let dump = cfg.resolved_export_policy(ExportCommand::Dump).unwrap();
    assert_eq!(dump.layout(), &ResolvedLayout::TimestampDirectory);
    assert_eq!(dump.output_dir(), cfg.output_dir.as_path());

    let session = cfg.resolved_session_export_policy().unwrap();
    assert_eq!(session.layout(), &ResolvedLayout::CommandDefault);
    assert_eq!(session.activity, recall.activity);
    assert_eq!(session.output_dir(), cfg.output_dir.as_path());
}

#[test]
fn legacy_config_resolution_rejects_noncanonical_output_root() {
    let mut cfg = valid_config();
    cfg.output_dir = PathBuf::from("/tmp/lamb-test/../escape");

    assert!(cfg.resolved_session_export_policy().is_err());
}
