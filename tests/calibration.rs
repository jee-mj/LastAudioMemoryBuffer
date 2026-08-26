use lamb::activity::ThresholdSource;
use lamb::activity::WINDOWED_RMS_PEAK_DETECTOR_VERSION;
use lamb::app_config::{ActivityThresholdConfig, AppConfig};
use lamb::calibration::{
    commit_prepared_generation_with_hooks, derive_calibrated_threshold, save_config_atomic,
    save_config_atomic_with_hook_and_name_source, state_root_from_env, CalibrationArtifactStatus,
    CalibrationMetadata, CalibrationStore, CalibrationValidity, CleanupCheckpoint,
    DurabilityCheckpoint, OldGenerationCleanup, PreparedCalibrationGeneration, RecordedGeneration,
    StaleReason,
};
use lamb::calibration::{
    ConfiguredDeviceSelector, ConfiguredInputIdentity, InputBackend, LiveDeviceKeyKind,
    ResolvedLiveInputIdentity,
};
use lamb::capture_arena::{
    CalibrationCaptureRequest, CaptureArena, CaptureIngress, CaptureRuntimeConfig,
};
use lamb::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
use lamb::sample_ring::{RingConfig, SampleFormat};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::os::unix::fs::{symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const DEADLINE: Duration = Duration::from_secs(2);
const MAXIMUM_FRAMES: u64 = 1_000;

fn calibration_store(root: impl AsRef<std::path::Path>) -> CalibrationStore {
    CalibrationStore::new(root, MAXIMUM_FRAMES).unwrap()
}

#[test]
fn calibration_store_rejects_zero_and_unrepresentable_riff_frame_limits() {
    let temp = tempfile::tempdir().unwrap();
    assert!(CalibrationStore::new(temp.path(), 0).is_err());

    let largest_riff_f32le_frames = u64::from(u32::MAX - 36) / 4;
    assert!(CalibrationStore::new(temp.path(), largest_riff_f32le_frames + 1).is_err());
    let store = CalibrationStore::new(temp.path(), largest_riff_f32le_frames).unwrap();
    assert_eq!(store.maximum_frames(), largest_riff_f32le_frames);
}

#[test]
fn tiny_positive_rms_derives_the_floored_threshold() {
    let mut stats = [1e-20];

    assert_eq!(derive_calibrated_threshold(&mut stats).unwrap(), -110.0);
}

#[test]
fn configured_identity_v2_tags_selector_kinds_and_excludes_live_device_state() {
    let auto = ConfiguredInputIdentity::new(
        InputBackend::PipeWire,
        ConfiguredDeviceSelector::PipeWireAuto,
        " mic ",
        " capture_MIC ",
    )
    .unwrap();
    let target = ConfiguredInputIdentity::new(
        InputBackend::PipeWire,
        ConfiguredDeviceSelector::PipeWireTarget("auto".to_string()),
        "mic",
        "capture_MIC",
    )
    .unwrap();
    let same_auto = ConfiguredInputIdentity::new(
        InputBackend::PipeWire,
        ConfiguredDeviceSelector::PipeWireAuto,
        "mic",
        "capture_MIC",
    )
    .unwrap();

    assert_eq!(auto.input_id(), same_auto.input_id());
    assert_ne!(auto.input_id(), target.input_id());
}

#[test]
fn resolved_live_identity_compares_key_kind_and_value() {
    let serial = ResolvedLiveInputIdentity::new(
        InputBackend::PipeWire,
        LiveDeviceKeyKind::HardwareSerial,
        "device-42",
        "capture_MIC",
    )
    .unwrap();
    let path = ResolvedLiveInputIdentity::new(
        InputBackend::PipeWire,
        LiveDeviceKeyKind::ObjectPath,
        "device-42",
        "capture_MIC",
    )
    .unwrap();

    assert_ne!(serial, path);
}

#[test]
fn jack_configured_and_live_sources_require_matching_complete_client_ports() {
    for source in ["client", "client:", ":port", "other:port"] {
        assert!(ConfiguredInputIdentity::new(
            InputBackend::Jack,
            ConfiguredDeviceSelector::JackSourceClient("client".into()),
            "mic",
            source,
        )
        .is_err());
        assert!(ResolvedLiveInputIdentity::new(
            InputBackend::Jack,
            LiveDeviceKeyKind::JackSourceClient,
            "client",
            source,
        )
        .is_err());
    }

    assert!(ConfiguredInputIdentity::new(
        InputBackend::Jack,
        ConfiguredDeviceSelector::JackSourceClient("client".into()),
        "mic",
        "client:port",
    )
    .is_ok());
    assert!(ResolvedLiveInputIdentity::new(
        InputBackend::Jack,
        LiveDeviceKeyKind::JackSourceClient,
        "client",
        "client:port",
    )
    .is_ok());
}

#[test]
fn jack_configured_and_live_sources_reject_extra_separators() {
    for source in ["client:port:extra", "client::port"] {
        assert!(ConfiguredInputIdentity::new(
            InputBackend::Jack,
            ConfiguredDeviceSelector::JackSourceClient("client".into()),
            "mic",
            source,
        )
        .is_err());
        assert!(ResolvedLiveInputIdentity::new(
            InputBackend::Jack,
            LiveDeviceKeyKind::JackSourceClient,
            "client",
            source,
        )
        .is_err());
    }
}

fn runtime() -> (CaptureArena, CaptureIngress) {
    let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
        retention_frames: 100,
        channels: 2,
        sample_rate: 1_000,
        sample_format: SampleFormat::F32Le,
        chunk_frames: 10,
        max_active_snapshots: 1,
        sample_bytes: 4,
        split_when_over_bytes: 1_000_000,
        control_queue_capacity: 2,
        worker_stack_bytes: 64 * 1024,
        capture_queue_slots: 16,
        capture_slot_frames: 10,
        capture_worker_stack_bytes: 256 * 1024,
        io_buffer_bytes_per_channel: 4 * 1024,
        maximum_path_bytes: 512,
        maximum_calibration_seconds: 1,
        headroom: 1.0,
    })
    .unwrap();
    let (arena, ingress) = CaptureArena::new(
        &plan,
        CaptureRuntimeConfig {
            ring: RingConfig {
                channels: 2,
                sample_rate: 1_000,
                format: SampleFormat::F32Le,
                chunk_frames: 10,
                chunk_count: 10,
                max_active_snapshots: 1,
            },
            queue_slots: 16,
            slot_frames: 10,
            sample_bytes: 4,
            worker_stack_bytes: 256 * 1024,
        },
    )
    .unwrap();
    (arena, ingress)
}

#[test]
fn calibration_request_validation_is_bounded_by_the_startup_plan() {
    let (mut arena, _ingress) = runtime();
    for request in [
        CalibrationCaptureRequest {
            channel: 2,
            frames: 1,
        },
        CalibrationCaptureRequest {
            channel: 1,
            frames: 0,
        },
        CalibrationCaptureRequest {
            channel: 1,
            frames: 1_001,
        },
    ] {
        assert!(arena.calibrate_channel(request, DEADLINE).is_err());
    }
    arena.shutdown(DEADLINE).unwrap();
}

fn identity() -> ConfiguredInputIdentity {
    ConfiguredInputIdentity::new(
        InputBackend::PipeWire,
        ConfiguredDeviceSelector::PipeWireTarget("alsa_input.usb".into()),
        "aux3",
        "capture_AUX3",
    )
    .unwrap()
}

fn live_identity() -> ResolvedLiveInputIdentity {
    ResolvedLiveInputIdentity::new(
        InputBackend::PipeWire,
        LiveDeviceKeyKind::HardwareSerial,
        "device-42",
        "capture_AUX3",
    )
    .unwrap()
}

fn identity_with_forged_input_id(input_id: &str) -> ConfiguredInputIdentity {
    let mut value = serde_json::to_value(identity()).unwrap();
    value["input_id"] = serde_json::Value::String(input_id.to_owned());
    serde_json::from_value(value).unwrap()
}

fn live_identity_with_forged_field(
    field: &str,
    value: serde_json::Value,
) -> ResolvedLiveInputIdentity {
    let mut identity = serde_json::to_value(live_identity()).unwrap();
    identity[field] = value;
    serde_json::from_value(identity).unwrap()
}

#[test]
fn prepared_metadata_v2_round_trips_configured_and_resolved_live_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let configured = identity();
    let live = live_identity();

    let prepared = store
        .prepare(
            &configured,
            &live,
            "generation-v2",
            &[0.0],
            48_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1_000,
        )
        .unwrap();
    let encoded = std::fs::read(prepared.metadata_path()).unwrap();
    let decoded: CalibrationMetadata = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded.version, 2);
    assert_eq!(decoded.configured_input.as_ref(), Some(&configured));
    assert_eq!(decoded.resolved_live_input.as_ref(), Some(&live));
    assert_eq!(
        serde_json::from_slice::<CalibrationMetadata>(&serde_json::to_vec(&decoded).unwrap())
            .unwrap(),
        decoded
    );
}

#[test]
fn forged_live_identity_is_rejected_before_filesystem_access() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing/state");
    let store = calibration_store(&root);
    for forged in [
        live_identity_with_forged_field("key_value", serde_json::json!(" device-42 ")),
        live_identity_with_forged_field("resolved_source", serde_json::json!(" capture_AUX3 ")),
        live_identity_with_forged_field("backend", serde_json::json!("jack")),
    ] {
        assert!(matches!(
            store.prepare(
                &identity(),
                &forged,
                "forged-live",
                &[0.0],
                48_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                1_000,
            ),
            Err(lamb::error::LambError::Validation(_))
        ));
    }
    assert!(!root.exists());
}

