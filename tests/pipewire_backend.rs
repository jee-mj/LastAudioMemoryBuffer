use lamb::capture_arena::{CaptureArena, CaptureRuntimeConfig};
use lamb::capture_pipewire::{
    plan_explicit_links, process_interleaved_f32_chunk, resolve_target, resolve_target_from_graph,
    AvailableNode, AvailablePort, PipeWireCapture, PipeWireCaptureConfig, ResolvedSourcePort,
    ResolvedTarget,
};
use lamb::capture_runtime::CaptureRuntimeParams;
use lamb::config::{
    CapturePortConfig, ConfiguredCapturePort, ExportConfig, LambConfig, MemoryConfig,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn cfg(target: Option<&str>) -> PipeWireCaptureConfig {
    PipeWireCaptureConfig {
        target: target.map(str::to_string),
        capture_ports: vec![
            ConfiguredCapturePort {
                source: "capture_AUX2".to_string(),
                name: "percL".to_string(),
            },
            ConfiguredCapturePort {
                source: "capture_AUX0".to_string(),
                name: "mic".to_string(),
            },
        ],
        sample_rate: 48_000,
        dont_remix: true,
        latency: None,
    }
}

fn port(
    id: u32,
    node_id: u32,
    port_id: u32,
    direction: &str,
    name: &str,
    format_dsp: Option<&str>,
) -> AvailablePort {
    AvailablePort {
        id,
        object_type: "PipeWire:Interface:Port".to_string(),
        node_id: Some(node_id),
        direction: Some(direction.to_string()),
        port_id: Some(port_id),
        name: Some(name.to_string()),
        format_dsp: format_dsp.map(str::to_string),
        audio_format: None,
    }
}

fn target_ports(node_id: u32) -> Vec<AvailablePort> {
    vec![
        port(
            113,
            node_id,
            0,
            "out",
            "capture_AUX0",
            Some("32 bit float mono audio"),
        ),
        port(
            115,
            node_id,
            2,
            "out",
            "capture_AUX2",
            Some("32 bit float mono audio"),
        ),
    ]
}

fn resolved_sources() -> Vec<ResolvedSourcePort> {
    vec![
        ResolvedSourcePort {
            global_id: 115,
            node_id: 72,
            port_id: 2,
            name: "capture_AUX2".to_string(),
        },
        ResolvedSourcePort {
            global_id: 113,
            node_id: 72,
            port_id: 0,
            name: "capture_AUX0".to_string(),
        },
    ]
}

#[test]
fn explicit_link_plan_pairs_sources_with_stream_ports_by_numeric_port_id() {
    let stream_ports = vec![
        port(
            902,
            900,
            1,
            "in",
            "input_FR",
            Some("32 bit float mono audio"),
        ),
        port(
            901,
            900,
            0,
            "in",
            "input_FL",
            Some("32 bit float mono audio"),
        ),
    ];

    let links = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap();

    assert_eq!(links[0].output_node_id, 72);
    assert_eq!(links[0].output_port_id, 115);
    assert_eq!(links[0].input_node_id, 900);
    assert_eq!(links[0].input_port_id, 901);
    assert_eq!(links[1].output_port_id, 113);
    assert_eq!(links[1].input_port_id, 902);
}

#[test]
fn explicit_link_plan_rejects_too_few_stream_ports() {
    let err = plan_explicit_links(
        &resolved_sources(),
        900,
        &[port(901, 900, 0, "in", "input_FL", Some("audio"))],
    )
    .unwrap_err();

    assert!(err
        .to_string()
        .contains("destination port count 1 does not match source count 2"));
}

#[test]
fn explicit_link_plan_rejects_too_many_stream_ports() {
    let stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 1, "in", "input_FR", Some("audio")),
        port(903, 900, 2, "in", "input_FC", Some("audio")),
    ];

    let err = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap_err();

    assert!(err
        .to_string()
        .contains("destination port count 3 does not match source count 2"));
}

#[test]
fn explicit_link_plan_rejects_duplicate_destination_port_id() {
    let stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 0, "in", "input_FR", Some("audio")),
    ];

    let err = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap_err();

    assert!(err.to_string().contains("duplicate destination port.id 0"));
}

#[test]
fn explicit_link_plan_rejects_missing_destination_port_id() {
    let mut stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 1, "in", "input_FR", Some("audio")),
    ];
    stream_ports[1].port_id = None;

    let err = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap_err();

    assert!(err
        .to_string()
        .contains("destination port 902 is missing port.id"));
}

