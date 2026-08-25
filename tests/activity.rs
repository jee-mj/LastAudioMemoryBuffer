use lamb::activity::{
    classify_samples, ActivityDetector, ActivityDetectorKind, ActivityResult, ChannelDisposition,
    ChannelExportMode, DetectorWorkspace, FrozenChannelDecision, FrozenExportDecision,
    SilencePolicyPreset, ThresholdSource, WindowedRmsPeakDetector,
};
use lamb::app_config::ActivityThresholdConfig;
use lamb::capture_arena::{CaptureArena, CaptureRuntimeConfig};
use lamb::export_policy::{ChannelActivityPolicy, ResolvedActivityPolicy};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(2);

#[test]
fn activity_enums_use_stable_kebab_case_names() {
    #[derive(serde::Deserialize)]
    struct Values {
        mode: ChannelExportMode,
        detector: ActivityDetectorKind,
        preset: SilencePolicyPreset,
        source: ThresholdSource,
    }

    let values: Values = toml::from_str(
        r#"
mode = "auto"
detector = "windowed-rms-peak"
preset = "all-channels-exact-zero"
source = "calibrated"
"#,
    )
    .unwrap();

    assert_eq!(values.mode, ChannelExportMode::Auto);
    assert_eq!(values.detector, ActivityDetectorKind::WindowedRmsPeak);
    assert_eq!(values.preset, SilencePolicyPreset::AllChannelsExactZero);
    assert_eq!(values.source, ThresholdSource::Calibrated);
}

#[test]
fn reserved_detector_names_still_deserialize() {
    #[derive(serde::Deserialize)]
    struct Detector {
        detector: ActivityDetectorKind,
    }

    let fixed: Detector = toml::from_str("detector = \"fixed-level\"").unwrap();
    let calibrated: Detector = toml::from_str("detector = \"calibrated-noise-floor\"").unwrap();

    assert_eq!(fixed.detector, ActivityDetectorKind::FixedLevel);
    assert_eq!(
        calibrated.detector,
        ActivityDetectorKind::CalibratedNoiseFloor
    );
}

fn plan(channels: u32, sample_rate: u32) -> SessionMemoryPlan {
    SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 1_000,
        channels,
        sample_rate,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 16,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 1_024,
        capture_queue_slots: 2,
        capture_slot_frames: 16,
        capture_worker_stack_bytes: 1_024,
        io_buffer_bytes_per_channel: 64,
        maximum_path_bytes: 128,
        headroom: 1.0,
    })
    .unwrap()
}

fn auto_policy(
    detector: ActivityDetectorKind,
    modes: &[ChannelExportMode],
) -> ResolvedActivityPolicy {
    ResolvedActivityPolicy {
        detector,
        channels: modes
            .iter()
            .enumerate()
            .map(|(index, &mode)| ChannelActivityPolicy {
                name: format!("channel-{index}"),
                mode,
                threshold: Some(ActivityThresholdConfig {
                    threshold_dbfs: -3.0,
                    threshold_source: ThresholdSource::Manual,
                    updated_at_unix_seconds: 0,
                    input_id: "test".into(),
                    calibration_id: None,
                }),
            })
            .collect(),
        whole_export_exact_zero_gate: false,
        trim_leading_silence: true,
    }
}

fn windowed_policy(
    modes: &[ChannelExportMode],
    threshold_dbfs: Option<f64>,
    whole_export_exact_zero_gate: bool,
) -> ResolvedActivityPolicy {
    let mut policy = auto_policy(ActivityDetectorKind::WindowedRmsPeak, modes);
    for channel in &mut policy.channels {
        channel.threshold = threshold_dbfs.map(|threshold_dbfs| ActivityThresholdConfig {
            threshold_dbfs,
            threshold_source: ThresholdSource::Manual,
            updated_at_unix_seconds: 0,
            input_id: "test".into(),
            calibration_id: None,
        });
    }
    policy.whole_export_exact_zero_gate = whole_export_exact_zero_gate;
    policy
}