#[test]
fn forged_configured_fields_and_selector_coherence_are_rejected_before_filesystem_access() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing/state");
    let store = calibration_store(&root);
    let mut forged_values = Vec::new();
    for (field, value) in [
        ("name", serde_json::json!(" aux3 ")),
        ("source", serde_json::json!(" capture_AUX3 ")),
        ("backend", serde_json::json!("jack")),
    ] {
        let mut forged = serde_json::to_value(identity()).unwrap();
        forged[field] = value;
        forged_values.push(serde_json::from_value(forged).unwrap());
    }
    let mut forged = serde_json::to_value(identity()).unwrap();
    forged["selector"] = serde_json::json!({
        "kind": "pipe-wire-target",
        "value": " alsa_input.usb "
    });
    forged_values.push(serde_json::from_value(forged).unwrap());

    for forged in forged_values {
        assert!(matches!(
            store.prepare(
                &forged,
                &live_identity(),
                "forged-configured",
                &[0.0],
                48_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                1_000,
            ),
            Err(lamb::error::LambError::Validation(_))
        ));
    }
    assert!(!root.exists());
}

#[test]
fn manual_validation_needs_no_live_identity_or_artifact_state() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path().join("missing"));
    let input = identity();
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: -40.0,
        threshold_source: ThresholdSource::Manual,
        updated_at_unix_seconds: 1,
        input_id: input.input_id().into(),
        calibration_id: None,
    };

    assert_eq!(
        store
            .validate(&threshold, &input, None, 48_000, u64::MAX)
            .unwrap(),
        CalibrationValidity::Valid
    );

    let forged_live = live_identity_with_forged_field("key_value", serde_json::json!(" forged "));
    assert_eq!(
        store
            .validate(&threshold, &input, Some(&forged_live), 48_000, u64::MAX,)
            .unwrap(),
        CalibrationValidity::Valid
    );

    let mismatched = ActivityThresholdConfig {
        input_id: "0".repeat(64),
        threshold_source: ThresholdSource::Calibrated,
        calibration_id: Some("missing".into()),
        ..threshold
    };
    assert_eq!(
        store
            .validate(&mismatched, &input, Some(&forged_live), 48_000, u64::MAX,)
            .unwrap(),
        CalibrationValidity::Stale(StaleReason::InputMismatch)
    );
}

#[test]
fn offline_inspection_and_cleanup_are_available_without_an_active_session() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let live = live_identity();
    let mut prepared = store
        .prepare(
            &input,
            &live,
            "offline",
            &[0.0],
            48_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1_000,
        )
        .unwrap();
    prepared.mark_authoritative();
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: -110.0,
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 1_000,
        input_id: input.input_id().into(),
        calibration_id: Some("offline".into()),
    };

    let inspection = store.inspect_offline(&threshold, &input, 1_000).unwrap();
    assert_eq!(inspection.status, CalibrationArtifactStatus::Complete);
    assert_eq!(inspection.metadata.as_ref().map(|m| m.version), Some(2));

    std::fs::remove_file(prepared.sample_path()).unwrap();
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::MissingSample)
    );
    assert!(store.cleanup_offline(&BTreeSet::new()).unwrap().is_empty());
    assert!(!prepared.path().exists());
}

fn assert_forged_identity_rejected_without_side_effects(input_id: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state/nested");
    let outside = temp.path().join("outside");
    let marker = outside.join("marker");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(&marker, b"outside remains exact").unwrap();
    let outside_before = std::fs::metadata(&outside).unwrap();
    let marker_before = std::fs::metadata(&marker).unwrap();

    let input = identity_with_forged_input_id(input_id);
    let store = calibration_store(&root);
    let preparation = store.prepare(
        &input,
        &live_identity(),
        "generation-a",
        &[0.0],
        48_000,
        derived_threshold(&[0.0]),
        &mut [0.0],
        &[0.0],
        1_000,
    );
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: -110.0,
        threshold_source: ThresholdSource::Manual,
        updated_at_unix_seconds: 1_000,
        input_id: input.input_id().to_owned(),
        calibration_id: None,
    };
    let validation = store.validate(&threshold, &input, Some(&live_identity()), 48_000, 1_000);

    assert!(matches!(
        preparation,
        Err(lamb::error::LambError::Validation(_))
    ));
    assert!(matches!(
        validation,
        Err(lamb::error::LambError::Validation(_))
    ));
    assert!(!root.exists(), "forged identity must not create state root");
    assert_eq!(std::fs::read(&marker).unwrap(), b"outside remains exact");
    let outside_after = std::fs::metadata(&outside).unwrap();
    let marker_after = std::fs::metadata(&marker).unwrap();
    assert_eq!(
        (
            outside_after.dev(),
            outside_after.ino(),
            outside_after.mode()
        ),
        (
            outside_before.dev(),
            outside_before.ino(),
            outside_before.mode()
        )
    );
    assert_eq!(
        (
            outside_after.mtime(),
            outside_after.mtime_nsec(),
            outside_after.ctime(),
            outside_after.ctime_nsec(),
        ),
        (
            outside_before.mtime(),
            outside_before.mtime_nsec(),
            outside_before.ctime(),
            outside_before.ctime_nsec(),
        )
    );
    assert_eq!(
        (
            marker_after.dev(),
            marker_after.ino(),
            marker_after.mode(),
            marker_after.len(),
            marker_after.mtime(),
            marker_after.mtime_nsec(),
            marker_after.ctime(),
            marker_after.ctime_nsec(),
        ),
        (
            marker_before.dev(),
            marker_before.ino(),
            marker_before.mode(),
            marker_before.len(),
            marker_before.mtime(),
            marker_before.mtime_nsec(),
            marker_before.ctime(),
            marker_before.ctime_nsec(),
        )
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        1,
        "forged identity must not create an outside child"
    );
}

#[test]
fn forged_short_nonhex_input_identity_is_rejected_before_filesystem_access() {
    assert_forged_identity_rejected_without_side_effects("not-hex");
}

#[test]
fn forged_parent_traversal_input_identity_is_rejected_before_filesystem_access() {
    assert_forged_identity_rejected_without_side_effects("../../outside");
}

#[test]
fn forged_absolute_input_identity_is_rejected_before_filesystem_access() {
    let temp = tempfile::tempdir().unwrap();
    let absolute = temp.path().join("absolute-outside");
    assert_forged_identity_rejected_without_side_effects(absolute.to_str().unwrap());
    assert!(!absolute.exists());
}

fn derived_threshold(rms: &[f32]) -> f32 {
    derive_calibrated_threshold(&mut rms.to_vec()).unwrap()
}

fn valid_candidate(prepared: &PreparedCalibrationGeneration) -> AppConfig {
    let mut candidate: AppConfig = toml::from_str(
        r#"
[daemon]
startMode = "manual"
activeProfile = "scarlett"

[profiles.scarlett]
backend = "pipewire"

[profiles.scarlett.pipewire]
target = "studio-input"
capturePorts = [{ source = "capture_AUX3", name = "aux3" }]

[profiles.scarlett.buffer]
seconds = 10

[profiles.scarlett.export]
outputDir = "/tmp/lamb-profile"
mode = "per-channel"
format = "wav"
"#,
    )
    .unwrap();
    candidate
        .profiles
        .get_mut("scarlett")
        .unwrap()
        .channels
        .insert(
            "aux3".into(),
            lamb::app_config::ProfileChannelConfig {
                activity: Some(ActivityThresholdConfig {
                    threshold_dbfs: f64::from(prepared.metadata().threshold_dbfs),
                    threshold_source: ThresholdSource::Calibrated,
                    updated_at_unix_seconds: prepared.metadata().created_at_unix_seconds,
                    input_id: prepared.input_id().to_owned(),
                    calibration_id: Some(prepared.calibration_id().to_owned()),
                }),
            },
        );
    candidate
}

fn generation_bytes(prepared: &PreparedCalibrationGeneration) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(prepared.sample_path()).unwrap(),
        std::fs::read(prepared.metadata_path()).unwrap(),
    )
}

fn generation_fixture() -> (
    tempfile::TempDir,
    CalibrationStore,
    ConfiguredInputIdentity,
    PreparedCalibrationGeneration,
    ActivityThresholdConfig,
) {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let prepared = store
        .prepare(
            &input,
            &live_identity(),
            "generation-a",
            &[0.0, -0.5, 1.0],
            48_000,
            derived_threshold(&[0.01, 0.02]),
            &mut [0.01, 0.02],
            &[0.1, 0.2],
            1_000,
        )
        .unwrap();
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: f64::from(derived_threshold(&[0.01, 0.02])),
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 1_000,
        input_id: input.input_id().to_string(),
        calibration_id: Some("generation-a".into()),
    };
    (temp, store, input, prepared, threshold)
}

#[test]
fn generated_generations_are_unique_and_explicit_duplicates_never_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let threshold = derived_threshold(&[0.01]);
    let first_generated = store
        .prepare_generated(
            &input,
            &live_identity(),
            &[0.25],
            48_000,
            threshold,
            &mut [0.01],
            &[0.02],
            1_000,
        )
        .unwrap();
    let second_generated = store
        .prepare_generated(
            &input,
            &live_identity(),
            &[0.5],
            48_000,
            threshold,
            &mut [0.01],
            &[0.02],
            1_000,
        )
        .unwrap();
    assert_ne!(
        first_generated.calibration_id(),
        second_generated.calibration_id()
    );
    assert!(first_generated.calibration_id().len() <= 128);

    let explicit = store
        .prepare(
            &input,
            &live_identity(),
            "fixed-generation",
            &[0.25],
            48_000,
            threshold,
            &mut [0.01],
            &[0.02],
            1_000,
        )
        .unwrap();
    let sample_before = std::fs::read(explicit.sample_path()).unwrap();
    let metadata_before = std::fs::read(explicit.metadata_path()).unwrap();
    assert!(store
        .prepare(
            &input,
            &live_identity(),
            "fixed-generation",
            &[0.75],
            48_000,
            threshold,
            &mut [0.01],
            &[0.02],
            2_000,
        )
        .is_err());
    assert_eq!(
        std::fs::read(explicit.sample_path()).unwrap(),
        sample_before
    );
    assert_eq!(
        std::fs::read(explicit.metadata_path()).unwrap(),
        metadata_before
    );
}