#[test]
fn explicit_link_plan_ignores_owned_monitor_outputs() {
    let stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 1, "in", "input_FR", Some("audio")),
        port(903, 900, 0, "out", "monitor_FL", Some("audio")),
    ];

    assert_eq!(
        plan_explicit_links(&resolved_sources(), 900, &stream_ports)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn explicit_link_plan_rejects_non_audio_destination() {
    let stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 1, "in", "input_FR", Some("8 bit raw midi")),
    ];

    let err = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap_err();

    assert!(err
        .to_string()
        .contains("destination port 902 is not audio"));
}

#[test]
fn explicit_link_plan_rejects_non_port_destination_object() {
    let mut stream_ports = vec![
        port(901, 900, 0, "in", "input_FL", Some("audio")),
        port(902, 900, 1, "in", "input_FR", Some("audio")),
    ];
    stream_ports[1].object_type = "PipeWire:Interface:Node".to_string();

    let err = plan_explicit_links(&resolved_sources(), 900, &stream_ports).unwrap_err();

    assert!(err
        .to_string()
        .contains("destination port 902 has object type 'PipeWire:Interface:Node'"));
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
    let resolved =
        resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &target_ports(10))
            .unwrap();

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
        let err = resolve_target_from_graph(
            &cfg(rejected.name.as_deref()),
            &[rejected],
            &target_ports(20),
        )
        .unwrap_err();
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

    let resolved =
        resolve_target_from_graph(&cfg(None), &[sink, source], &target_ports(31)).unwrap();

    assert_eq!(resolved.id, Some(31));
    assert_eq!(resolved.name, "studio-input");
}

#[test]
fn explicit_sources_resolve_in_configured_order() {
    let mut source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    source.channels = Some(16);
    let registry_ports = vec![
        port(
            113,
            72,
            0,
            "out",
            "capture_AUX0",
            Some("32 bit float mono audio"),
        ),
        port(
            115,
            72,
            2,
            "out",
            "capture_AUX2",
            Some("32 bit float mono audio"),
        ),
    ];

    let resolved =
        resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports).unwrap();

    assert_eq!(resolved.channels, 2);
    assert_eq!(resolved.source_ports[0].global_id, 115);
    assert_eq!(resolved.source_ports[0].name, "capture_AUX2");
    assert_eq!(resolved.source_ports[1].global_id, 113);
    assert_eq!(resolved.source_ports[1].name, "capture_AUX0");
}

#[test]
fn missing_source_lists_available_audio_outputs_in_sorted_order() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let registry_ports = vec![
        port(113, 72, 0, "out", "zeta", Some("audio")),
        port(114, 72, 1, "out", "capture_AUX0", Some("audio")),
        port(116, 72, 3, "out", "alpha", Some("audio")),
        port(117, 72, 4, "in", "ignored-input", Some("audio")),
        port(118, 72, 5, "out", "ignored-midi", Some("8 bit raw midi")),
    ];

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();
    let message = err.to_string();

    assert!(message.contains("source port 'capture_AUX2' was not found on target 'studio-input'"));
    assert!(message.contains("available target audio outputs: alpha, capture_AUX0, zeta"));
}

#[test]
fn source_existing_only_on_another_node_reports_wrong_node() {
    let selected = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let other = node(99, "PipeWire:Interface:Node", "Audio/Source", "other-input");
    let registry_ports = vec![port(215, 99, 2, "out", "capture_AUX2", Some("audio"))];

    let err = resolve_target_from_graph(
        &cfg(Some("studio-input")),
        &[selected, other],
        &registry_ports,
    )
    .unwrap_err();

    assert!(err.to_string().contains(
        "source port 'capture_AUX2' belongs to node 'other-input', expected 'studio-input'"
    ));
}

#[test]
fn wrong_node_diagnostic_is_independent_of_registry_order() {
    let selected = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let alpha = node(
        101,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "alpha-input",
    );
    let zeta = node(99, "PipeWire:Interface:Node", "Audio/Source", "zeta-input");
    let ports = vec![
        port(215, 99, 2, "out", "capture_AUX2", Some("audio")),
        port(216, 101, 2, "out", "capture_AUX2", Some("audio")),
    ];

    let forward = resolve_target_from_graph(
        &cfg(Some("studio-input")),
        &[selected.clone(), alpha.clone(), zeta.clone()],
        &ports,
    )
    .unwrap_err()
    .to_string();
    let mut reversed = ports;
    reversed.reverse();
    let backward = resolve_target_from_graph(
        &cfg(Some("studio-input")),
        &[selected, zeta, alpha],
        &reversed,
    )
    .unwrap_err()
    .to_string();

    assert_eq!(forward, backward);
    assert!(forward.contains("belongs to node 'alpha-input', expected 'studio-input'"));
}

#[test]
fn wrong_direction_source_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let registry_ports = vec![port(115, 72, 2, "in", "capture_AUX2", Some("audio"))];

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' has direction 'in'; expected 'out'"));
}

