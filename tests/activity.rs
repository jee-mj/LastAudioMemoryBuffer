use lamb::activity::{
    ActivityDetectorKind, ChannelExportMode, SilencePolicyPreset, ThresholdSource,
};

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