#[test]
fn lease_preparation_rejects_unusable_capture_and_persists_capture_geometry() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let (mut arena, ingress) = runtime();

    let mut short = thread::scope(|scope| {
        let capture = scope.spawn(|| {
            arena.calibrate_channel(
                CalibrationCaptureRequest {
                    channel: 0,
                    frames: 3,
                },
                DEADLINE,
            )
        });
        let block = [0.01f32; 20];
        while !capture.is_finished() {
            let _ = ingress.try_push_interleaved(&block, 2);
            let _ = arena.status(DEADLINE).unwrap();
        }
        capture.join().unwrap().unwrap()
    });
    assert!(!short.metadata().usable);
    assert_eq!(short.complete_windows(), 0);
    assert!(store
        .prepare_lease(&input, &live_identity(), "short", &mut short, -110.0, 1)
        .is_err());
    assert!(!temp.path().join(input.input_id()).join("short").exists());
    drop(short);

    let mut usable = thread::scope(|scope| {
        let capture = scope.spawn(|| {
            arena.calibrate_channel(
                CalibrationCaptureRequest {
                    channel: 0,
                    frames: 25,
                },
                DEADLINE,
            )
        });
        let block = [0.01f32; 20];
        while !capture.is_finished() {
            let _ = ingress.try_push_interleaved(&block, 2);
            let _ = arena.status(DEADLINE).unwrap();
        }
        capture.join().unwrap().unwrap()
    });
    assert!(usable.metadata().usable);
    assert_eq!(usable.partial_final_frames(), 5);
    let threshold = derive_calibrated_threshold(usable.rms_mut()).unwrap();
    let mut checkpoints = Vec::new();
    let prepared = store
        .prepare_generated_lease_with_hook(
            &input,
            &live_identity(),
            &mut usable,
            threshold,
            2,
            &mut |checkpoint| {
                checkpoints.push(checkpoint);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(prepared.metadata().partial_final_frames, 5);
    assert_eq!(prepared.metadata().dropped_frames, 0);
    assert!(checkpoints.contains(&DurabilityCheckpoint::SampleWritten));
    assert!(checkpoints.contains(&DurabilityCheckpoint::MetadataWritten));
    assert!(checkpoints.contains(&DurabilityCheckpoint::GenerationDirectorySynced));
    assert!(checkpoints.contains(&DurabilityCheckpoint::InputDirectorySynced));
    assert!(checkpoints.contains(&DurabilityCheckpoint::RootDirectorySynced));
    drop(prepared);
    drop(usable);
    arena.shutdown(DEADLINE).unwrap();
}

fn rewrite_metadata(
    prepared: &PreparedCalibrationGeneration,
    change: impl FnOnce(&mut CalibrationMetadata),
) {
    let mut metadata: CalibrationMetadata =
        serde_json::from_slice(&std::fs::read(prepared.metadata_path()).unwrap()).unwrap();
    change(&mut metadata);
    std::fs::write(
        prepared.metadata_path(),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

fn rewrite_metadata_value(
    prepared: &PreparedCalibrationGeneration,
    change: impl FnOnce(&mut serde_json::Value),
) {
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(prepared.metadata_path()).unwrap()).unwrap();
    change(&mut metadata);
    std::fs::write(
        prepared.metadata_path(),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

fn convert_to_legacy_v1(metadata: &mut serde_json::Value) {
    let fields = ["pipewire", "alsa_input.usb", "aux3", "capture_AUX3"];
    let mut hasher = Sha256::new();
    hasher.update(b"lamb/stable-input-identity/v1\0");
    for field in fields {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    let input_id = format!("{:x}", hasher.finalize());
    metadata["version"] = serde_json::json!(1);
    metadata["input"] = serde_json::json!({
        "backend": fields[0],
        "device": fields[1],
        "name": fields[2],
        "source": fields[3],
        "input_id": input_id,
    });
    metadata["input_id"] = serde_json::json!(input_id);
    metadata
        .as_object_mut()
        .unwrap()
        .remove("resolved_live_input");
}

#[test]
fn calibrated_validation_distinguishes_missing_and_mismatched_live_identity() {
    let (_temp, store, input, prepared, threshold) = generation_fixture();
    rewrite_metadata_value(&prepared, convert_to_legacy_v1);
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::MissingLiveIdentity,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    rewrite_metadata_value(&prepared, |metadata| {
        metadata
            .as_object_mut()
            .unwrap()
            .remove("resolved_live_input");
    });
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::MissingLiveIdentity,
    );
    assert_eq!(
        store
            .validate(&threshold, &input, None, 48_000, 1_000)
            .unwrap(),
        CalibrationValidity::Stale(StaleReason::MissingLiveIdentity)
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    let other = ConfiguredInputIdentity::new(
        InputBackend::PipeWire,
        ConfiguredDeviceSelector::PipeWireTarget("other".into()),
        "aux3",
        "capture_AUX3",
    )
    .unwrap();
    rewrite_metadata_value(&prepared, |metadata| {
        metadata["input"] = serde_json::to_value(&other).unwrap();
        metadata["input_id"] = serde_json::json!(other.input_id());
        metadata
            .as_object_mut()
            .unwrap()
            .remove("resolved_live_input");
    });
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::InputMismatch,
    );

    for current_live in [
        ResolvedLiveInputIdentity::new(
            InputBackend::PipeWire,
            LiveDeviceKeyKind::ObjectPath,
            "device-42",
            "capture_AUX3",
        )
        .unwrap(),
        ResolvedLiveInputIdentity::new(
            InputBackend::PipeWire,
            LiveDeviceKeyKind::HardwareSerial,
            "device-43",
            "capture_AUX3",
        )
        .unwrap(),
        ResolvedLiveInputIdentity::new(
            InputBackend::PipeWire,
            LiveDeviceKeyKind::HardwareSerial,
            "device-42",
            "capture_OTHER",
        )
        .unwrap(),
    ] {
        let (_temp, store, input, _prepared, threshold) = generation_fixture();
        assert_eq!(
            store
                .validate(&threshold, &input, Some(&current_live), 48_000, 1_000)
                .unwrap(),
            CalibrationValidity::Stale(StaleReason::LiveIdentityMismatch)
        );
    }
}

#[test]
fn offline_inspection_distinguishes_persisted_artifact_failures() {
    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::remove_file(prepared.metadata_path()).unwrap();
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::MissingMetadata)
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::write(prepared.metadata_path(), b"{").unwrap();
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::CorruptMetadata)
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::write(prepared.sample_path(), b"not a wav").unwrap();
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::CorruptSample)
    );

    let (_temp, store, input, _prepared, threshold) = generation_fixture();
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000 + 30 * 24 * 60 * 60 + 1)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::Expired)
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    rewrite_metadata_value(&prepared, convert_to_legacy_v1);
    assert_eq!(
        store
            .inspect_offline(&threshold, &input, 1_000)
            .unwrap()
            .status,
        CalibrationArtifactStatus::Stale(StaleReason::MissingLiveIdentity)
    );
}

fn assert_stale(
    store: &CalibrationStore,
    threshold: &ActivityThresholdConfig,
    input: &ConfiguredInputIdentity,
    sample_rate: u32,
    now: u64,
    reason: StaleReason,
) {
    assert_eq!(
        store
            .validate(threshold, input, Some(&live_identity()), sample_rate, now)
            .unwrap(),
        CalibrationValidity::Stale(reason)
    );
}

#[test]
fn calibration_identity_is_stable_length_delimited_and_order_independent() {
    let original = identity();
    assert_eq!(original.input_id().len(), 64);
    assert_eq!(original, identity());
    for changed in [
        ConfiguredInputIdentity::new(
            InputBackend::Jack,
            ConfiguredDeviceSelector::JackSourceClient("client".into()),
            "aux3",
            "client:port",
        )
        .unwrap(),
        ConfiguredInputIdentity::new(
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget("other".into()),
            "aux3",
            "capture_AUX3",
        )
        .unwrap(),
        ConfiguredInputIdentity::new(
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget("alsa_input.usb".into()),
            "aux4",
            "capture_AUX3",
        )
        .unwrap(),
        ConfiguredInputIdentity::new(
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget("alsa_input.usb".into()),
            "aux3",
            "other",
        )
        .unwrap(),
        ConfiguredInputIdentity::new(
            InputBackend::PipeWire,
            ConfiguredDeviceSelector::PipeWireTarget("alsa_input.usb:a".into()),
            "aux3",
            "capture_AUX3",
        )
        .unwrap(),
    ] {
        assert_ne!(original.input_id(), changed.input_id());
    }
}

#[test]
fn calibration_state_root_requires_absolute_xdg_or_home() {
    assert_eq!(
        state_root_from_env(Some(PathBuf::from("/state")), None).unwrap(),
        PathBuf::from("/state/lamb/calibration")
    );
    assert_eq!(
        state_root_from_env(None, Some(PathBuf::from("/home/a"))).unwrap(),
        PathBuf::from("/home/a/.local/state/lamb/calibration")
    );
    assert!(state_root_from_env(Some(PathBuf::from("relative")), None).is_err());
    assert!(state_root_from_env(None, None).is_err());
}

#[test]
fn threshold_is_nearest_rank_with_zero_floor_and_rejects_invalid_stats() {
    let mut values = [0.04, 0.01, 0.10, 0.03, 0.02];
    let threshold = derive_calibrated_threshold(&mut values).unwrap();
    assert!((threshold - (-10.0)).abs() < 0.001);
    assert_eq!(values, [0.01, 0.02, 0.03, 0.04, 0.10]);
    let mut zeros = [0.0, 0.0];
    assert_eq!(derive_calibrated_threshold(&mut zeros).unwrap(), -110.0);
    for mut invalid in [
        vec![],
        vec![-0.1],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![2.0],
    ] {
        assert!(derive_calibrated_threshold(&mut invalid).is_err());
    }
}