#[test]
fn selected_node_match_is_preferred_before_wrong_node_diagnostics() {
    let selected = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let other = node(99, "PipeWire:Interface:Node", "Audio/Source", "other-input");
    let registry_ports = vec![
        port(115, 72, 2, "in", "capture_AUX2", Some("audio")),
        port(215, 99, 2, "out", "capture_AUX2", Some("audio")),
    ];

    let err = resolve_target_from_graph(
        &cfg(Some("studio-input")),
        &[selected, other],
        &registry_ports,
    )
    .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' has direction 'in'; expected 'out'"));
}

#[test]
fn non_audio_source_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let registry_ports = vec![port(
        115,
        72,
        2,
        "out",
        "capture_AUX2",
        Some("8 bit raw midi"),
    )];

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' is not audio (format.dsp='8 bit raw midi')"));
}

#[test]
fn audio_metadata_rules_accept_terminal_audio_word_or_nonempty_audio_format() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let mut registry_ports = vec![
        port(113, 72, 0, "out", "capture_AUX0", Some("mono AuDiO")),
        port(115, 72, 2, "out", "capture_AUX2", Some("8 bit raw midi")),
    ];
    registry_ports[1].audio_format = Some(" F32LE ".to_string());

    let resolved =
        resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports).unwrap();

    assert_eq!(resolved.source_ports.len(), 2);
}

#[test]
fn duplicate_selected_node_source_name_is_ambiguous() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let registry_ports = vec![
        port(115, 72, 2, "out", "capture_AUX2", Some("audio")),
        port(116, 72, 3, "out", "capture_AUX2", Some("audio")),
    ];

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' is ambiguous on target 'studio-input'"));
}

#[test]
fn duplicate_programmatic_capture_source_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let mut config = cfg(Some("studio-input"));
    config.capture_ports[1].source = "capture_AUX2".to_string();

    let err = resolve_target_from_graph(&config, &[source], &target_ports(72)).unwrap_err();

    assert!(err
        .to_string()
        .contains("capture source 'capture_AUX2' is configured more than once"));
}

#[test]
fn matching_global_with_wrong_object_type_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let mut registry_ports = target_ports(72);
    registry_ports[1].object_type = "PipeWire:Interface:Node".to_string();

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err.to_string().contains(
        "source port 'capture_AUX2' has object type 'PipeWire:Interface:Node'; expected 'PipeWire:Interface:Port'"
    ));
}

#[test]
fn matching_source_without_node_id_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let mut registry_ports = target_ports(72);
    registry_ports[1].node_id = None;

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' is missing node.id"));
}

#[test]
fn matching_source_without_port_id_is_rejected() {
    let source = node(
        72,
        "PipeWire:Interface:Node",
        "Audio/Source",
        "studio-input",
    );
    let mut registry_ports = target_ports(72);
    registry_ports[1].port_id = None;

    let err = resolve_target_from_graph(&cfg(Some("studio-input")), &[source], &registry_ports)
        .unwrap_err();

    assert!(err
        .to_string()
        .contains("source port 'capture_AUX2' is missing port.id"));
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
        maximum_calibration_seconds: 0,
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
        channels: None,
        channel_map: None,
        capture_ports: vec![
            CapturePortConfig {
                source: Some("capture_AUX0".to_string()),
                name: Some("mic".to_string()),
            },
            CapturePortConfig {
                source: Some("capture_AUX1".to_string()),
                name: Some("gtr".to_string()),
            },
            CapturePortConfig {
                source: Some("capture_AUX2".to_string()),
                name: Some("percL".to_string()),
            },
            CapturePortConfig {
                source: Some("capture_AUX3".to_string()),
                name: Some("percR".to_string()),
            },
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

    let pipewire_cfg = PipeWireCaptureConfig::from_lamb_config(&lamb_cfg).unwrap();

    assert_eq!(pipewire_cfg.target.as_deref(), Some("studio-input"));
    assert_eq!(pipewire_cfg.channel_count().unwrap(), 4);
    assert_eq!(
        pipewire_cfg
            .capture_ports
            .iter()
            .map(|port| (port.source.as_str(), port.name.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("capture_AUX0", "mic"),
            ("capture_AUX1", "gtr"),
            ("capture_AUX2", "percL"),
            ("capture_AUX3", "percR"),
        ]
    );
    assert_eq!(pipewire_cfg.sample_rate, 48_000);
    assert!(pipewire_cfg.dont_remix);
    assert_eq!(
        pipewire_cfg.channel_names(),
        vec!["mic", "gtr", "percL", "percR"]
    );
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
        source_ports: Vec::new(),
    };

    assert_eq!(
        target.log_message(),
        "resolved PipeWire target: studio-input (10), channels=2, sample_rate=48000, format=F32LE"
    );
}
