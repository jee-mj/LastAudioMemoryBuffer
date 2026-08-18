use lamb::capture_arena::{CaptureArena, CaptureRuntimeConfig};
use lamb::capture_pipewire::{
    process_interleaved_f32_chunk, resolve_target, resolve_target_from_nodes, AvailableNode,
    PipeWireCapture, PipeWireCaptureConfig, ResolvedTarget,
};
use lamb::capture_runtime::CaptureRuntimeParams;
use lamb::config::{ExportConfig, LambConfig, MemoryConfig};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn cfg(target: Option<&str>) -> PipeWireCaptureConfig {
    PipeWireCaptureConfig {
        target: target.map(str::to_string),
        channels: Some(2),
        sample_rate: 48_000,
        dont_remix: true,
        channel_map: Vec::new(),
        latency: None,
    }
}

fn node(id: u32, object_type: &str, media_class: &str, name: &str) -> AvailableNode {
    AvailableNode {
        id,
        object_type: object_type.to_string(),
        media_class: Some(media_class.to_string()),
        name: Some(name.to_string()),
        description: Some(format!("description for {name}")),
        channels: Some(2),
        sample_rate: Some(48_000),
        format: Some("F32LE".to_string()),
    }
}

#[test]
fn target_selection_accepts_only_input_source_nodes() {
    let source = node(
        10,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let resolved = resolve_target_from_nodes(&cfg(Some("studio-input")), &[source]).unwrap();

    assert_eq!(resolved.id, Some(10));
    assert_eq!(resolved.name, "studio-input");
    assert_eq!(resolved.channels, 2);
    assert_eq!(resolved.sample_rate, 48_000);
    assert_eq!(resolved.format, "F32LE");
}

#[test]
fn target_selection_rejects_sinks_monitors_and_devices() {
    for rejected in [
        node(20, "PipeWire:Interface:Node", "Audio/Sink", "studio-output"),
        node(
            21,
            "PipeWire:Interface:Node",
            "Audio/Source",
            "studio-output.monitor",
        ),
        node(
            22,
            "PipeWire:Interface:Device",
            "Audio/Device",
            "scarlett-device",
        ),
    ] {
        let err =
            resolve_target_from_nodes(&cfg(rejected.name.as_deref()), &[rejected]).unwrap_err();
        assert!(
            err.to_string()
                .contains("target is not an input/source node"),
            "unexpected error: {err}"
        );
    }
}

#[test]
fn default_target_selects_first_available_input_source() {
    let sink = node(30, "PipeWire:Interface:Node", "Audio/Sink", "studio-output");
    let source = node(
        31,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );

    let resolved = resolve_target_from_nodes(&cfg(None), &[sink, source]).unwrap();

    assert_eq!(resolved.id, Some(31));
    assert_eq!(resolved.name, "studio-input");
}

fn test_runtime() -> (CaptureArena, lamb::capture_arena::CaptureIngress) {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 64,
        channels: 2,
        sample_rate: 48_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 8,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 3_900_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 8,
        capture_slot_frames: 8,
        capture_worker_stack_bytes: 64 * 1024,
        io_buffer_bytes_per_channel: 4096,
        maximum_path_bytes: 512,
        headroom: 1.0,
    })
    .unwrap();
    CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: 2,
                sample_rate: 48_000,
                format: SampleFormat::F32Le,
                chunk_frames: 8,
                chunk_count: 8,
                max_active_snapshots: 1,
            },
            queue_slots: 8,
            slot_frames: 8,
            sample_bytes: 4,
            worker_stack_bytes: 64 * 1024,
        },
    )
    .unwrap()
}

#[test]
fn process_chunk_respects_pipewire_offset_size_and_stride() {
    let (arena, ingress) = test_runtime();
    let samples = [99.0_f32, 99.0, 1.0, 2.0, 3.0, 4.0, 88.0, 88.0];
    let bytes = unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr().cast::<u8>(),
            samples.len() * std::mem::size_of::<f32>(),
        )
    };

    process_interleaved_f32_chunk(bytes, 8, 16, 8, 2, &ingress).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = arena.status(Duration::from_secs(1)).unwrap();
        if status.worker_written_frames >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "capture worker did not drain the ingress"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let frozen = arena
        .freeze_since(None, Duration::from_secs(1))
        .unwrap()
        .expect("two frames were written");
    let mut interleaved = vec![0.0; 4];
    frozen
        .copy_interleaved_range_into(frozen.absolute_range(), &mut interleaved)
        .unwrap();
    assert_eq!(interleaved, vec![1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn live_resolver_uses_the_public_capture_config_contract() {
    let _resolver: fn(&PipeWireCaptureConfig) -> lamb::error::Result<ResolvedTarget> =
        resolve_target;
}

#[test]
fn pipewire_capture_exposes_start_stop_and_resolved_target_api() {
    let _start: fn(
        PipeWireCaptureConfig,
        CaptureRuntimeParams,
    ) -> lamb::error::Result<(
        PipeWireCapture,
        lamb::capture_runtime::CaptureRuntime,
    )> = PipeWireCapture::start;
    let _resolved_target: for<'a> fn(&'a PipeWireCapture) -> &'a ResolvedTarget =
        PipeWireCapture::resolved_target;
    let _stop: fn(PipeWireCapture) = PipeWireCapture::stop;
}

#[test]
fn pipewire_capture_config_is_derived_from_lamb_config() {
    let lamb_cfg = LambConfig {
        config_version: 1,
        user: "<USERNAME>".to_string(),
        target: Some("studio-input".to_string()),
        backend: "pipewire".to_string(),
        channels: Some(4),
        channel_map: vec![
            "FL".to_string(),
            "FR".to_string(),
            "RL".to_string(),
            "RR".to_string(),
        ],
        seconds: 10,
        sample_rate: 48_000,
        sample_format: "F32LE".to_string(),
        latency: Some("256/48000".to_string()),
        dont_remix: true,
        output_dir: PathBuf::from("/tmp/lamb"),
        memory: MemoryConfig {
            max: None,
            headroom: 1.25,
        },
        max_active_snapshots: 1,
        allow_queued_recall: false,
        chunk_frames: Some(128),
        control_socket_path: PathBuf::from("/tmp/lamb.sock"),
        control_permissions: "0600".to_string(),
        export: ExportConfig {
            mode: "per-channel".to_string(),
            format: "wav".to_string(),
            split_when_over_bytes: 3_900_000_000,
        },
    };

    let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&lamb_cfg);

    assert_eq!(pipewire_cfg.target.as_deref(), Some("studio-input"));
    assert_eq!(pipewire_cfg.channels, Some(4));
    assert_eq!(pipewire_cfg.sample_rate, 48_000);
    assert!(pipewire_cfg.dont_remix);
    assert_eq!(pipewire_cfg.channel_map, lamb_cfg.channel_map);
    assert_eq!(pipewire_cfg.latency.as_deref(), Some("256/48000"));
}

#[test]
fn resolved_target_log_message_includes_target_and_negotiated_format() {
    let target = ResolvedTarget {
        id: Some(10),
        name: "studio-input".to_string(),
        description: Some("Studio Input".to_string()),
        channels: 2,
        sample_rate: 48_000,
        format: "F32LE".to_string(),
    };

    assert_eq!(
        target.log_message(),
        "resolved PipeWire target: studio-input (10), channels=2, sample_rate=48000, format=F32LE"
    );
}