#[test]
fn prepared_generation_round_trips_exact_float_wav_and_validates_staleness() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut rms = [0.01, 0.02];
    let prepared = store
        .prepare(
            &input,
            &live_identity(),
            "generation-a",
            &[0.0, -0.5, 1.0],
            48_000,
            derived_threshold(&[0.01, 0.02]),
            &mut rms,
            &[0.1, 0.2],
            1_000,
        )
        .unwrap();
    let wav = std::fs::read(prepared.sample_path()).unwrap();
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 3);
    assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 32);
    assert_eq!(&wav[44..48], &0.0f32.to_le_bytes());
    assert_eq!(&wav[48..52], &(-0.5f32).to_le_bytes());
    let metadata: CalibrationMetadata =
        serde_json::from_slice(&std::fs::read(prepared.metadata_path()).unwrap()).unwrap();
    assert_eq!(
        metadata.detector_version,
        WINDOWED_RMS_PEAK_DETECTOR_VERSION
    );
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: f64::from(derived_threshold(&[0.01, 0.02])),
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 1_000,
        input_id: input.input_id().to_string(),
        calibration_id: Some("generation-a".into()),
    };
    assert_eq!(
        store
            .validate(
                &threshold,
                &input,
                Some(&live_identity()),
                48_000,
                1_000 + 30 * 24 * 60 * 60,
            )
            .unwrap(),
        CalibrationValidity::Valid
    );
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000 + 30 * 24 * 60 * 60 + 1,
        StaleReason::Expired,
    );
}

#[test]
fn every_calibration_staleness_reason_is_structured_and_manual_does_not_age() {
    let (_temp, store, input, _prepared, mut threshold) = generation_fixture();
    threshold.calibration_id = None;
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::MissingCalibrationId,
    );

    let temp = tempfile::tempdir().unwrap();
    let empty_store = calibration_store(temp.path());
    let calibrated = ActivityThresholdConfig {
        threshold_dbfs: -30.0,
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 1_000,
        input_id: input.input_id().to_string(),
        calibration_id: Some("absent-generation".into()),
    };
    assert_stale(
        &empty_store,
        &calibrated,
        &input,
        48_000,
        1_000,
        StaleReason::MissingState,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::remove_file(prepared.metadata_path()).unwrap();
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::MissingMetadata,
    );

    for corrupt in ["invalid-json", "oversized", "non-regular", "symlink"] {
        let (_temp, store, input, prepared, threshold) = generation_fixture();
        match corrupt {
            "invalid-json" => std::fs::write(prepared.metadata_path(), b"{").unwrap(),
            "oversized" => {
                std::fs::write(prepared.metadata_path(), vec![b' '; 64 * 1024 + 1]).unwrap()
            }
            "non-regular" => {
                std::fs::remove_file(prepared.metadata_path()).unwrap();
                std::fs::create_dir(prepared.metadata_path()).unwrap();
            }
            "symlink" => {
                use std::os::unix::fs::symlink;
                let target = prepared.path().join("metadata-target.json");
                std::fs::rename(prepared.metadata_path(), &target).unwrap();
                symlink(&target, prepared.metadata_path()).unwrap();
            }
            _ => unreachable!(),
        }
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptMetadata,
        );
    }

    for (name, change, reason) in [
        (
            "version",
            (|m: &mut CalibrationMetadata| m.version = 3) as fn(&mut CalibrationMetadata),
            StaleReason::CorruptMetadata,
        ),
        (
            "generation",
            |m: &mut CalibrationMetadata| m.calibration_id = "other-generation".into(),
            StaleReason::GenerationMismatch,
        ),
        (
            "detector",
            |m: &mut CalibrationMetadata| m.detector_version = "detector-v2".into(),
            StaleReason::DetectorMismatch,
        ),
    ] {
        let (_temp, store, input, prepared, threshold) = generation_fixture();
        rewrite_metadata(&prepared, change);
        assert_stale(&store, &threshold, &input, 48_000, 1_000, reason);
        assert!(!name.is_empty());
    }

    let (_temp, store, input, _prepared, threshold) = generation_fixture();
    assert_stale(
        &store,
        &threshold,
        &input,
        44_100,
        1_000,
        StaleReason::SampleRateMismatch,
    );

    let (_temp, store, input, _prepared, mut threshold) = generation_fixture();
    threshold.input_id = "0".repeat(64);
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::InputMismatch,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    let other = ConfiguredInputIdentity::new(
        InputBackend::Jack,
        ConfiguredDeviceSelector::JackSourceClient("other".into()),
        "mic",
        "other:capture_1",
    )
    .unwrap();
    rewrite_metadata(&prepared, |m| {
        m.configured_input = Some(other.clone());
        m.input_id = other.input_id().to_string();
    });
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::InputMismatch,
    );

    let (_temp, store, input, prepared, mut threshold) = generation_fixture();
    rewrite_metadata(&prepared, |m| m.created_at_unix_seconds = 1_001);
    threshold.updated_at_unix_seconds = 1_001;
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::FutureTimestamp,
    );

    let (_temp, store, input, _prepared, mut threshold) = generation_fixture();
    threshold.updated_at_unix_seconds = 999;
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::IncoherentTimestamp,
    );

    let (_temp, store, input, _prepared, mut threshold) = generation_fixture();
    threshold.threshold_dbfs = -31.0;
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::ThresholdMismatch,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::remove_file(prepared.sample_path()).unwrap();
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::MissingSample,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    std::fs::write(prepared.sample_path(), b"not a wav").unwrap();
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::CorruptSample,
    );

    let manual = ActivityThresholdConfig {
        threshold_dbfs: -40.0,
        threshold_source: ThresholdSource::Manual,
        updated_at_unix_seconds: 1,
        input_id: input.input_id().to_string(),
        calibration_id: None,
    };
    assert_eq!(
        empty_store
            .validate(&manual, &input, None, 48_000, u64::MAX)
            .unwrap(),
        CalibrationValidity::Valid
    );
    let other = ConfiguredInputIdentity::new(
        InputBackend::Jack,
        ConfiguredDeviceSelector::JackSourceClient("other".into()),
        "mic",
        "other:capture_1",
    )
    .unwrap();
    assert_stale(
        &empty_store,
        &manual,
        &other,
        48_000,
        u64::MAX,
        StaleReason::InputMismatch,
    );

    let malicious = ActivityThresholdConfig {
        calibration_id: Some("../../escape".into()),
        ..calibrated
    };
    assert_stale(
        &empty_store,
        &malicious,
        &input,
        48_000,
        1_000,
        StaleReason::GenerationMismatch,
    );
}

#[test]
fn malformed_metadata_numeric_and_identity_fields_never_validate() {
    let changes: [fn(&mut CalibrationMetadata); 9] = [
        |m| m.input_id = "unsafe/input".into(),
        |m| {
            let mut forged = serde_json::to_value(identity()).unwrap();
            forged["name"] = serde_json::json!(" name ");
            m.configured_input = Some(serde_json::from_value(forged).unwrap());
        },
        |m| {
            m.resolved_live_input = Some(live_identity_with_forged_field(
                "key_value",
                serde_json::json!(" device-42 "),
            ));
        },
        |m| m.sample_rate = 0,
        |m| m.frames = 0,
        |m| m.complete_windows = 0,
        |m| m.complete_windows = m.frames + 1,
        |m| m.p95_rms = -1.0,
        |m| m.observed_peak = -1.0,
    ];
    for change in changes {
        let (_temp, store, input, prepared, threshold) = generation_fixture();
        rewrite_metadata(&prepared, change);
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptMetadata,
        );
    }
}

#[test]
fn finite_incoherent_persisted_summaries_are_corrupt_metadata() {
    for change in [
        |m: &mut CalibrationMetadata| m.p95_rms = 0.019,
        |m: &mut CalibrationMetadata| m.p95_rms = m.observed_peak + 0.1,
        |m: &mut CalibrationMetadata| m.observed_peak = 1.1,
    ] {
        let (_temp, store, input, prepared, threshold) = generation_fixture();
        rewrite_metadata(&prepared, change);
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptMetadata,
        );
    }
}

#[test]
fn canonical_wav_header_and_every_reopen_field_are_exact() {
    let (_temp, store, input, prepared, threshold) = generation_fixture();
    let original = std::fs::read(prepared.sample_path()).unwrap();
    assert_eq!(original.len(), 44 + 3 * 4);
    assert_eq!(&original[0..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(original[4..8].try_into().unwrap()), 48);
    assert_eq!(&original[8..12], b"WAVE");
    assert_eq!(&original[12..16], b"fmt ");
    assert_eq!(u32::from_le_bytes(original[16..20].try_into().unwrap()), 16);
    assert_eq!(u16::from_le_bytes(original[20..22].try_into().unwrap()), 3);
    assert_eq!(u16::from_le_bytes(original[22..24].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(original[24..28].try_into().unwrap()),
        48_000
    );
    assert_eq!(
        u32::from_le_bytes(original[28..32].try_into().unwrap()),
        192_000
    );
    assert_eq!(u16::from_le_bytes(original[32..34].try_into().unwrap()), 4);
    assert_eq!(u16::from_le_bytes(original[34..36].try_into().unwrap()), 32);
    assert_eq!(&original[36..40], b"data");
    assert_eq!(u32::from_le_bytes(original[40..44].try_into().unwrap()), 12);
    assert_eq!(&original[44..48], &0.0f32.to_le_bytes());
    assert_eq!(&original[48..52], &(-0.5f32).to_le_bytes());
    assert_eq!(&original[52..56], &1.0f32.to_le_bytes());

    let corruptions: &[(usize, &[u8])] = &[
        (0, b"NOPE"),
        (4, &0u32.to_le_bytes()),
        (8, b"NOPE"),
        (12, b"NOPE"),
        (16, &0u32.to_le_bytes()),
        (20, &1u16.to_le_bytes()),
        (22, &2u16.to_le_bytes()),
        (24, &44_100u32.to_le_bytes()),
        (28, &0u32.to_le_bytes()),
        (32, &2u16.to_le_bytes()),
        (34, &16u16.to_le_bytes()),
        (36, b"NOPE"),
        (40, &0u32.to_le_bytes()),
    ];
    for &(offset, replacement) in corruptions {
        let mut corrupted = original.clone();
        corrupted[offset..offset + replacement.len()].copy_from_slice(replacement);
        std::fs::write(prepared.sample_path(), corrupted).unwrap();
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptSample,
        );
    }
    for malformed in [&original[..43], &original[..original.len() - 1]] {
        std::fs::write(prepared.sample_path(), malformed).unwrap();
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptSample,
        );
    }
    let mut extra = original.clone();
    extra.push(0);
    std::fs::write(prepared.sample_path(), extra).unwrap();
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::CorruptSample,
    );

    std::fs::write(prepared.sample_path(), &original).unwrap();
    rewrite_metadata(&prepared, |m| m.frames -= 1);
    assert_stale(
        &store,
        &threshold,
        &input,
        48_000,
        1_000,
        StaleReason::CorruptSample,
    );

    let (_temp, store, input, prepared, threshold) = generation_fixture();
    rewrite_metadata(&prepared, |m| m.sample_rate = u32::MAX);
    assert_stale(
        &store,
        &threshold,
        &input,
        u32::MAX,
        1_000,
        StaleReason::CorruptSample,
    );
}

