use lamb::config::{load_config_file, ExportConfig, LambConfig, MemoryConfig};
use lamb::math::{estimate_ring_bytes, wav_parts_for_channel};
use std::{fs, path::PathBuf};

fn valid_config() -> LambConfig {
    LambConfig {
        config_version: 1,
        user: "<USERNAME>".to_string(),
        target: None,
        backend: "fake".to_string(),
        channels: Some(4),
        channel_map: Vec::new(),
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

#[test]
fn valid_config_passes_static_validation() {
    valid_config().validate_static().unwrap();
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
    cfg.channel_map = vec!["in1".to_string(), "in2".to_string(), "in3".to_string()];
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