fn frozen_runtime(
    channels: u32,
    sample_rate: u32,
) -> (
    SessionMemoryPlan,
    CaptureArena,
    lamb::capture_arena::CaptureIngress,
) {
    let memory_plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 1_000,
        channels,
        sample_rate,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 4,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 1_024,
        capture_queue_slots: 8,
        capture_slot_frames: 4,
        capture_worker_stack_bytes: 1_024,
        io_buffer_bytes_per_channel: 64,
        maximum_path_bytes: 128,
        headroom: 1.0,
    })
    .unwrap();
    let (arena, ingress) = CaptureArena::new(
        &memory_plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels,
                sample_rate,
                format: SampleFormat::F32Le,
                chunk_frames: 4,
                chunk_count: 250,
                max_active_snapshots: 1,
            },
            queue_slots: 8,
            slot_frames: 4,
            sample_bytes: 4,
            worker_stack_bytes: 1_024,
        },
    )
    .unwrap();
    (memory_plan, arena, ingress)
}

#[test]
fn exact_zero_keeps_only_nonzero_auto_channels_and_retains_ambiguous() {
    let memory_plan = plan(4, 100);
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let samples = [1.0, 0.0, 0.0, -0.0, 1.0, 0.01, 0.0, -0.0];
    let results = classify_samples(
        &samples,
        4,
        0,
        100,
        &auto_policy(
            ActivityDetectorKind::ExactZero,
            &[ChannelExportMode::Auto; 4],
        ),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(results[0].result, ActivityResult::Active);
    assert_eq!(results[1].result, ActivityResult::Active);
    assert_eq!(results[2].result, ActivityResult::Inactive);
    assert_eq!(results[3].result, ActivityResult::Inactive);
    assert_eq!(results[0].disposition, ChannelDisposition::Retain);
    assert_eq!(results[1].disposition, ChannelDisposition::Retain);
    assert_eq!(results[2].disposition, ChannelDisposition::Omit);
    assert_eq!(results[3].disposition, ChannelDisposition::Omit);
    assert_eq!(results[0].first_evidence_frame, Some(0));
    assert_eq!(results[1].first_evidence_frame, Some(1));
    assert_eq!(results[2].first_evidence_frame, None);
    assert_eq!(results[3].first_evidence_frame, None);
    let one = plan(1, 100);
    let mut one_workspace = DetectorWorkspace::new(&one).unwrap();
    let ambiguous = classify_samples(
        &[f32::INFINITY],
        1,
        0,
        100,
        &auto_policy(ActivityDetectorKind::ExactZero, &[ChannelExportMode::Auto]),
        &mut one_workspace,
    )
    .unwrap();
    assert_eq!(ambiguous[0].result, ActivityResult::Ambiguous);
    assert_eq!(ambiguous[0].disposition, ChannelDisposition::Retain);
}

#[test]
fn modes_and_whole_export_gate_have_separate_dispositions() {
    let plan = plan(3, 100);
    let mut workspace = DetectorWorkspace::new(&plan).unwrap();
    let all_never = classify_samples(
        &[1.0, 0.0, 0.0],
        3,
        0,
        100,
        &auto_policy(
            ActivityDetectorKind::ExactZero,
            &[ChannelExportMode::Never; 3],
        ),
        &mut workspace,
    )
    .unwrap();
    assert!(all_never
        .iter()
        .all(|entry| entry.disposition == ChannelDisposition::Omit));
    let mixed = classify_samples(
        &[0.0, 0.0, 0.0],
        3,
        0,
        100,
        &auto_policy(
            ActivityDetectorKind::ExactZero,
            &[
                ChannelExportMode::Never,
                ChannelExportMode::Auto,
                ChannelExportMode::Always,
            ],
        ),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(mixed[0].disposition, ChannelDisposition::Omit);
    assert_eq!(mixed[1].disposition, ChannelDisposition::Omit);
    assert_eq!(mixed[2].disposition, ChannelDisposition::Retain);
}

#[test]
fn windowed_detector_uses_20ms_windows_10ms_hop_and_earliest_window_start() {
    let plan = plan(1, 100);
    let mut workspace = DetectorWorkspace::new(&plan).unwrap();
    let detector = WindowedRmsPeakDetector;
    let samples = [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0];
    let result = detector
        .classify(&samples, 1, 10, 100, -3.0, &mut workspace)
        .unwrap();
    assert_eq!(result.result, ActivityResult::Ambiguous);
    assert_eq!(result.first_evidence_frame, Some(14));
}

#[test]
fn windowed_wrong_threshold_and_missing_sustained_duration_stay_non_active() {
    let memory_plan = plan(1, 100);
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let below_close = classify_samples(
        &[0.01; 40],
        1,
        40,
        100,
        &windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(below_close[0].result, ActivityResult::Inactive);
    assert_eq!(below_close[0].first_evidence_frame, None);

    let not_sustained = classify_samples(
        &[0.8; 9],
        1,
        40,
        100,
        &windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(not_sustained[0].result, ActivityResult::Ambiguous);
    assert_eq!(not_sustained[0].first_evidence_frame, Some(40));
}

#[test]
fn windowed_overlap_hysteresis_sustain_transient_and_partial_windows_are_exact() {
    let memory_plan = plan(1, 100);
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let policy = windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false);

    // 0.4 is approximately -7.96 dBFS: below the -3 dBFS open threshold,
    // but above its required -9 dBFS close/evidence threshold.
    let closed_in_band =
        classify_samples(&[0.4; 12], 1, 100, 100, &policy, &mut workspace).unwrap();
    assert_eq!(closed_in_band[0].result, ActivityResult::Ambiguous);
    assert_eq!(closed_in_band[0].first_evidence_frame, Some(100));

    let mut open_then_band = [0.4; 10];
    open_then_band[0] = 0.8;
    open_then_band[1] = 0.8;
    let sustained =
        classify_samples(&open_then_band, 1, 100, 100, &policy, &mut workspace).unwrap();
    assert_eq!(sustained[0].result, ActivityResult::Active);
    assert_eq!(sustained[0].first_evidence_frame, Some(100));

    let transient = classify_samples(&[4.0], 1, 250, 100, &policy, &mut workspace).unwrap();
    assert_eq!(transient[0].result, ActivityResult::Active);
    assert_eq!(transient[0].first_evidence_frame, Some(250));
}

#[test]
fn windowed_missing_threshold_fails_open_at_transaction_start() {
    let memory_plan = plan(1, 100);
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let results = classify_samples(
        &[0.0; 12],
        1,
        700,
        100,
        &windowed_policy(&[ChannelExportMode::Auto], None, false),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(results[0].result, ActivityResult::Ambiguous);
    assert_eq!(results[0].disposition, ChannelDisposition::Retain);
    assert_eq!(results[0].first_evidence_frame, Some(700));
}

#[test]
fn frozen_windowed_gate_state_crosses_workspace_copy_boundaries() {
    let (memory_plan, mut arena, ingress) = frozen_runtime(1, 100);
    let mut samples = [0.8; 15];
    samples[..3].fill(0.0);
    ingress.try_push_interleaved(&samples, 1).unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    let outcome = lamb::activity::classify_frozen_epoch(
        &frozen,
        &windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false),
        &mut workspace,
        &mut decision,
    )
    .unwrap();
    assert!(outcome.valid);
    assert_eq!(decision.channels()[0].result, ActivityResult::Active);
    assert_eq!(decision.channels()[0].first_evidence_frame, Some(2));
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn frozen_windowed_missing_sustain_across_chunks_is_ambiguous() {
    let (memory_plan, mut arena, ingress) = frozen_runtime(1, 100);
    ingress.try_push_interleaved(&[0.8; 9], 1).unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    lamb::activity::classify_frozen_epoch(
        &frozen,
        &windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false),
        &mut workspace,
        &mut decision,
    )
    .unwrap();
    assert_eq!(decision.channels()[0].result, ActivityResult::Ambiguous);
    assert_eq!(decision.channels()[0].first_evidence_frame, Some(0));
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn frozen_exact_zero_non_finite_gate_and_suppressed_nonzero_block_global_omission() {
    for samples in [
        [0.0, f32::NAN],
        [0.0, f32::INFINITY],
        [0.0, f32::NEG_INFINITY],
        [1.0, 0.0],
    ] {
        let (memory_plan, mut arena, ingress) = frozen_runtime(2, 100);
        ingress.try_push_interleaved(&samples, 2).unwrap();
        let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
        let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
        let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
        let mut policy = auto_policy(
            ActivityDetectorKind::ExactZero,
            &[ChannelExportMode::Never, ChannelExportMode::Always],
        );
        policy.whole_export_exact_zero_gate = true;
        lamb::activity::classify_frozen_epoch(&frozen, &policy, &mut workspace, &mut decision)
            .unwrap();
        assert_eq!(
            decision.channels()[1].disposition,
            ChannelDisposition::Retain
        );
        arena.release_frozen(&mut frozen, DEADLINE).unwrap();
        arena.shutdown(DEADLINE).unwrap();
    }
}

#[test]
fn frozen_windowed_missing_threshold_fallback_is_ambiguous_at_range_start() {
    let (memory_plan, mut arena, ingress) = frozen_runtime(1, 100);
    ingress.try_push_interleaved(&[0.0; 12], 1).unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    lamb::activity::classify_frozen_epoch(
        &frozen,
        &windowed_policy(&[ChannelExportMode::Auto], None, false),
        &mut workspace,
        &mut decision,
    )
    .unwrap();
    assert_eq!(decision.channels()[0].result, ActivityResult::Ambiguous);
    assert_eq!(decision.channels()[0].first_evidence_frame, Some(0));
    assert_eq!(
        decision.channels()[0].disposition,
        ChannelDisposition::Retain
    );
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn windowed_non_finite_overrides_complete_activity_and_retains_evidence() {
    let memory_plan = plan(1, 100);
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let mut samples = [0.8; 12];
    samples[11] = f32::NAN;
    let results = classify_samples(
        &samples,
        1,
        900,
        100,
        &windowed_policy(&[ChannelExportMode::Auto], Some(-3.0), false),
        &mut workspace,
    )
    .unwrap();
    assert_eq!(results[0].result, ActivityResult::Ambiguous);
    assert_eq!(results[0].disposition, ChannelDisposition::Retain);
    assert_eq!(results[0].first_evidence_frame, Some(900));
}

#[test]
fn exact_zero_global_gate_omits_only_an_entire_finite_zero_range() {
    let (memory_plan, mut arena, ingress) = frozen_runtime(2, 100);
    ingress
        .try_push_interleaved(&[0.0, -0.0, -0.0, 0.0], 2)
        .unwrap();
    let mut frozen = arena.freeze_since(None, DEADLINE).unwrap().unwrap();
    let mut workspace = DetectorWorkspace::new(&memory_plan).unwrap();
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    let mut policy = auto_policy(
        ActivityDetectorKind::ExactZero,
        &[ChannelExportMode::Never, ChannelExportMode::Always],
    );
    policy.whole_export_exact_zero_gate = true;
    lamb::activity::classify_frozen_epoch(&frozen, &policy, &mut workspace, &mut decision).unwrap();
    assert!(decision
        .channels()
        .iter()
        .all(|channel| channel.disposition == ChannelDisposition::Omit));
    arena.release_frozen(&mut frozen, DEADLINE).unwrap();
    arena.shutdown(DEADLINE).unwrap();
}

#[test]
fn invalid_evidence_never_partially_mutates_or_validates_a_frozen_decision() {
    let memory_plan = plan(1, 100);
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    let invalid = [FrozenChannelDecision::retained(
        ChannelExportMode::Auto,
        ActivityResult::Active,
        Some(99),
    )];
    assert!(decision.finalize(100..500, &invalid, true, false).is_err());
    assert!(!decision.valid());
    assert_eq!(decision.export_range(), 0..0);
    assert_eq!(decision.channels()[0].mode, ChannelExportMode::Never);
    assert_eq!(decision.channels()[0].disposition, ChannelDisposition::Omit);
}

#[test]
fn contradictory_dispositions_are_rejected_without_mutating_frozen_decision() {
    let memory_plan = plan(1, 100);
    let default = [FrozenChannelDecision {
        mode: ChannelExportMode::Never,
        result: ActivityResult::Inactive,
        disposition: ChannelDisposition::Omit,
        first_evidence_frame: None,
    }];
    let contradictory = [
        FrozenChannelDecision {
            mode: ChannelExportMode::Never,
            result: ActivityResult::Active,
            disposition: ChannelDisposition::Retain,
            first_evidence_frame: None,
        },
        FrozenChannelDecision {
            mode: ChannelExportMode::Always,
            result: ActivityResult::Inactive,
            disposition: ChannelDisposition::Omit,
            first_evidence_frame: None,
        },
        FrozenChannelDecision {
            mode: ChannelExportMode::Auto,
            result: ActivityResult::Inactive,
            disposition: ChannelDisposition::Retain,
            first_evidence_frame: None,
        },
        FrozenChannelDecision {
            mode: ChannelExportMode::Auto,
            result: ActivityResult::Active,
            disposition: ChannelDisposition::Omit,
            first_evidence_frame: None,
        },
    ];

    for invalid in contradictory {
        let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
        assert!(decision
            .finalize(100..500, &[invalid], true, false)
            .is_err());
        assert!(!decision.valid());
        assert_eq!(decision.export_range(), 0..0);
        assert_eq!(decision.channels(), &default);
    }
}

#[test]
fn frozen_decision_is_immutable_and_crops_common_preroll_without_trimming_end() {
    let memory_plan = plan(2, 100);
    let mut decision = FrozenExportDecision::new(&memory_plan).unwrap();
    let channels = [
        FrozenChannelDecision::retained(ChannelExportMode::Auto, ActivityResult::Active, Some(350)),
        FrozenChannelDecision::retained(
            ChannelExportMode::Always,
            ActivityResult::Active,
            Some(450),
        ),
    ];
    decision.finalize(100..500, &channels, true, false).unwrap();
    assert!(decision.valid());
    assert_eq!(decision.export_range(), 150..500);
    assert!(decision.finalize(100..500, &channels, true, false).is_err());
    let mut compatibility = FrozenExportDecision::new(&memory_plan).unwrap();
    compatibility
        .finalize(100..500, &channels, false, false)
        .unwrap();
    assert_eq!(compatibility.export_range(), 100..500);

    let one = plan(1, 100);
    let always_inactive = [FrozenChannelDecision::retained(
        ChannelExportMode::Always,
        ActivityResult::Inactive,
        None,
    )];
    let mut no_evidence = FrozenExportDecision::new(&one).unwrap();
    no_evidence
        .finalize(100..500, &always_inactive, true, false)
        .unwrap();
    assert_eq!(no_evidence.export_range(), 100..500);
    let clamp = [FrozenChannelDecision::retained(
        ChannelExportMode::Auto,
        ActivityResult::Active,
        Some(150),
    )];
    let mut clamped = FrozenExportDecision::new(&one).unwrap();
    clamped.finalize(100..500, &clamp, true, false).unwrap();
    assert_eq!(clamped.export_range(), 100..500);
}