#[test]
fn non_finite_wav_payload_is_corrupt_sample() {
    for value in [f32::NAN, f32::INFINITY] {
        let (_temp, store, input, prepared, threshold) = generation_fixture();
        let mut bytes = std::fs::read(prepared.sample_path()).unwrap();
        bytes[44..48].copy_from_slice(&value.to_le_bytes());
        std::fs::write(prepared.sample_path(), bytes).unwrap();
        assert_stale(
            &store,
            &threshold,
            &input,
            48_000,
            1_000,
            StaleReason::CorruptSample,
        );
    }
}

#[test]
fn prepare_above_store_bound_creates_no_input_or_generation_directory() {
    let temp = tempfile::tempdir().unwrap();
    let store = CalibrationStore::new(temp.path(), 2).unwrap();
    let input = identity();

    assert!(store
        .prepare(
            &input,
            &live_identity(),
            "too-large",
            &[0.0, 0.0, 0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .is_err());
    assert!(!temp.path().join(input.input_id()).exists());
    assert!(!temp
        .path()
        .join(input.input_id())
        .join("too-large")
        .exists());
}

#[test]
fn metadata_above_store_bound_is_corrupt_before_sample_is_opened() {
    let (_temp, store, input, prepared, threshold) = generation_fixture();
    rewrite_metadata(&prepared, |metadata| metadata.frames = MAXIMUM_FRAMES + 1);
    std::fs::remove_file(prepared.sample_path()).unwrap();

    assert_eq!(
        store
            .validate(&threshold, &input, Some(&live_identity()), 48_000, 1_000)
            .unwrap(),
        CalibrationValidity::Stale(StaleReason::CorruptMetadata)
    );
}

#[test]
fn invalid_generation_geometry_never_creates_a_generation_directory() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    for (id, samples, mut rms, peak) in [
        ("empty", vec![], vec![0.0], vec![0.0]),
        ("mismatch", vec![0.0], vec![0.0], vec![0.0, 0.0]),
    ] {
        assert!(store
            .prepare(
                &input,
                &live_identity(),
                id,
                &samples,
                1_000,
                -110.0,
                &mut rms,
                &peak,
                1
            )
            .is_err());
        assert!(!temp.path().join(input.input_id()).join(id).exists());
    }
    assert!(store
        .prepare(
            &input,
            &live_identity(),
            "../escape",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1
        )
        .is_err());
    assert!(!temp.path().join("escape").exists());
    assert!(store
        .prepare(
            &input,
            &live_identity(),
            "incoherent-threshold",
            &[0.0],
            1_000,
            -30.0,
            &mut [0.0],
            &[0.0],
            1
        )
        .is_err());
    assert!(!temp
        .path()
        .join(input.input_id())
        .join("incoherent-threshold")
        .exists());
}

#[test]
fn inconsistent_supplied_statistics_never_create_a_generation_directory() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    for (id, samples, mut rms, peak) in [
        ("rms-above-peak", vec![0.5], vec![0.1], vec![0.05]),
        ("peak-above-sample", vec![0.25], vec![0.1], vec![0.5]),
    ] {
        let threshold = derived_threshold(&rms);
        assert!(store
            .prepare(
                &input,
                &live_identity(),
                id,
                &samples,
                1_000,
                threshold,
                &mut rms,
                &peak,
                1,
            )
            .is_err());
        assert!(!temp.path().join(input.input_id()).join(id).exists());
    }
}

#[test]
fn every_generation_durability_checkpoint_cleans_only_the_new_generation() {
    for checkpoint in [
        DurabilityCheckpoint::SampleWritten,
        DurabilityCheckpoint::SampleSynced,
        DurabilityCheckpoint::MetadataWritten,
        DurabilityCheckpoint::MetadataSynced,
        DurabilityCheckpoint::GenerationDirectorySynced,
        DurabilityCheckpoint::InputDirectorySynced,
        DurabilityCheckpoint::RootDirectorySynced,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = calibration_store(temp.path());
        let input = identity();
        let mut old = store
            .prepare(
                &input,
                &live_identity(),
                "old",
                &[0.25],
                1_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                1,
            )
            .unwrap();
        old.mark_authoritative();
        let old_bytes = generation_bytes(&old);
        let config = temp.path().join("lamb.toml");
        let config_bytes = b"old authoritative config\n".to_vec();
        std::fs::write(&config, &config_bytes).unwrap();
        let callback_count = Cell::new(0);
        let mut hook = |actual| {
            if actual == checkpoint {
                Err(lamb::error::LambError::Config(
                    "injected preparation failure".into(),
                ))
            } else {
                Ok(())
            }
        };
        assert!(store
            .prepare_with_hook(
                &input,
                &live_identity(),
                "new",
                &[0.5],
                1_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                2,
                &mut hook,
            )
            .is_err());
        assert_eq!(std::fs::read(&config).unwrap(), config_bytes);
        assert_eq!(generation_bytes(&old), old_bytes);
        assert!(!temp.path().join(input.input_id()).join("new").exists());
        assert_eq!(callback_count.get(), 0);
        callback_count.set(0);
    }
}

#[test]
fn prepared_cleanup_preserves_a_foreign_replacement_after_identity_capture() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "prepared",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    let original = prepared.path().to_path_buf();
    let moved = original.with_file_name("recorded-moved-away");
    let mut hook = |path: &std::path::Path, checkpoint| {
        assert_eq!(checkpoint, CleanupCheckpoint::IdentityCaptured);
        assert_eq!(path, original);
        std::fs::rename(path, &moved).unwrap();
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("foreign"), b"preserve me").unwrap();
        Ok(())
    };
    assert!(!prepared.cleanup_with_hook(&mut hook).unwrap());
    assert_eq!(
        std::fs::read(original.join("foreign")).unwrap(),
        b"preserve me"
    );
    assert!(moved.exists());
}

#[test]
fn precommit_failures_preserve_old_authority_skip_callback_and_cleanup_prepared() {
    for failed_checkpoint in [
        None,
        Some(DurabilityCheckpoint::ConfigTempSynced),
        Some(DurabilityCheckpoint::ConfigRenamed),
        Some(DurabilityCheckpoint::ConfigParentSynced),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let store = calibration_store(temp.path());
        let input = identity();
        let mut old = store
            .prepare(
                &input,
                &live_identity(),
                "old",
                &[0.0],
                1_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                1,
            )
            .unwrap();
        old.mark_authoritative();
        let old_bytes = generation_bytes(&old);
        let config_path = temp.path().join("lamb.toml");
        let config_bytes = b"old authoritative config\n".to_vec();
        std::fs::write(&config_path, &config_bytes).unwrap();
        let mut prepared = store
            .prepare(
                &input,
                &live_identity(),
                "new",
                &[0.0],
                1_000,
                -110.0,
                &mut [0.0],
                &[0.0],
                2,
            )
            .unwrap();
        let prepared_path = prepared.path().to_path_buf();
        let candidate = if failed_checkpoint.is_none() {
            AppConfig::default()
        } else {
            valid_candidate(&prepared)
        };
        let installed = Cell::new(0);
        let mut config_hook = |actual| {
            if Some(actual) == failed_checkpoint {
                Err(lamb::error::LambError::Config(
                    "injected config failure".into(),
                ))
            } else {
                Ok(())
            }
        };
        assert!(commit_prepared_generation_with_hooks(
            &config_path,
            &candidate,
            &mut prepared,
            None,
            || installed.set(installed.get() + 1),
            &mut config_hook,
            &mut |_, _| Ok(()),
        )
        .is_err());
        assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
        assert_eq!(generation_bytes(&old), old_bytes);
        assert_eq!(installed.get(), 0);
        assert!(!prepared_path.exists());
        assert!(std::fs::read_dir(temp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".lamb.toml.config")
        }));
    }
}

#[test]
fn precommit_cleanup_hook_failure_preserves_operation_and_cleanup_contexts() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let config_path = temp.path().join("lamb.toml");
    let config_bytes = b"old authoritative config\n";
    std::fs::write(&config_path, config_bytes).unwrap();
    let installed = Cell::new(0);

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &AppConfig::default(),
        &mut prepared,
        None,
        || installed.set(installed.get() + 1),
        &mut |_| Ok(()),
        &mut |_, checkpoint| {
            assert_eq!(checkpoint, CleanupCheckpoint::IdentityCaptured);
            Err(lamb::error::LambError::Config(
                "injected prepared cleanup failure".into(),
            ))
        },
    )
    .unwrap_err();

    match error {
        lamb::error::LambError::PersistenceCleanup { operation, cleanup } => {
            assert!(
                matches!(*operation, lamb::error::LambError::Validation(ref message)
                if message == "candidate does not reference exactly the prepared calibrated generation")
            );
            assert!(
                matches!(*cleanup, lamb::error::LambError::Config(ref message)
                if message == "injected prepared cleanup failure")
            );
        }
        other => panic!("expected persistence cleanup error, got {other:?}"),
    }
    assert_eq!(installed.get(), 0);
    assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
    assert!(prepared_path.exists());
}

#[test]
fn precommit_cleanup_identity_race_is_reported_instead_of_discarded() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let moved_prepared = temp.path().join("prepared-moved-away");
    let config_path = temp.path().join("lamb.toml");
    std::fs::write(&config_path, b"old\n").unwrap();

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &AppConfig::default(),
        &mut prepared,
        None,
        || panic!("must not install"),
        &mut |_| Ok(()),
        &mut |path, _| {
            std::fs::rename(path, &moved_prepared).unwrap();
            std::fs::create_dir(path).unwrap();
            Ok(())
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        lamb::error::LambError::PersistenceCleanup { cleanup, .. }
            if matches!(*cleanup, lamb::error::LambError::UnidentifiedStagingCleanup { ref path }
                if path == &prepared_path)
    ));
    assert_eq!(std::fs::read(&config_path).unwrap(), b"old\n");
    assert!(prepared_path.exists());
    assert!(moved_prepared.exists());
}

#[test]
fn config_persistence_cleanup_failure_conservatively_preserves_prepared() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    let config_bytes = b"old authoritative config\n";
    std::fs::write(&config_path, config_bytes).unwrap();
    let moved_temp = temp.path().join("candidate-temp-moved-away");
    let installed = Cell::new(0);
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigTempSynced {
            let config_temp = std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
                })
                .unwrap();
            std::fs::rename(&config_temp, &moved_temp).unwrap();
            std::fs::write(&config_temp, b"foreign temporary replacement").unwrap();
            return Err(lamb::error::LambError::Config(
                "injected config preparation failure".into(),
            ));
        }
        Ok(())
    };

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || installed.set(installed.get() + 1),
        &mut hook,
        &mut |_, _| Ok(()),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        lamb::error::LambError::PersistenceCleanup { .. }
    ));
    assert_eq!(installed.get(), 0);
    assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
    assert!(prepared_path.exists());
    assert!(moved_temp.exists());
}

#[test]
fn candidate_structure_is_validated_before_parent_or_temp_side_effects() {
    fn activity(candidate: &mut AppConfig) -> &mut ActivityThresholdConfig {
        candidate
            .profiles
            .get_mut("incomplete")
            .unwrap()
            .channels
            .get_mut("aux")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing-parent").join("lamb.toml");
    let mut candidate = AppConfig::default();
    candidate.daemon.start_mode = "sometimes".into();
    assert!(save_config_atomic(&path, &candidate).is_err());
    assert!(!path.parent().unwrap().exists());

    candidate.daemon.start_mode = "manual".into();
    let profile = candidate.profiles.entry("incomplete".into()).or_default();
    profile.channels.insert(
        "aux".into(),
        lamb::app_config::ProfileChannelConfig {
            activity: Some(ActivityThresholdConfig {
                threshold_dbfs: f64::NAN,
                threshold_source: ThresholdSource::Manual,
                updated_at_unix_seconds: 1,
                input_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                calibration_id: None,
            }),
        },
    );
    assert!(save_config_atomic(&path, &candidate).is_err());
    assert!(!path.parent().unwrap().exists());

    activity(&mut candidate).threshold_dbfs = -60.0;
    activity(&mut candidate).input_id = "  ".into();
    assert!(save_config_atomic(&path, &candidate).is_err());
    activity(&mut candidate).input_id =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
    activity(&mut candidate).calibration_id = Some(" ".into());
    assert!(save_config_atomic(&path, &candidate).is_err());
    activity(&mut candidate).calibration_id = None;
    activity(&mut candidate).threshold_source = ThresholdSource::Calibrated;
    assert!(save_config_atomic(&path, &candidate).is_err());
    assert!(!path.parent().unwrap().exists());

    activity(&mut candidate).threshold_source = ThresholdSource::Manual;
    save_config_atomic(&path, &candidate).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'));
    assert!(!text.ends_with("\n\n"));
    assert_eq!(toml::from_str::<AppConfig>(&text).unwrap(), candidate);
}

#[test]
fn persisted_activity_binding_ids_use_stable_safe_grammars_before_atomic_save() {
    const VALID_INPUT_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn candidate(source: ThresholdSource, calibration_id: Option<&str>) -> AppConfig {
        let mut candidate = AppConfig::default();
        candidate
            .profiles
            .entry("incomplete".into())
            .or_default()
            .channels
            .insert(
                "aux".into(),
                lamb::app_config::ProfileChannelConfig {
                    activity: Some(ActivityThresholdConfig {
                        threshold_dbfs: -60.0,
                        threshold_source: source,
                        updated_at_unix_seconds: 1,
                        input_id: VALID_INPUT_ID.into(),
                        calibration_id: calibration_id.map(str::to_owned),
                    }),
                },
            );
        candidate
    }

    fn activity(candidate: &mut AppConfig) -> &mut ActivityThresholdConfig {
        candidate
            .profiles
            .get_mut("incomplete")
            .unwrap()
            .channels
            .get_mut("aux")
            .unwrap()
            .activity
            .as_mut()
            .unwrap()
    }

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    for malformed in [
        "short".to_owned(),
        "A".repeat(64),
        "g".repeat(64),
        "a".repeat(63) + "/",
    ] {
        let mut candidate = candidate(ThresholdSource::Manual, None);
        activity(&mut candidate).input_id = malformed;
        assert!(save_config_atomic(&path, &candidate).is_err());
    }

    for source in [ThresholdSource::Manual, ThresholdSource::Calibrated] {
        for malformed in [
            String::new(),
            "   ".to_owned(),
            "a".repeat(129),
            "unsafe/path".to_owned(),
            "surrounding-space ".to_owned(),
        ] {
            assert!(save_config_atomic(&path, &candidate(source, Some(&malformed))).is_err());
        }
    }

    assert!(save_config_atomic(&path, &candidate(ThresholdSource::Calibrated, None)).is_err());
    let valid_manual = candidate(ThresholdSource::Manual, Some("old_generation-1"));
    save_config_atomic(&path, &valid_manual).unwrap();
    assert_eq!(
        toml::from_str::<AppConfig>(&std::fs::read_to_string(path).unwrap()).unwrap(),
        valid_manual
    );
}

#[test]
fn atomic_config_preserves_metadata_rejects_special_targets_and_retries_collisions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    std::fs::write(&path, b"old\n").unwrap();
    // Keep a non-default mode while avoiding set-ID bits, which sandboxed Nix
    // builders may reject even for a builder-owned temporary file.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0751)).unwrap();
    let before = std::fs::symlink_metadata(&path).unwrap();
    let collision = temp.path().join(".lamb.toml.config.41.tmp");
    std::fs::write(&collision, b"collision stays").unwrap();
    let mut names = [41_u64, 42].into_iter();
    save_config_atomic_with_hook_and_name_source(
        &path,
        &AppConfig::default(),
        &mut |_| Ok(()),
        &mut || {
            names
                .next()
                .ok_or_else(|| lamb::error::LambError::Config("names exhausted".into()))
        },
    )
    .unwrap();
    let after = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(after.mode() & 0o7777, before.mode() & 0o7777);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    assert_eq!(std::fs::read(&collision).unwrap(), b"collision stays");
    assert!(!temp.path().join(".lamb.toml.config.42.tmp").exists());

    let missing = temp.path().join("new.toml");
    save_config_atomic(&missing, &AppConfig::default()).unwrap();
    let metadata = std::fs::symlink_metadata(&missing).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o600);
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(metadata.gid(), unsafe { libc::getegid() });

    let referent = temp.path().join("referent");
    std::fs::write(&referent, b"referent").unwrap();
    let link = temp.path().join("link.toml");
    symlink(&referent, &link).unwrap();
    assert!(save_config_atomic(&link, &AppConfig::default()).is_err());
    assert_eq!(std::fs::read(&referent).unwrap(), b"referent");
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    let directory = temp.path().join("directory.toml");
    std::fs::create_dir(&directory).unwrap();
    assert!(save_config_atomic(&directory, &AppConfig::default()).is_err());
    assert!(directory.is_dir());

    let fifo = temp.path().join("fifo.toml");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    assert!(save_config_atomic(&fifo, &AppConfig::default()).is_err());
    assert!(std::fs::symlink_metadata(&fifo)
        .unwrap()
        .file_type()
        .is_fifo());
}

#[test]
fn temp_synced_replacements_are_preserved_for_existing_and_missing_targets() {
    let candidate = AppConfig::default();

    let existing_dir = tempfile::tempdir().unwrap();
    let existing = existing_dir.path().join("lamb.toml");
    let moved_old = existing_dir.path().join("old-preserved.toml");
    let referent = existing_dir.path().join("foreign-referent.toml");
    std::fs::write(&existing, b"old config\n").unwrap();
    std::fs::write(&referent, b"foreign referent\n").unwrap();
    let mut existing_hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigTempSynced {
            std::fs::rename(&existing, &moved_old).unwrap();
            symlink(&referent, &existing).unwrap();
        }
        Ok(())
    };
    assert!(lamb::calibration::save_config_atomic_with_hook(
        &existing,
        &candidate,
        &mut existing_hook,
    )
    .is_err());
    assert_eq!(std::fs::read(&moved_old).unwrap(), b"old config\n");
    assert_eq!(std::fs::read(&referent).unwrap(), b"foreign referent\n");
    assert!(std::fs::symlink_metadata(&existing)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_dir(existing_dir.path())
        .unwrap()
        .all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".lamb.toml.config")
        }));

    let missing_dir = tempfile::tempdir().unwrap();
    let missing = missing_dir.path().join("lamb.toml");
    let mut missing_hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigTempSynced {
            std::fs::write(&missing, b"foreign appeared\n").unwrap();
        }
        Ok(())
    };
    assert!(lamb::calibration::save_config_atomic_with_hook(
        &missing,
        &candidate,
        &mut missing_hook,
    )
    .is_err());
    assert_eq!(std::fs::read(&missing).unwrap(), b"foreign appeared\n");
    assert!(std::fs::read_dir(missing_dir.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".lamb.toml.config")
    }));
}

#[test]
fn missing_target_post_rename_failure_removes_only_candidate_and_returns_ordinary_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigRenamed {
            Err(lamb::error::LambError::Config(
                "injected after missing-target rename".into(),
            ))
        } else {
            Ok(())
        }
    };
    let error =
        lamb::calibration::save_config_atomic_with_hook(&path, &AppConfig::default(), &mut hook)
            .unwrap_err();
    assert!(matches!(error, lamb::error::LambError::Config(_)));
    assert!(!path.exists());
    assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
}

#[test]
fn durable_success_still_installs_when_old_config_cleanup_identity_changes() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    std::fs::write(&config_path, b"old config\n").unwrap();
    let moved_old = temp.path().join("old-config-preserved");
    let installed = Cell::new(0);
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigParentSynced {
            let recovery = std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
                })
                .unwrap();
            std::fs::rename(&recovery, &moved_old).unwrap();
            std::fs::write(&recovery, b"foreign recovery replacement").unwrap();
        }
        Ok(())
    };
    let result = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || installed.set(installed.get() + 1),
        &mut hook,
        &mut |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(result, OldGenerationCleanup::NotRequested);
    assert_eq!(installed.get(), 1);
    assert_eq!(std::fs::read(&moved_old).unwrap(), b"old config\n");
    assert_eq!(
        toml::from_str::<AppConfig>(&std::fs::read_to_string(&config_path).unwrap()).unwrap(),
        candidate
    );
}

#[test]
fn post_exchange_identity_change_preserves_foreign_target_and_old_config_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    let old_bytes = b"old authoritative config\n";
    let foreign_bytes = b"foreign replacement\n";
    std::fs::write(&config_path, old_bytes).unwrap();
    let moved_candidate = temp.path().join("candidate-moved-away.toml");
    let installed = Cell::new(0);
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigRenamed {
            std::fs::rename(&config_path, &moved_candidate).unwrap();
            std::fs::write(&config_path, foreign_bytes).unwrap();
            Err(lamb::error::LambError::Config(
                "injected post-exchange failure".into(),
            ))
        } else {
            Ok(())
        }
    };

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || installed.set(installed.get() + 1),
        &mut hook,
        &mut |_, _| Ok(()),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        lamb::error::LambError::IndeterminatePublication { .. }
    ));
    assert_eq!(std::fs::read(&config_path).unwrap(), foreign_bytes);
    let recovery = std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
        })
        .expect("old authoritative config must remain in adjacent recovery state");
    assert_eq!(std::fs::read(recovery).unwrap(), old_bytes);
    assert_eq!(installed.get(), 0);
    drop(prepared);
    assert!(prepared_path.exists());
}

#[test]
fn indeterminate_publication_keeps_prepared_generation_after_drop() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    let old_bytes = b"old authoritative config\n";
    let foreign_bytes = b"foreign replacement\n";
    std::fs::write(&config_path, old_bytes).unwrap();
    let moved_candidate = temp.path().join("candidate-moved-away.toml");
    let installed = Cell::new(0);
    let cleanup_invocations = Cell::new(0);
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigRenamed {
            std::fs::rename(&config_path, &moved_candidate).unwrap();
            std::fs::write(&config_path, foreign_bytes).unwrap();
            Err(lamb::error::LambError::Config(
                "injected post-exchange failure".into(),
            ))
        } else {
            Ok(())
        }
    };

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || installed.set(installed.get() + 1),
        &mut hook,
        &mut |_, _| {
            cleanup_invocations.set(cleanup_invocations.get() + 1);
            Ok(())
        },
    )
    .unwrap_err();

    assert!(matches!(
        error,
        lamb::error::LambError::IndeterminatePublication { .. }
    ));
    assert_eq!(installed.get(), 0);
    assert_eq!(std::fs::read(&config_path).unwrap(), foreign_bytes);
    assert!(std::fs::read_dir(temp.path())
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .any(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
                && path.exists()
        }));
    assert_eq!(cleanup_invocations.get(), 0);
    drop(prepared);
    assert!(prepared_path.exists());
}

#[test]
fn installed_candidate_with_disturbed_old_recovery_is_indeterminate_and_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut old = store
        .prepare(
            &input,
            &live_identity(),
            "old",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    let previous = RecordedGeneration::capture(old.path()).unwrap();
    old.mark_authoritative();
    let old_generation_bytes = generation_bytes(&old);
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    let old_config_bytes = b"old authoritative config\n";
    std::fs::write(&config_path, old_config_bytes).unwrap();
    let moved_recovery = temp.path().join("old-config-recovery-moved");
    let installed = Cell::new(0);
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigRenamed {
            let recovery = std::fs::read_dir(temp.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
                })
                .unwrap();
            std::fs::rename(&recovery, &moved_recovery).unwrap();
            std::fs::write(&recovery, b"foreign recovery replacement").unwrap();
            return Err(lamb::error::LambError::Config(
                "injected post-exchange recovery disturbance".into(),
            ));
        }
        Ok(())
    };

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        Some(previous),
        || installed.set(installed.get() + 1),
        &mut hook,
        &mut |_, _| Ok(()),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        lamb::error::LambError::IndeterminatePublication { operation }
            if matches!(*operation, lamb::error::LambError::Config(ref message)
                if message == "injected post-exchange recovery disturbance")
    ));
    assert_eq!(installed.get(), 0);
    assert_eq!(
        toml::from_str::<AppConfig>(&std::fs::read_to_string(&config_path).unwrap()).unwrap(),
        candidate
    );
    assert_eq!(std::fs::read(&moved_recovery).unwrap(), old_config_bytes);
    assert_eq!(generation_bytes(&old), old_generation_bytes);
    assert!(prepared_path.exists());
}

#[test]
fn commit_rejects_previous_generation_aliasing_prepared_before_config_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let previous = RecordedGeneration::capture(&prepared_path).unwrap();
    let candidate = valid_candidate(&prepared);
    let config_path = temp.path().join("lamb.toml");
    let config_bytes = b"old exact config bytes\n";
    std::fs::write(&config_path, config_bytes).unwrap();
    let installed = Cell::new(0);

    let error = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        Some(previous),
        || installed.set(installed.get() + 1),
        &mut |_| panic!("must reject before config persistence"),
        &mut |_, _| panic!("must not clean prepared on caller misuse"),
    )
    .unwrap_err();

    assert!(
        matches!(error, lamb::error::LambError::Validation(ref message)
        if message == "previous generation aliases the prepared generation")
    );
    assert_eq!(installed.get(), 0);
    assert_eq!(std::fs::read(&config_path).unwrap(), config_bytes);
    assert!(prepared_path.exists());
}

#[test]
fn commit_requires_the_exact_prepared_threshold_reference() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let mut candidate = valid_candidate(&prepared);
    candidate
        .profiles
        .get_mut("scarlett")
        .unwrap()
        .channels
        .get_mut("aux3")
        .unwrap()
        .activity
        .as_mut()
        .unwrap()
        .threshold_dbfs += 1e-12;
    let config_path = temp.path().join("lamb.toml");
    std::fs::write(&config_path, b"old\n").unwrap();
    assert!(commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || panic!("must not install"),
        &mut |_| Ok(()),
        &mut |_, _| Ok(()),
    )
    .is_err());
    assert_eq!(std::fs::read(&config_path).unwrap(), b"old\n");
    assert!(!prepared_path.exists());
}

#[test]
fn commit_rejects_an_invalid_profile_containing_the_prepared_reference() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let mut candidate = valid_candidate(&prepared);
    candidate.profiles.get_mut("scarlett").unwrap().backend = None;
    let config_path = temp.path().join("lamb.toml");
    std::fs::write(&config_path, b"old\n").unwrap();
    assert!(commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        None,
        || panic!("must not install"),
        &mut |_| Ok(()),
        &mut |_, _| Ok(()),
    )
    .is_err());
    assert_eq!(std::fs::read(&config_path).unwrap(), b"old\n");
    assert!(!prepared_path.exists());
}

#[test]
fn successful_commit_is_durable_before_one_callback_and_reports_old_identity_race_pending() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut old = store
        .prepare(
            &input,
            &live_identity(),
            "old",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    let old_recorded = RecordedGeneration::capture(old.path()).unwrap();
    old.mark_authoritative();
    let old_path = old.path().to_path_buf();
    let moved_old = old_path.with_file_name("old-recorded-moved");
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "new",
            &[0.25],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            2,
        )
        .unwrap();
    let prepared_path = prepared.path().to_path_buf();
    let mut candidate = valid_candidate(&prepared);
    candidate
        .profiles
        .insert("inactive-incomplete".into(), Default::default());
    let config_path = temp.path().join("lamb.toml");
    std::fs::write(&config_path, b"old\n").unwrap();
    let installed = Cell::new(0);
    let mut cleanup_hook = |path: &std::path::Path, _| {
        if path == old_path {
            std::fs::rename(path, &moved_old).unwrap();
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("foreign"), b"preserve me").unwrap();
        }
        Ok(())
    };
    let cleanup = commit_prepared_generation_with_hooks(
        &config_path,
        &candidate,
        &mut prepared,
        Some(old_recorded),
        || {
            let persisted: AppConfig =
                toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
            assert_eq!(persisted, candidate);
            installed.set(installed.get() + 1);
        },
        &mut |_| Ok(()),
        &mut cleanup_hook,
    )
    .unwrap();
    assert_eq!(installed.get(), 1);
    assert_eq!(cleanup, OldGenerationCleanup::Pending(old_path.clone()));
    assert_eq!(
        std::fs::read(old_path.join("foreign")).unwrap(),
        b"preserve me"
    );
    drop(prepared);
    assert!(prepared_path.exists());
}

#[test]
fn startup_cleanup_keeps_referenced_removes_orphans_and_reports_foreign_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut referenced = store
        .prepare(
            &input,
            &live_identity(),
            "referenced",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    referenced.mark_authoritative();
    let mut orphan = store
        .prepare(
            &input,
            &live_identity(),
            "orphan",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    orphan.mark_authoritative();
    let mut replaced = store
        .prepare(
            &input,
            &live_identity(),
            "replaced",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    replaced.mark_authoritative();
    let incomplete = temp.path().join(input.input_id()).join("incomplete");
    std::fs::create_dir(&incomplete).unwrap();
    std::fs::write(incomplete.join("sample.wav"), b"partial").unwrap();
    let replaced_path = replaced.path().to_path_buf();
    let moved = replaced_path.with_file_name("replaced-recorded-moved");
    let mut hook = |path: &std::path::Path, _| {
        if path == replaced_path {
            std::fs::rename(path, &moved).unwrap();
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("foreign"), b"preserve me").unwrap();
        }
        Ok(())
    };
    let keep = BTreeSet::from([(
        input.input_id().to_owned(),
        referenced.calibration_id().to_owned(),
    )]);
    let pending = store
        .cleanup_unreferenced_with_hook(&keep, &mut hook)
        .unwrap();
    assert!(referenced.path().exists());
    assert!(!orphan.path().exists());
    assert!(!incomplete.exists());
    assert_eq!(pending, vec![replaced_path.clone()]);
    assert_eq!(
        std::fs::read(replaced_path.join("foreign")).unwrap(),
        b"preserve me"
    );
}

#[test]
fn root_maintenance_api_inspects_and_cleans_without_an_unbounded_preparation_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let live = live_identity();
    let mut referenced = store
        .prepare(
            &input,
            &live,
            "referenced-root-api",
            &[0.25],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            10,
        )
        .unwrap();
    referenced.mark_authoritative();
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: -110.0,
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 10,
        input_id: input.input_id().to_string(),
        calibration_id: Some("referenced-root-api".to_string()),
    };

    let inspected = CalibrationStore::inspect_root(temp.path(), &threshold, &input, 10).unwrap();
    assert_eq!(inspected.status, CalibrationArtifactStatus::Complete);
    assert_eq!(inspected.metadata.unwrap().sample_rate, 1_000);

    let orphan = temp.path().join(input.input_id()).join("orphan-root-api");
    std::fs::create_dir(&orphan).unwrap();
    std::fs::write(orphan.join("partial"), b"bounded maintenance").unwrap();
    let keep = BTreeSet::from([(
        input.input_id().to_string(),
        "referenced-root-api".to_string(),
    )]);
    assert_eq!(
        CalibrationStore::cleanup_root(temp.path(), &keep).unwrap(),
        Vec::<std::path::PathBuf>::new()
    );
    assert!(referenced.path().exists());
    assert!(!orphan.exists());
}

#[test]
fn prepare_refuses_an_input_directory_symlink_without_touching_its_referent() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let input = identity();
    symlink(outside.path(), temp.path().join(input.input_id())).unwrap();

    let store = calibration_store(temp.path());
    assert!(store
        .prepare(
            &input,
            &live_identity(),
            "anchored",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .is_err());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn validation_refuses_state_ancestor_and_generation_symlinks_without_reading_outside() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let input = identity();
    let external = calibration_store(outside.path())
        .prepare(
            &input,
            &live_identity(),
            "external",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    let threshold = ActivityThresholdConfig {
        threshold_dbfs: -110.0,
        threshold_source: ThresholdSource::Calibrated,
        updated_at_unix_seconds: 1,
        input_id: input.input_id().into(),
        calibration_id: Some("external".into()),
    };
    let ancestor = temp.path().join("ancestor");
    symlink(outside.path(), &ancestor).unwrap();
    let store = calibration_store(ancestor.join("ignored"));
    assert!(matches!(
        store.validate(&threshold, &input, Some(&live_identity()), 1_000, 1),
        Ok(CalibrationValidity::Stale(StaleReason::MissingState))
    ));
    assert!(external.path().exists());
}

#[test]
fn config_parent_intermediate_symlink_is_rejected_without_touching_referent() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let linked = temp.path().join("linked");
    symlink(outside.path(), &linked).unwrap();
    let config = linked.join("nested").join("lamb.toml");
    assert!(save_config_atomic(&config, &AppConfig::default()).is_err());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn temp_synced_parent_replacement_never_writes_the_replacement_parent() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("config-parent");
    std::fs::create_dir(&parent).unwrap();
    let old_parent = temp.path().join("old-parent");
    let config = parent.join("lamb.toml");
    std::fs::write(&config, b"old config\n").unwrap();
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigTempSynced {
            std::fs::rename(&parent, &old_parent).unwrap();
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(parent.join("lamb.toml"), b"foreign config\n").unwrap();
        }
        Ok(())
    };
    assert!(lamb::calibration::save_config_atomic_with_hook(
        &config,
        &AppConfig::default(),
        &mut hook,
    )
    .is_err());
    assert_eq!(
        std::fs::read(parent.join("lamb.toml")).unwrap(),
        b"foreign config\n"
    );
    assert_eq!(
        std::fs::read(old_parent.join("lamb.toml")).unwrap(),
        b"old config\n"
    );
    assert!(std::fs::read_dir(&old_parent).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".lamb.toml.config")));
}

#[test]
fn prepare_refuses_a_generation_symlink_without_overwriting_the_external_directory() {
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let input = identity();
    std::fs::create_dir(temp.path().join(input.input_id())).unwrap();
    symlink(
        outside.path(),
        temp.path().join(input.input_id()).join("aliased"),
    )
    .unwrap();
    assert!(calibration_store(temp.path())
        .prepare(
            &input,
            &live_identity(),
            "aliased",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1
        )
        .is_err());
    assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
}

#[test]
fn post_rename_parent_replacement_is_indeterminate_and_preserves_recovery_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().join("config-parent");
    std::fs::create_dir(&parent).unwrap();
    let moved = temp.path().join("config-parent-moved");
    let config = parent.join("lamb.toml");
    std::fs::write(&config, b"old config\n").unwrap();
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::ConfigRenamed {
            std::fs::rename(&parent, &moved).unwrap();
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(parent.join("lamb.toml"), b"foreign config\n").unwrap();
        }
        Ok(())
    };
    let error =
        lamb::calibration::save_config_atomic_with_hook(&config, &AppConfig::default(), &mut hook)
            .unwrap_err();
    assert!(matches!(
        error,
        lamb::error::LambError::IndeterminatePublication { .. }
    ));
    assert_eq!(
        std::fs::read(parent.join("lamb.toml")).unwrap(),
        b"foreign config\n"
    );
    assert_ne!(
        std::fs::read(moved.join("lamb.toml")).unwrap(),
        b"old config\n"
    );
    let recovery = std::fs::read_dir(&moved)
        .unwrap()
        .map(std::result::Result::unwrap)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".lamb.toml.config"))
        })
        .expect("old recovery artifact remains in the anchored moved parent");
    assert_eq!(std::fs::read(recovery).unwrap(), b"old config\n");
}

#[test]
fn nested_missing_ancestry_syncs_each_created_component_before_working() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("one").join("two");
    let input = identity();
    let count = Cell::new(0);
    calibration_store(&root)
        .prepare_with_hook(
            &input,
            &live_identity(),
            "nested",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
            &mut |checkpoint| {
                if checkpoint == DurabilityCheckpoint::CreatedParentSynced {
                    count.set(count.get() + 1);
                }
                Ok(())
            },
        )
        .unwrap();
    // `one`, `two`, and the stable input directory were created through
    // descriptor parents before the generation was created.
    assert_eq!(count.get(), 3);
}

#[test]
fn prepared_cleanup_removes_its_owned_generation_and_returns_true() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "owned",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    assert!(prepared.cleanup().unwrap());
    assert!(!prepared.path().exists());
}

#[test]
fn cleanup_quarantine_replacement_is_left_pending_without_recursing_into_foreign_data() {
    let temp = tempfile::tempdir().unwrap();
    let store = calibration_store(temp.path());
    let input = identity();
    let mut prepared = store
        .prepare(
            &input,
            &live_identity(),
            "owned",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
        )
        .unwrap();
    let moved = temp.path().join("owned-quarantine-moved");
    let mut hook = |path: &std::path::Path, checkpoint| {
        if checkpoint == CleanupCheckpoint::QuarantineVerified {
            std::fs::rename(path, &moved).unwrap();
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("foreign"), b"preserve me").unwrap();
        }
        Ok(())
    };
    assert!(!prepared.cleanup_with_hook(&mut hook).unwrap());
    let foreign = std::fs::read_dir(temp.path().join(input.input_id()))
        .unwrap()
        .map(std::result::Result::unwrap)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".owned.cleanup"))
        })
        .expect("foreign quarantine replacement remains");
    assert_eq!(
        std::fs::read(foreign.join("foreign")).unwrap(),
        b"preserve me"
    );
    assert!(moved.exists());
}

#[test]
fn created_child_replacement_after_ancestry_sync_receives_no_generation_files() {
    let temp = tempfile::tempdir().unwrap();
    let input = identity();
    let replacement = temp.path().join(input.input_id());
    let mut replaced = false;
    let result = calibration_store(temp.path()).prepare_with_hook(
        &input,
        &live_identity(),
        "owned",
        &[0.0],
        1_000,
        -110.0,
        &mut [0.0],
        &[0.0],
        1,
        &mut |checkpoint| {
            if checkpoint == DurabilityCheckpoint::CreatedParentSynced && !replaced {
                replaced = true;
                std::fs::rename(&replacement, temp.path().join("input-moved")).unwrap();
                std::fs::create_dir(&replacement).unwrap();
                std::fs::write(replacement.join("foreign"), b"preserve me").unwrap();
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(replacement.join("foreign")).unwrap(),
        b"preserve me"
    );
    assert!(!replacement.join("owned").exists());
}

#[test]
fn root_replacement_after_root_sync_fails_without_touching_the_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let store = calibration_store(&root);
    let input = identity();
    let moved = temp.path().join("state-moved");
    let mut hook = |checkpoint| {
        if checkpoint == DurabilityCheckpoint::RootDirectorySynced {
            std::fs::rename(&root, &moved).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("foreign"), b"preserve me").unwrap();
        }
        Ok(())
    };
    assert!(store
        .prepare_with_hook(
            &input,
            &live_identity(),
            "owned",
            &[0.0],
            1_000,
            -110.0,
            &mut [0.0],
            &[0.0],
            1,
            &mut hook,
        )
        .is_err());
    assert_eq!(std::fs::read(root.join("foreign")).unwrap(), b"preserve me");
    assert!(!moved.join(input.input_id()).join("owned").exists());
}
