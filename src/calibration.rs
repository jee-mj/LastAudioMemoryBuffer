//! Durable, immutable calibration sample generations and profile persistence.
use crate::activity::{ThresholdSource, WINDOWED_RMS_PEAK_DETECTOR_VERSION};
use crate::app_config::{ActivityThresholdConfig, AppConfig};
use crate::capture_arena::CalibrationLease;
use crate::error::{io_error, LambError, Result};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const WAV_HEADER_BYTES: usize = 44;
const STALE_SECONDS: u64 = 30 * 24 * 60 * 60;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputBackend {
    PipeWire,
    Jack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum ConfiguredDeviceSelector {
    PipeWireTarget(String),
    PipeWireAuto,
    JackSourceClient(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LiveDeviceKeyKind {
    HardwareSerial,
    ObjectPath,
    NodeName,
    JackSourceClient,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedLiveInputIdentity {
    pub backend: InputBackend,
    pub key_kind: LiveDeviceKeyKind,
    pub key_value: String,
    pub resolved_source: String,
}

impl ResolvedLiveInputIdentity {
    pub fn new(
        backend: InputBackend,
        key_kind: LiveDeviceKeyKind,
        key_value: &str,
        resolved_source: &str,
    ) -> Result<Self> {
        let key_value = canonical_identity_field(key_value, "live device key")?;
        let resolved_source = canonical_identity_field(resolved_source, "resolved source")?;
        match (backend, key_kind) {
            (InputBackend::PipeWire, LiveDeviceKeyKind::JackSourceClient)
            | (InputBackend::Jack, LiveDeviceKeyKind::HardwareSerial)
            | (InputBackend::Jack, LiveDeviceKeyKind::ObjectPath)
            | (InputBackend::Jack, LiveDeviceKeyKind::NodeName) => {
                return Err(LambError::Validation(
                    "live device key kind does not match backend".into(),
                ));
            }
            _ => {}
        }
        if backend == InputBackend::Jack {
            validate_jack_source(&key_value, &resolved_source)?;
        }
        Ok(Self {
            backend,
            key_kind,
            key_value,
            resolved_source,
        })
    }

    fn validate_canonical(&self) -> Result<()> {
        let canonical = Self::new(
            self.backend,
            self.key_kind,
            &self.key_value,
            &self.resolved_source,
        )?;
        if *self != canonical {
            return Err(LambError::Validation(
                "resolved live input identity is not canonical".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredInputIdentity {
    pub backend: InputBackend,
    pub selector: ConfiguredDeviceSelector,
    pub name: String,
    pub source: String,
    input_id: String,
}

impl ConfiguredInputIdentity {
    pub fn new(
        backend: InputBackend,
        selector: ConfiguredDeviceSelector,
        name: &str,
        source: &str,
    ) -> Result<Self> {
        let name = canonical_identity_field(name, "configured channel name")?;
        let source = canonical_identity_field(source, "configured source")?;
        let selector = match selector {
            ConfiguredDeviceSelector::PipeWireTarget(value) => {
                ConfiguredDeviceSelector::PipeWireTarget(canonical_identity_field(
                    &value,
                    "PipeWire target",
                )?)
            }
            ConfiguredDeviceSelector::PipeWireAuto => ConfiguredDeviceSelector::PipeWireAuto,
            ConfiguredDeviceSelector::JackSourceClient(value) => {
                ConfiguredDeviceSelector::JackSourceClient(canonical_identity_field(
                    &value,
                    "JACK source client",
                )?)
            }
        };
        match (&backend, &selector) {
            (InputBackend::PipeWire, ConfiguredDeviceSelector::PipeWireTarget(_))
            | (InputBackend::PipeWire, ConfiguredDeviceSelector::PipeWireAuto)
            | (InputBackend::Jack, ConfiguredDeviceSelector::JackSourceClient(_)) => {}
            _ => {
                return Err(LambError::Validation(
                    "configured device selector does not match backend".into(),
                ));
            }
        }
        if let ConfiguredDeviceSelector::JackSourceClient(client) = &selector {
            validate_jack_source(client, &source)?;
        }
        let mut hasher = Sha256::new();
        hasher.update(b"lamb/configured-input-identity/v2\0");
        hash_identity_field(
            &mut hasher,
            match backend {
                InputBackend::PipeWire => b"pipewire",
                InputBackend::Jack => b"jack",
            },
        )?;
        match &selector {
            ConfiguredDeviceSelector::PipeWireTarget(value) => {
                hash_identity_field(&mut hasher, b"pipewire-target")?;
                hash_identity_field(&mut hasher, value.as_bytes())?;
            }
            ConfiguredDeviceSelector::PipeWireAuto => {
                hash_identity_field(&mut hasher, b"pipewire-auto")?;
            }
            ConfiguredDeviceSelector::JackSourceClient(value) => {
                hash_identity_field(&mut hasher, b"jack-source-client")?;
                hash_identity_field(&mut hasher, value.as_bytes())?;
            }
        }
        hash_identity_field(&mut hasher, name.as_bytes())?;
        hash_identity_field(&mut hasher, source.as_bytes())?;
        Ok(Self {
            backend,
            selector,
            name,
            source,
            input_id: format!("{:x}", hasher.finalize()),
        })
    }

    pub fn input_id(&self) -> &str {
        &self.input_id
    }

    fn validate_canonical(&self) -> Result<()> {
        let exact_lowercase_sha256 = self.input_id.len() == 64
            && self
                .input_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let canonical = Self::new(
            self.backend,
            self.selector.clone(),
            &self.name,
            &self.source,
        )?;
        if !exact_lowercase_sha256 || *self != canonical {
            return Err(LambError::Validation(
                "configured input identity is not canonical".into(),
            ));
        }
        Ok(())
    }
}

fn canonical_identity_field(value: &str, label: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(LambError::Validation(format!("{label} must be non-empty")));
    }
    Ok(value.to_owned())
}

fn validate_jack_source(client: &str, source: &str) -> Result<()> {
    let mut components = source.split(':');
    let source_client = components.next().unwrap_or_default();
    let Some(port) = components.next() else {
        return Err(LambError::Validation(
            "JACK source must be a complete client:port".into(),
        ));
    };
    if source_client.is_empty()
        || port.is_empty()
        || components.next().is_some()
        || source_client != client
    {
        return Err(LambError::Validation(
            "JACK source must match its non-empty source client".into(),
        ));
    }
    Ok(())
}

fn hash_identity_field(hasher: &mut Sha256, value: &[u8]) -> Result<()> {
    let len = u64::try_from(value.len())
        .map_err(|_| LambError::Validation("identity field too long".into()))?;
    hasher.update(len.to_be_bytes());
    hasher.update(value);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LegacyInputIdentity {
    backend: String,
    device: String,
    name: String,
    source: String,
    input_id: String,
}

impl LegacyInputIdentity {
    fn new(backend: &str, device: &str, name: &str, source: &str) -> Result<Self> {
        let fields = [backend, device, name, source].map(|field| {
            let value = field.trim();
            if value.is_empty() {
                Err(LambError::Validation(
                    "stable input identity fields must be non-empty".into(),
                ))
            } else {
                Ok(value.to_owned())
            }
        });
        let [backend, device, name, source] = fields;
        let (backend, device, name, source) = (backend?, device?, name?, source?);
        let mut hasher = Sha256::new();
        hasher.update(b"lamb/stable-input-identity/v1\0");
        for field in [&backend, &device, &name, &source] {
            let len = u64::try_from(field.len())
                .map_err(|_| LambError::Validation("identity field too long".into()))?;
            hasher.update(len.to_be_bytes());
            hasher.update(field.as_bytes());
        }
        let input_id = format!("{:x}", hasher.finalize());
        Ok(Self {
            backend,
            device,
            name,
            source,
            input_id,
        })
    }
    fn input_id(&self) -> &str {
        &self.input_id
    }

    fn validate_canonical(&self) -> Result<()> {
        let exact_lowercase_sha256 = self.input_id.len() == 64
            && self
                .input_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let canonical = Self::new(&self.backend, &self.device, &self.name, &self.source)?;
        if !exact_lowercase_sha256 || *self != canonical {
            return Err(LambError::Validation(
                "stable input identity is not canonical".into(),
            ));
        }
        Ok(())
    }
}

pub fn state_root_from_env(
    xdg_state_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf> {
    let base = xdg_state_home
        .or_else(|| home.map(|home| home.join(".local/state")))
        .ok_or_else(|| {
            LambError::Config(
                "cannot determine calibration state root: set XDG_STATE_HOME or HOME".into(),
            )
        })?;
    if !base.is_absolute() {
        return Err(LambError::Config(
            "calibration state root must be absolute".into(),
        ));
    }
    Ok(base.join("lamb/calibration"))
}
pub fn default_state_root() -> Result<PathBuf> {
    state_root_from_env(
        env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
    )
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationMetadata {
    pub version: u32,
    pub calibration_id: String,
    #[serde(rename = "input", skip_serializing_if = "Option::is_none")]
    pub configured_input: Option<ConfiguredInputIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_live_input: Option<ResolvedLiveInputIdentity>,
    pub input_id: String,
    pub detector_version: String,
    pub threshold_dbfs: f32,
    pub p95_rms: f32,
    pub observed_peak: f32,
    pub complete_windows: u64,
    pub partial_final_frames: u64,
    pub dropped_frames: u64,
    pub frames: u64,
    pub sample_rate: u32,
    pub created_at_unix_seconds: u64,
    #[serde(skip)]
    legacy_input: Option<LegacyInputIdentity>,
}

#[derive(Deserialize)]
struct CalibrationMetadataWire {
    version: u32,
    calibration_id: String,
    input: serde_json::Value,
    #[serde(default)]
    resolved_live_input: Option<ResolvedLiveInputIdentity>,
    input_id: String,
    detector_version: String,
    threshold_dbfs: f32,
    p95_rms: f32,
    observed_peak: f32,
    complete_windows: u64,
    partial_final_frames: u64,
    dropped_frames: u64,
    frames: u64,
    sample_rate: u32,
    created_at_unix_seconds: u64,
}

impl<'de> Deserialize<'de> for CalibrationMetadata {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CalibrationMetadataWire::deserialize(deserializer)?;
        let (configured_input, legacy_input) = if wire.version == 1 {
            (
                None,
                Some(serde_json::from_value(wire.input).map_err(serde::de::Error::custom)?),
            )
        } else {
            (
                Some(serde_json::from_value(wire.input).map_err(serde::de::Error::custom)?),
                None,
            )
        };
        Ok(Self {
            version: wire.version,
            calibration_id: wire.calibration_id,
            configured_input,
            resolved_live_input: wire.resolved_live_input,
            input_id: wire.input_id,
            detector_version: wire.detector_version,
            threshold_dbfs: wire.threshold_dbfs,
            p95_rms: wire.p95_rms,
            observed_peak: wire.observed_peak,
            complete_windows: wire.complete_windows,
            partial_final_frames: wire.partial_final_frames,
            dropped_frames: wire.dropped_frames,
            frames: wire.frames,
            sample_rate: wire.sample_rate,
            created_at_unix_seconds: wire.created_at_unix_seconds,
            legacy_input,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StaleReason {
    MissingCalibrationId,
    MissingState,
    MissingMetadata,
    CorruptMetadata,
    MissingSample,
    CorruptSample,
    GenerationMismatch,
    DetectorMismatch,
    SampleRateMismatch,
    InputMismatch,
    MissingLiveIdentity,
    LiveIdentityMismatch,
    Expired,
    FutureTimestamp,
    IncoherentTimestamp,
    ThresholdMismatch,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibrationValidity {
    Valid,
    Stale(StaleReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalibrationArtifactStatus {
    Complete,
    Stale(StaleReason),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationArtifactInspection {
    pub status: CalibrationArtifactStatus,
    pub metadata: Option<CalibrationMetadata>,
}

/// Explicit, caller-owned failure-injection seam for durability tests.
/// Production callers use the no-op default through the convenience methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityCheckpoint {
    SampleWritten,
    SampleSynced,
    MetadataWritten,
    MetadataSynced,
    GenerationDirectorySynced,
    InputDirectorySynced,
    RootDirectorySynced,
    ConfigTempSynced,
    ConfigRenamed,
    ConfigParentSynced,
    /// A newly-created path component's parent has been durably published.
    /// This narrow test seam is intentionally local to calibration persistence.
    CreatedParentSynced,
}

pub type DurabilityHook<'a> = dyn FnMut(DurabilityCheckpoint) -> Result<()> + 'a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupCheckpoint {
    IdentityCaptured,
    /// The quarantined directory has been opened and identity-checked.
    QuarantineVerified,
}

pub type CleanupHook<'a> = dyn FnMut(&Path, CleanupCheckpoint) -> Result<()> + 'a;

pub fn derive_calibrated_threshold(stats: &mut [f32]) -> Result<f32> {
    if stats.is_empty() {
        return Err(LambError::Validation(
            "calibration requires complete RMS windows".into(),
        ));
    }
    if stats.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(LambError::Validation(
            "calibration RMS values must be finite and non-negative".into(),
        ));
    }
    stats.sort_by(|a, b| a.total_cmp(b));
    let rank = (95 * stats.len()).div_ceil(100) - 1;
    derived_threshold_from_amplitude(stats[rank])
}

fn derived_threshold_from_amplitude(amplitude: f32) -> Result<f32> {
    let dbfs = if amplitude <= 0.0 {
        -120.0
    } else {
        (20.0 * amplitude.log10()).max(-120.0)
    };
    let threshold_dbfs = dbfs + 10.0;
    if !threshold_dbfs.is_finite() || !(-120.0..=0.0).contains(&threshold_dbfs) {
        return Err(LambError::Validation(
            "derived calibration threshold is outside [-120, 0] dBFS".into(),
        ));
    }
    Ok(threshold_dbfs)
}

#[derive(Debug)]
pub struct CalibrationStore {
    root: PathBuf,
    maximum_frames: u64,
}
impl CalibrationStore {
    pub fn new(root: impl AsRef<Path>, maximum_frames: u64) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(LambError::Validation(
                "calibration store root must be absolute".into(),
            ));
        }
        if maximum_frames == 0 {
            return Err(LambError::Validation(
                "calibration store maximum frames must be nonzero".into(),
            ));
        }
        canonical_wav_data_bytes(maximum_frames)?;
        Ok(Self {
            root: root.to_path_buf(),
            maximum_frames,
        })
    }

    /// Inspects one generation from a caller-owned state root without creating
    /// a preparation-capable store with an invented capture-session capacity.
    pub fn inspect_root(
        root: impl AsRef<Path>,
        threshold: &ActivityThresholdConfig,
        input: &ConfiguredInputIdentity,
        now: u64,
    ) -> Result<CalibrationArtifactInspection> {
        // RIFF's 32-bit chunk length is the format-level upper bound. This is
        // used only by the associated inspection call and is never returned as
        // a store that could prepare a calibration sample.
        let maximum_frames = u64::from((u32::MAX - 36) / 4);
        Self::new(root, maximum_frames)?.inspect_offline(threshold, input, now)
    }

    /// Validates against live session facts using that session's real planned
    /// calibration capacity rather than a maintenance-only synthetic bound.
    pub fn validate_root(
        root: impl AsRef<Path>,
        maximum_frames: u64,
        threshold: &ActivityThresholdConfig,
        input: &ConfiguredInputIdentity,
        current_live_input: Option<&ResolvedLiveInputIdentity>,
        sample_rate: u32,
        now: u64,
    ) -> Result<CalibrationValidity> {
        Self::new(root, maximum_frames)?.validate(
            threshold,
            input,
            current_live_input,
            sample_rate,
            now,
        )
    }

    /// Reconciles generations beneath a caller-owned state root without a
    /// capture session or sample-sized allocation.
    pub fn cleanup_root(
        root: impl AsRef<Path>,
        referenced: &BTreeSet<(String, String)>,
    ) -> Result<Vec<PathBuf>> {
        Self::new(root, 1)?.cleanup_offline(referenced)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn maximum_frames(&self) -> u64 {
        self.maximum_frames
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_generated(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        samples: &[f32],
        sample_rate: u32,
        threshold_dbfs: f32,
        rms: &mut [f32],
        peak: &[f32],
        created_at_unix_seconds: u64,
    ) -> Result<PreparedCalibrationGeneration> {
        for _ in 0..1024 {
            let id = next_generation_id(created_at_unix_seconds)?;
            match self.prepare(
                input,
                live_input,
                &id,
                samples,
                sample_rate,
                threshold_dbfs,
                rms,
                peak,
                created_at_unix_seconds,
            ) {
                Err(LambError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                result => return result,
            }
        }
        Err(LambError::Config(
            "could not allocate a unique calibration generation id".into(),
        ))
    }
    pub fn prepare_generated_lease(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        lease: &mut CalibrationLease<'_>,
        threshold_dbfs: f32,
        created_at_unix_seconds: u64,
    ) -> Result<PreparedCalibrationGeneration> {
        self.prepare_generated_lease_with_hook(
            input,
            live_input,
            lease,
            threshold_dbfs,
            created_at_unix_seconds,
            &mut |_| Ok(()),
        )
    }
    pub fn prepare_generated_lease_with_hook(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        lease: &mut CalibrationLease<'_>,
        threshold_dbfs: f32,
        created_at_unix_seconds: u64,
        hook: &mut DurabilityHook<'_>,
    ) -> Result<PreparedCalibrationGeneration> {
        for _ in 0..1024 {
            let id = next_generation_id(created_at_unix_seconds)?;
            match self.prepare_lease_with_hook(
                input,
                live_input,
                &id,
                lease,
                threshold_dbfs,
                created_at_unix_seconds,
                hook,
            ) {
                Err(LambError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                result => return result,
            }
        }
        Err(LambError::Config(
            "could not allocate a unique calibration generation id".into(),
        ))
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        calibration_id: &str,
        samples: &[f32],
        sample_rate: u32,
        threshold_dbfs: f32,
        rms: &mut [f32],
        peak: &[f32],
        created_at_unix_seconds: u64,
    ) -> Result<PreparedCalibrationGeneration> {
        self.prepare_with_hook(
            input,
            live_input,
            calibration_id,
            samples,
            sample_rate,
            threshold_dbfs,
            rms,
            peak,
            created_at_unix_seconds,
            &mut |_| Ok(()),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_with_hook(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        calibration_id: &str,
        samples: &[f32],
        sample_rate: u32,
        threshold_dbfs: f32,
        rms: &mut [f32],
        peak: &[f32],
        created_at_unix_seconds: u64,
        hook: &mut DurabilityHook<'_>,
    ) -> Result<PreparedCalibrationGeneration> {
        self.prepare_captured_with_hook(
            input,
            live_input,
            calibration_id,
            samples,
            sample_rate,
            threshold_dbfs,
            rms,
            peak,
            created_at_unix_seconds,
            0,
            0,
            hook,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn prepare_captured_with_hook(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        calibration_id: &str,
        samples: &[f32],
        sample_rate: u32,
        threshold_dbfs: f32,
        rms: &mut [f32],
        peak: &[f32],
        created_at_unix_seconds: u64,
        partial_final_frames: u64,
        dropped_frames: u64,
        hook: &mut DurabilityHook<'_>,
    ) -> Result<PreparedCalibrationGeneration> {
        input.validate_canonical()?;
        live_input.validate_canonical()?;
        let frames = u64::try_from(samples.len())
            .map_err(|_| LambError::Validation("calibration sample count overflow".into()))?;
        if frames > self.maximum_frames {
            return Err(LambError::Validation(
                "calibration sample count exceeds store maximum".into(),
            ));
        }
        if !safe_id(calibration_id)
            || sample_rate == 0
            || samples.is_empty()
            || rms.is_empty()
            || rms.len() != peak.len()
            || !threshold_dbfs.is_finite()
            || !(-120.0..=0.0).contains(&threshold_dbfs)
        {
            return Err(LambError::Validation(
                "invalid calibration generation metadata".into(),
            ));
        }
        if samples.iter().any(|v| !v.is_finite())
            || rms.iter().any(|v| !v.is_finite() || *v < 0.0)
            || peak.iter().any(|v| !v.is_finite() || *v < 0.0)
        {
            return Err(LambError::Validation(
                "calibration sample/statistics must be finite".into(),
            ));
        }
        if rms.iter().zip(peak.iter()).any(|(rms, peak)| rms > peak) {
            return Err(LambError::Validation(
                "calibration RMS cannot exceed its corresponding peak".into(),
            ));
        }
        let sample_abs_peak = samples
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0, f32::max);
        let observed_peak = peak.iter().copied().fold(0.0f32, f32::max);
        if observed_peak > sample_abs_peak {
            return Err(LambError::Validation(
                "calibration complete-window peak exceeds retained samples".into(),
            ));
        }
        let derived_threshold = derive_calibrated_threshold(rms)?;
        if threshold_dbfs.to_bits() != derived_threshold.to_bits() {
            return Err(LambError::Validation(
                "calibration threshold does not match p95 RMS +10 dB".into(),
            ));
        }
        let p95 = rms[(95 * rms.len()).div_ceil(100) - 1];
        let root = open_or_create_directory(&self.root, hook)?;
        let input_dir = open_or_create_child(&root, input.input_id().as_ref(), &self.root, hook)?;
        let dir = self.root.join(input.input_id()).join(calibration_id);
        let generation_dir = create_new_directory_at(&input_dir, calibration_id.as_ref(), &dir)?;
        let mut prepared = PreparedCalibrationGeneration::new(
            dir.clone(),
            input.input_id().to_owned(),
            calibration_id.to_owned(),
            &input_dir,
        )?;
        let write_result = (|| {
            write_wav_at(
                &generation_dir,
                "sample.wav",
                &prepared.sample_path,
                samples,
                sample_rate,
                hook,
            )?;
            let metadata = CalibrationMetadata {
                version: 2,
                calibration_id: calibration_id.into(),
                configured_input: Some(input.clone()),
                resolved_live_input: Some(live_input.clone()),
                input_id: input.input_id().into(),
                detector_version: WINDOWED_RMS_PEAK_DETECTOR_VERSION.into(),
                threshold_dbfs,
                p95_rms: p95,
                observed_peak,
                complete_windows: rms.len() as u64,
                partial_final_frames,
                dropped_frames,
                frames,
                sample_rate,
                created_at_unix_seconds,
                legacy_input: None,
            };
            write_metadata_at(
                &generation_dir,
                "metadata.json",
                &prepared.metadata_path,
                &metadata,
                hook,
            )?;
            generation_dir.sync_all().map_err(|e| io_error(&dir, e))?;
            hook(DurabilityCheckpoint::GenerationDirectorySynced)?;
            input_dir
                .sync_all()
                .map_err(|e| io_error(self.root.join(input.input_id()), e))?;
            hook(DurabilityCheckpoint::InputDirectorySynced)?;
            root.sync_all().map_err(|e| io_error(&self.root, e))?;
            hook(DurabilityCheckpoint::RootDirectorySynced)?;
            verify_state_ancestry(
                &self.root,
                &root,
                input.input_id(),
                &input_dir,
                calibration_id,
                &generation_dir,
            )?;
            validate_generation(
                &generation_dir,
                &dir,
                input,
                live_input,
                calibration_id,
                Some(sample_rate),
                self.maximum_frames,
            )?;
            Ok(metadata)
        })();
        match write_result {
            Ok(metadata) => {
                prepared.metadata = metadata;
                Ok(prepared)
            }
            Err(error) => Err(error),
        }
    }
    pub fn prepare_lease(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        calibration_id: &str,
        lease: &mut CalibrationLease<'_>,
        threshold_dbfs: f32,
        created_at_unix_seconds: u64,
    ) -> Result<PreparedCalibrationGeneration> {
        self.prepare_lease_with_hook(
            input,
            live_input,
            calibration_id,
            lease,
            threshold_dbfs,
            created_at_unix_seconds,
            &mut |_| Ok(()),
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_lease_with_hook(
        &self,
        input: &ConfiguredInputIdentity,
        live_input: &ResolvedLiveInputIdentity,
        calibration_id: &str,
        lease: &mut CalibrationLease<'_>,
        threshold_dbfs: f32,
        created_at_unix_seconds: u64,
        hook: &mut DurabilityHook<'_>,
    ) -> Result<PreparedCalibrationGeneration> {
        let lease_metadata = lease.metadata();
        if !lease_metadata.usable
            || lease_metadata.dropped_frames != 0
            || lease_metadata.frames == 0
            || lease_metadata.frames > self.maximum_frames
            || lease_metadata.complete_windows == 0
            || lease_metadata.complete_windows > lease_metadata.frames
            || lease_metadata.sample_rate == 0
        {
            return Err(LambError::Validation(
                "calibration lease is not a complete usable capture".into(),
            ));
        }
        let sample_rate = lease_metadata.sample_rate;
        let (samples, rms, peak) = lease.persistence_parts_mut();
        self.prepare_captured_with_hook(
            input,
            live_input,
            calibration_id,
            samples,
            sample_rate,
            threshold_dbfs,
            rms,
            peak,
            created_at_unix_seconds,
            lease_metadata.partial_final_frames,
            lease_metadata.dropped_frames,
            hook,
        )
    }
    pub fn validate(
        &self,
        threshold: &ActivityThresholdConfig,
        input: &ConfiguredInputIdentity,
        current_live_input: Option<&ResolvedLiveInputIdentity>,
        sample_rate: u32,
        now: u64,
    ) -> Result<CalibrationValidity> {
        input.validate_canonical()?;
        if threshold.input_id != input.input_id() {
            return Ok(CalibrationValidity::Stale(StaleReason::InputMismatch));
        }
        if threshold.threshold_source == ThresholdSource::Manual {
            return Ok(CalibrationValidity::Valid);
        }
        if let Some(live_input) = current_live_input {
            live_input.validate_canonical()?;
        }
        let Some(id) = threshold
            .calibration_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Ok(CalibrationValidity::Stale(
                StaleReason::MissingCalibrationId,
            ));
        };
        if !safe_id(id) {
            return Ok(CalibrationValidity::Stale(StaleReason::GenerationMismatch));
        }
        let dir = self.root.join(input.input_id()).join(id);
        let generation = match open_generation(&self.root, input.input_id(), id) {
            Ok(dir) => dir,
            Err(_) => return Ok(CalibrationValidity::Stale(StaleReason::MissingState)),
        };
        let metadata =
            match read_metadata_at(&generation, "metadata.json", &dir.join("metadata.json")) {
                Ok(metadata) => metadata,
                Err(LambError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(CalibrationValidity::Stale(StaleReason::MissingMetadata));
                }
                Err(_) => return Ok(CalibrationValidity::Stale(StaleReason::CorruptMetadata)),
            };
        if metadata.frames > self.maximum_frames {
            return Ok(CalibrationValidity::Stale(StaleReason::CorruptMetadata));
        }
        if metadata.calibration_id != id {
            return Ok(CalibrationValidity::Stale(StaleReason::GenerationMismatch));
        }
        if metadata.version == 1 {
            return Ok(CalibrationValidity::Stale(StaleReason::MissingLiveIdentity));
        }
        if metadata.input_id != input.input_id()
            || metadata.configured_input.as_ref() != Some(input)
        {
            return Ok(CalibrationValidity::Stale(StaleReason::InputMismatch));
        }
        if metadata.resolved_live_input.is_none() || current_live_input.is_none() {
            return Ok(CalibrationValidity::Stale(StaleReason::MissingLiveIdentity));
        }
        if metadata.resolved_live_input.as_ref() != current_live_input {
            return Ok(CalibrationValidity::Stale(
                StaleReason::LiveIdentityMismatch,
            ));
        }
        if metadata.detector_version != WINDOWED_RMS_PEAK_DETECTOR_VERSION {
            return Ok(CalibrationValidity::Stale(StaleReason::DetectorMismatch));
        }
        if metadata.sample_rate != sample_rate {
            return Ok(CalibrationValidity::Stale(StaleReason::SampleRateMismatch));
        }
        if metadata.created_at_unix_seconds > now {
            return Ok(CalibrationValidity::Stale(StaleReason::FutureTimestamp));
        }
        if metadata.created_at_unix_seconds != threshold.updated_at_unix_seconds {
            return Ok(CalibrationValidity::Stale(StaleReason::IncoherentTimestamp));
        }
        if f64::from(metadata.threshold_dbfs) != threshold.threshold_dbfs {
            return Ok(CalibrationValidity::Stale(StaleReason::ThresholdMismatch));
        }
        if now - metadata.created_at_unix_seconds > STALE_SECONDS {
            return Ok(CalibrationValidity::Stale(StaleReason::Expired));
        }
        let sample_abs_peak = match validate_wav_at(
            &generation,
            "sample.wav",
            &dir.join("sample.wav"),
            metadata.sample_rate,
            metadata.frames,
        ) {
            Ok(sample_abs_peak) => sample_abs_peak,
            Err(LambError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CalibrationValidity::Stale(StaleReason::MissingSample));
            }
            Err(_) => return Ok(CalibrationValidity::Stale(StaleReason::CorruptSample)),
        };
        if metadata.observed_peak > sample_abs_peak {
            return Ok(CalibrationValidity::Stale(StaleReason::CorruptMetadata));
        }
        Ok(CalibrationValidity::Valid)
    }

    /// Inspects persisted calibration artifacts without making any claim about
    /// the current live device. All reads remain descriptor-relative and bounded.
    pub fn inspect_offline(
        &self,
        threshold: &ActivityThresholdConfig,
        input: &ConfiguredInputIdentity,
        now: u64,
    ) -> Result<CalibrationArtifactInspection> {
        input.validate_canonical()?;
        let stale = |reason, metadata| CalibrationArtifactInspection {
            status: CalibrationArtifactStatus::Stale(reason),
            metadata,
        };
        if threshold.input_id != input.input_id() {
            return Ok(stale(StaleReason::InputMismatch, None));
        }
        if threshold.threshold_source == ThresholdSource::Manual {
            return Ok(CalibrationArtifactInspection {
                status: CalibrationArtifactStatus::Complete,
                metadata: None,
            });
        }
        let Some(id) = threshold
            .calibration_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Ok(stale(StaleReason::MissingCalibrationId, None));
        };
        if !safe_id(id) {
            return Ok(stale(StaleReason::GenerationMismatch, None));
        }
        let dir = self.root.join(input.input_id()).join(id);
        let generation = match open_generation(&self.root, input.input_id(), id) {
            Ok(generation) => generation,
            Err(_) => return Ok(stale(StaleReason::MissingState, None)),
        };
        let metadata =
            match read_metadata_at(&generation, "metadata.json", &dir.join("metadata.json")) {
                Ok(metadata) => metadata,
                Err(LambError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(stale(StaleReason::MissingMetadata, None));
                }
                Err(_) => return Ok(stale(StaleReason::CorruptMetadata, None)),
            };
        if metadata.frames > self.maximum_frames {
            return Ok(stale(StaleReason::CorruptMetadata, Some(metadata)));
        }
        if metadata.calibration_id != id {
            return Ok(stale(StaleReason::GenerationMismatch, Some(metadata)));
        }
        if metadata.version == 1 {
            return Ok(stale(StaleReason::MissingLiveIdentity, Some(metadata)));
        }
        if metadata.input_id != input.input_id()
            || metadata.configured_input.as_ref() != Some(input)
        {
            return Ok(stale(StaleReason::InputMismatch, Some(metadata)));
        }
        if metadata.resolved_live_input.is_none() {
            return Ok(stale(StaleReason::MissingLiveIdentity, Some(metadata)));
        }
        if metadata.detector_version != WINDOWED_RMS_PEAK_DETECTOR_VERSION {
            return Ok(stale(StaleReason::DetectorMismatch, Some(metadata)));
        }
        if metadata.created_at_unix_seconds > now {
            return Ok(stale(StaleReason::FutureTimestamp, Some(metadata)));
        }
        if metadata.created_at_unix_seconds != threshold.updated_at_unix_seconds {
            return Ok(stale(StaleReason::IncoherentTimestamp, Some(metadata)));
        }
        if f64::from(metadata.threshold_dbfs) != threshold.threshold_dbfs {
            return Ok(stale(StaleReason::ThresholdMismatch, Some(metadata)));
        }
        if now - metadata.created_at_unix_seconds > STALE_SECONDS {
            return Ok(stale(StaleReason::Expired, Some(metadata)));
        }
        let sample_abs_peak = match validate_wav_at(
            &generation,
            "sample.wav",
            &dir.join("sample.wav"),
            metadata.sample_rate,
            metadata.frames,
        ) {
            Ok(sample_abs_peak) => sample_abs_peak,
            Err(LambError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(stale(StaleReason::MissingSample, Some(metadata)));
            }
            Err(_) => return Ok(stale(StaleReason::CorruptSample, Some(metadata))),
        };
        if metadata.observed_peak > sample_abs_peak {
            return Ok(stale(StaleReason::CorruptMetadata, Some(metadata)));
        }
        Ok(CalibrationArtifactInspection {
            status: CalibrationArtifactStatus::Complete,
            metadata: Some(metadata),
        })
    }

    /// Descriptor-safe cleanup callable during startup/reset without a capture session.
    pub fn cleanup_offline(&self, referenced: &BTreeSet<(String, String)>) -> Result<Vec<PathBuf>> {
        self.cleanup_unreferenced(referenced)
    }

    /// Startup maintenance for generations which are not referenced by the
    /// loaded configuration.  Each removal is guarded by the directory's
    /// observed device/inode so a replacement is left for manual recovery.
    pub fn cleanup_unreferenced(
        &self,
        referenced: &BTreeSet<(String, String)>,
    ) -> Result<Vec<PathBuf>> {
        self.cleanup_unreferenced_with_hook(referenced, &mut |_, _| Ok(()))
    }

    pub fn cleanup_unreferenced_with_hook(
        &self,
        referenced: &BTreeSet<(String, String)>,
        hook: &mut CleanupHook<'_>,
    ) -> Result<Vec<PathBuf>> {
        let mut pending = Vec::new();
        let root = match open_existing_directory(&self.root) {
            Ok(root) => root,
            Err(LambError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(pending)
            }
            Err(error) => return Err(error),
        };
        for input_name in directory_entry_names(&root, &self.root)? {
            let input_id = input_name.to_string_lossy().to_string();
            let input_path = self.root.join(&input_name);
            let input = match open_directory_at(&root, &input_name) {
                Ok(input) => input,
                Err(_) => continue,
            };
            for generation_name in directory_entry_names(&input, &input_path)? {
                let path = input_path.join(&generation_name);
                let id = generation_name.to_string_lossy().to_string();
                if referenced.contains(&(input_id.clone(), id)) {
                    continue;
                }
                let generation = match open_directory_at(&input, &generation_name) {
                    Ok(generation) => generation,
                    Err(_) => {
                        pending.push(path);
                        continue;
                    }
                };
                let metadata = generation
                    .metadata()
                    .map_err(|error| io_error(&path, error))?;
                let identity = RecordedGeneration {
                    path: path.clone(),
                    parent: input
                        .try_clone()
                        .map_err(|error| io_error(&input_path, error))?,
                    name: generation_name,
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                };
                if !remove_recorded_generation(&identity, hook)? {
                    pending.push(path);
                }
            }
        }
        Ok(pending)
    }
}

pub struct PreparedCalibrationGeneration {
    dir: PathBuf,
    input_id: String,
    calibration_id: String,
    sample_path: PathBuf,
    metadata_path: PathBuf,
    metadata: CalibrationMetadata,
    dev: u64,
    ino: u64,
    input_dir: File,
    authoritative: bool,
}
impl PreparedCalibrationGeneration {
    fn new(
        dir: PathBuf,
        input_id: String,
        calibration_id: String,
        input_dir: &File,
    ) -> Result<Self> {
        let generation =
            open_directory_at(input_dir, calibration_id.as_ref()).map_err(|e| io_error(&dir, e))?;
        let m = generation.metadata().map_err(|e| io_error(&dir, e))?;
        Ok(Self {
            sample_path: dir.join("sample.wav"),
            metadata_path: dir.join("metadata.json"),
            dir: dir.clone(),
            input_id,
            calibration_id,
            metadata: dummy_metadata(),
            dev: m.dev(),
            ino: m.ino(),
            input_dir: input_dir.try_clone().map_err(|e| io_error(&dir, e))?,
            authoritative: false,
        })
    }
    pub fn calibration_id(&self) -> &str {
        &self.calibration_id
    }
    pub fn input_id(&self) -> &str {
        &self.input_id
    }
    pub fn path(&self) -> &Path {
        &self.dir
    }
    pub fn sample_path(&self) -> &Path {
        &self.sample_path
    }
    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }
    pub fn metadata(&self) -> &CalibrationMetadata {
        &self.metadata
    }
    pub fn mark_authoritative(&mut self) {
        self.authoritative = true;
    }
    pub fn cleanup(&mut self) -> Result<bool> {
        self.cleanup_with_hook(&mut |_, _| Ok(()))
    }
    pub fn cleanup_with_hook(&mut self, hook: &mut CleanupHook<'_>) -> Result<bool> {
        if self.authoritative {
            return Ok(false);
        }
        remove_generation_at(
            &self.input_dir,
            &self.calibration_id,
            &self.dir,
            (self.dev, self.ino),
            hook,
        )
    }
}
impl Drop for PreparedCalibrationGeneration {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldGenerationCleanup {
    NotRequested,
    Removed,
    Pending(PathBuf),
}

#[derive(Debug)]
pub struct RecordedGeneration {
    path: PathBuf,
    parent: File,
    name: std::ffi::OsString,
    dev: u64,
    ino: u64,
}

impl RecordedGeneration {
    pub fn capture(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let parent_path = path
            .parent()
            .ok_or_else(|| LambError::Validation("calibration generation has no parent".into()))?;
        let name = normal_file_name(&path)?.to_os_string();
        let parent = open_existing_directory(parent_path)?;
        let directory =
            open_directory_at(&parent, &name).map_err(|error| io_error(&path, error))?;
        let metadata = directory
            .metadata()
            .map_err(|error| io_error(&path, error))?;
        if !metadata.file_type().is_dir() {
            return Err(LambError::Validation(
                "calibration generation is not a directory".into(),
            ));
        }
        Ok(Self {
            path,
            parent,
            name,
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

fn remove_recorded_generation(
    recorded: &RecordedGeneration,
    hook: &mut CleanupHook<'_>,
) -> Result<bool> {
    remove_generation_at(
        &recorded.parent,
        recorded.name.to_string_lossy().as_ref(),
        &recorded.path,
        (recorded.dev, recorded.ino),
        hook,
    )
}

fn remove_generation_at(
    parent: &File,
    name: &str,
    display: &Path,
    expected: (u64, u64),
    hook: &mut CleanupHook<'_>,
) -> Result<bool> {
    let current = match open_directory_at(parent, name.as_ref()) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Ok(false),
    };
    let m = current.metadata().map_err(|e| io_error(display, e))?;
    if (m.dev(), m.ino()) != expected {
        return Ok(false);
    }
    hook(display, CleanupCheckpoint::IdentityCaptured)?;
    let current = match open_directory_at(parent, name.as_ref()) {
        Ok(dir) => dir,
        Err(_) => return Ok(false),
    };
    let current_m = current.metadata().map_err(|e| io_error(display, e))?;
    if (current_m.dev(), current_m.ino()) != expected {
        return Ok(false);
    }
    let quarantine = match unique_adjacent(display, "cleanup") {
        Ok(path) => path,
        Err(_) => return Ok(false),
    };
    let Some(quarantine_name) = quarantine.file_name() else {
        return Ok(false);
    };
    if rename_no_replace_or_exchange_at(parent, name.as_ref(), quarantine_name, display, false)
        .is_err()
    {
        return Ok(false);
    }
    let moved = match open_directory_at(parent, quarantine_name) {
        Ok(dir) => dir,
        Err(_) => return Ok(false),
    };
    let moved_m = moved.metadata().map_err(|e| io_error(display, e))?;
    if (moved_m.dev(), moved_m.ino()) != expected {
        return Ok(false);
    }
    hook(&quarantine, CleanupCheckpoint::QuarantineVerified)?;
    let verified = match open_directory_at(parent, quarantine_name) {
        Ok(dir) => dir,
        Err(_) => return Ok(false),
    };
    let verified_m = verified.metadata().map_err(|e| io_error(display, e))?;
    if (verified_m.dev(), verified_m.ino()) != expected {
        return Ok(false);
    }
    let mut budget = 4096;
    if clear_directory_at(&verified, display, 0, &mut budget).is_err() {
        return Ok(false);
    }
    let final_m = match open_directory_at(parent, quarantine_name).and_then(|dir| dir.metadata()) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    if (final_m.dev(), final_m.ino()) != expected {
        return Ok(false);
    }
    let q = c_name(quarantine_name).map_err(|e| io_error(display, e))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), q.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Ok(false);
    }
    let _ = parent.sync_all();
    Ok(true)
}

fn clear_directory_at(
    directory: &File,
    display: &Path,
    depth: usize,
    budget: &mut usize,
) -> Result<()> {
    if depth >= 64 || *budget == 0 {
        return Err(LambError::Config(
            "calibration cleanup tree exceeds safe bounds".into(),
        ));
    }
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .map_err(|e| io_error(display, e))?
    {
        if *budget == 0 {
            return Err(LambError::Config(
                "calibration cleanup tree exceeds safe bounds".into(),
            ));
        }
        *budget -= 1;
        let name = entry.map_err(|e| io_error(display, e))?.file_name();
        let Some(metadata) = metadata_at(directory, &name, display)? else {
            return Err(LambError::Config(
                "calibration cleanup entry disappeared".into(),
            ));
        };
        let name_c = c_name(&name).map_err(|e| io_error(display, e))?;
        if metadata.mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_directory_at(directory, &name).map_err(|e| io_error(display, e))?;
            clear_directory_at(&child, display, depth + 1, budget)?;
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(io_error(display, std::io::Error::last_os_error()));
            }
        } else if unsafe { libc::unlinkat(directory.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(io_error(display, std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn directory_entry_names(directory: &File, display: &Path) -> Result<Vec<std::ffi::OsString>> {
    fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))
        .map_err(|e| io_error(display, e))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|e| io_error(display, e))
        })
        .collect()
}

/// The narrow state/config transaction seam used by the daemon layer.  The
/// installer is intentionally infallible: durable configuration is the commit
/// boundary, and live policy installation can never run ahead of it.
pub fn commit_prepared_generation(
    config_path: &Path,
    candidate: &AppConfig,
    prepared: &mut PreparedCalibrationGeneration,
    previous_generation: Option<RecordedGeneration>,
    install: impl FnOnce(),
) -> Result<OldGenerationCleanup> {
    commit_prepared_generation_with_hooks(
        config_path,
        candidate,
        prepared,
        previous_generation,
        install,
        &mut |_| Ok(()),
        &mut |_, _| Ok(()),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSaveFailureOutcome {
    DefinitelyNotPublished,
    NotEstablished,
}

fn config_save_failure_outcome(error: &LambError) -> ConfigSaveFailureOutcome {
    match error {
        LambError::IndeterminatePublication { .. } | LambError::PersistenceCleanup { .. } => {
            ConfigSaveFailureOutcome::NotEstablished
        }
        _ => ConfigSaveFailureOutcome::DefinitelyNotPublished,
    }
}

fn cleanup_failed_precommit(
    operation: LambError,
    prepared: &mut PreparedCalibrationGeneration,
    cleanup_hook: &mut CleanupHook<'_>,
) -> LambError {
    let cleanup = match prepared.cleanup_with_hook(cleanup_hook) {
        Ok(true) => return operation,
        Ok(false) => LambError::UnidentifiedStagingCleanup {
            path: prepared.path().to_path_buf(),
        },
        Err(error) => error,
    };
    LambError::PersistenceCleanup {
        operation: Box::new(operation),
        cleanup: Box::new(cleanup),
    }
}

pub fn commit_prepared_generation_with_hooks(
    config_path: &Path,
    candidate: &AppConfig,
    prepared: &mut PreparedCalibrationGeneration,
    previous_generation: Option<RecordedGeneration>,
    install: impl FnOnce(),
    config_hook: &mut DurabilityHook<'_>,
    cleanup_hook: &mut CleanupHook<'_>,
) -> Result<OldGenerationCleanup> {
    if previous_generation
        .as_ref()
        .is_some_and(|old| old.dev == prepared.dev && old.ino == prepared.ino)
    {
        return Err(LambError::Validation(
            "previous generation aliases the prepared generation".into(),
        ));
    }

    let candidate_validation = (|| {
        let (profile_name, profile) = candidate_profile_referencing_prepared(candidate, prepared)
            .ok_or_else(|| {
            LambError::Validation(
                "candidate does not reference exactly the prepared calibrated generation".into(),
            )
        })?;
        crate::profile::validate_profile(profile_name, profile)
    })();
    if let Err(operation) = candidate_validation {
        return Err(cleanup_failed_precommit(operation, prepared, cleanup_hook));
    }

    if let Err(operation) = save_config_atomic_with_hook(config_path, candidate, config_hook) {
        if config_save_failure_outcome(&operation) == ConfigSaveFailureOutcome::NotEstablished {
            // The candidate may still be installed and reference this generation. Preserve the
            // orphan so startup reconciliation can inspect the durable config and remove it safely.
            prepared.mark_authoritative();
            return Err(operation);
        }
        return Err(cleanup_failed_precommit(operation, prepared, cleanup_hook));
    }

    prepared.mark_authoritative();
    install();
    let Some(old) = previous_generation else {
        return Ok(OldGenerationCleanup::NotRequested);
    };
    if remove_recorded_generation(&old, cleanup_hook).unwrap_or(false) {
        Ok(OldGenerationCleanup::Removed)
    } else {
        Ok(OldGenerationCleanup::Pending(old.path))
    }
}

fn candidate_profile_referencing_prepared<'a>(
    candidate: &'a AppConfig,
    prepared: &PreparedCalibrationGeneration,
) -> Option<(&'a str, &'a crate::app_config::ProfileConfig)> {
    let mut matches = candidate.profiles.iter().filter(|(_, profile)| {
        profile
            .channels
            .values()
            .filter_map(|channel| channel.activity.as_ref())
            .any(|activity| {
                activity.threshold_source == ThresholdSource::Calibrated
                    && activity.input_id == prepared.metadata.input_id
                    && activity.calibration_id.as_deref() == Some(prepared.calibration_id())
                    && activity.updated_at_unix_seconds == prepared.metadata.created_at_unix_seconds
                    && activity.threshold_dbfs == f64::from(prepared.metadata.threshold_dbfs)
            })
    });
    let (name, profile) = matches.next()?;
    if matches.next().is_some()
        || profile
            .channels
            .values()
            .filter_map(|channel| channel.activity.as_ref())
            .filter(|activity| {
                activity.threshold_source == ThresholdSource::Calibrated
                    && activity.input_id == prepared.metadata.input_id
                    && activity.calibration_id.as_deref() == Some(prepared.calibration_id())
                    && activity.updated_at_unix_seconds == prepared.metadata.created_at_unix_seconds
                    && activity.threshold_dbfs == f64::from(prepared.metadata.threshold_dbfs)
            })
            .count()
            != 1
    {
        return None;
    }
    Some((name, profile))
}

pub fn save_config_atomic(path: &Path, candidate: &AppConfig) -> Result<()> {
    save_config_atomic_with_hook(path, candidate, &mut |_| Ok(()))
}

pub fn save_config_atomic_with_hook(
    path: &Path,
    candidate: &AppConfig,
    hook: &mut DurabilityHook<'_>,
) -> Result<()> {
    save_config_atomic_with_hook_and_name_source(path, candidate, hook, &mut || {
        NEXT_TEMP
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| LambError::Config("config temporary-name counter exhausted".into()))
    })
}

/// Deterministic naming seam for collision and publication tests.
#[doc(hidden)]
pub fn save_config_atomic_with_hook_and_name_source(
    path: &Path,
    candidate: &AppConfig,
    hook: &mut DurabilityHook<'_>,
    name_source: &mut dyn FnMut() -> Result<u64>,
) -> Result<()> {
    crate::app_config::validate_persisted_config(candidate)?;
    let mut text = toml::to_string_pretty(candidate)
        .map_err(|e| LambError::Config(format!("failed to serialize app config: {e}")))?;
    while text.ends_with('\n') {
        text.pop();
    }
    text.push('\n');
    let round_trip: AppConfig = toml::from_str(&text)
        .map_err(|e| LambError::Config(format!("failed to round-trip app config: {e}")))?;
    if &round_trip != candidate {
        return Err(LambError::Config(
            "serialized app config did not round-trip exactly".into(),
        ));
    }

    let parent_path = path
        .parent()
        .ok_or_else(|| LambError::Config("config path has no parent".into()))?;
    let target_name = normal_file_name(path)?;
    let parent = open_or_create_directory(parent_path, hook)?;
    let parent_identity = directory_identity(&parent, parent_path)?;
    let previous = capture_config_target_at(&parent, target_name, path)?;
    let (temp_name, temp, mut file) =
        create_unique_adjacent_at(&parent, target_name, "config", path, name_source)?;
    let new_identity = file_identity_at(&parent, &temp_name, &temp)?;

    let prepare = (|| {
        if let Some(previous) = previous {
            let result = unsafe {
                libc::fchown(
                    file.as_raw_fd(),
                    previous.uid as libc::uid_t,
                    previous.gid as libc::gid_t,
                )
            };
            if result != 0 {
                return Err(io_error(&temp, std::io::Error::last_os_error()));
            }
        }
        file.write_all(text.as_bytes())
            .map_err(|e| io_error(&temp, e))?;
        file.set_permissions(fs::Permissions::from_mode(
            previous.map_or(0o600, |target| target.mode),
        ))
        .map_err(|e| io_error(&temp, e))?;
        file.sync_all().map_err(|e| io_error(&temp, e))?;
        hook(DurabilityCheckpoint::ConfigTempSynced)
    })();
    drop(file);
    if let Err(operation) = prepare {
        return cleanup_owned_error_at(operation, &parent, &temp_name, &temp, new_identity);
    }

    if !parent_path_matches(parent_path, parent_identity)
        || !target_matches_at(&parent, target_name, path, previous)?
    {
        return cleanup_owned_error_at(
            LambError::Config("config target changed before atomic installation".into()),
            &parent,
            &temp_name,
            &temp,
            new_identity,
        );
    }

    if let Some(previous) = previous {
        if let Err(operation) =
            rename_no_replace_or_exchange_at(&parent, &temp_name, target_name, path, true)
        {
            return cleanup_owned_error_at(operation, &parent, &temp_name, &temp, new_identity);
        }
        let installed_is_new =
            file_identity_at(&parent, target_name, path).ok() == Some(new_identity);
        let recovery_is_old =
            file_identity_at(&parent, &temp_name, &temp).ok() == Some(previous.identity());
        if !installed_is_new || !recovery_is_old {
            return Err(LambError::IndeterminatePublication {
                operation: Box::new(LambError::Config(
                    "config identities changed during atomic exchange".into(),
                )),
            });
        }

        let publication = hook(DurabilityCheckpoint::ConfigRenamed)
            .and_then(|_| {
                if !parent_path_matches(parent_path, parent_identity) {
                    return Err(LambError::Config(
                        "config parent changed after atomic installation".into(),
                    ));
                }
                parent.sync_all().map_err(|e| io_error(parent_path, e))
            })
            .and_then(|_| hook(DurabilityCheckpoint::ConfigParentSynced));
        if let Err(operation) = publication {
            if !parent_path_matches(parent_path, parent_identity)
                || file_identity_at(&parent, target_name, path).ok() != Some(new_identity)
                || file_identity_at(&parent, &temp_name, &temp).ok() != Some(previous.identity())
            {
                return Err(LambError::IndeterminatePublication {
                    operation: Box::new(operation),
                });
            }
            if rename_no_replace_or_exchange_at(&parent, &temp_name, target_name, path, true)
                .is_err()
                || file_identity_at(&parent, target_name, path).ok() != Some(previous.identity())
                || file_identity_at(&parent, &temp_name, &temp).ok() != Some(new_identity)
                || parent.sync_all().is_err()
            {
                return Err(LambError::IndeterminatePublication {
                    operation: Box::new(operation),
                });
            }
            match remove_if_identity_at(&parent, &temp_name, &temp, new_identity)
                .and_then(|_| parent.sync_all().map_err(|e| io_error(parent_path, e)))
            {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(LambError::PersistenceCleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            }
        } else {
            // Publication is durable. Old-target cleanup is best effort and must
            // not cause the caller to skip installing the live candidate.
            if remove_if_identity_at(&parent, &temp_name, &temp, previous.identity()).is_ok() {
                let _ = parent.sync_all();
            }
            Ok(())
        }
    } else {
        if let Err(operation) =
            rename_no_replace_or_exchange_at(&parent, &temp_name, target_name, path, false)
        {
            return cleanup_owned_error_at(operation, &parent, &temp_name, &temp, new_identity);
        }
        if file_identity_at(&parent, target_name, path).ok() != Some(new_identity) {
            return Err(LambError::IndeterminatePublication {
                operation: Box::new(LambError::Config(
                    "installed config identity cannot be established".into(),
                )),
            });
        }
        let publication = hook(DurabilityCheckpoint::ConfigRenamed)
            .and_then(|_| {
                if !parent_path_matches(parent_path, parent_identity) {
                    return Err(LambError::Config(
                        "config parent changed after atomic installation".into(),
                    ));
                }
                parent.sync_all().map_err(|e| io_error(parent_path, e))
            })
            .and_then(|_| hook(DurabilityCheckpoint::ConfigParentSynced));
        if let Err(operation) = publication {
            if !parent_path_matches(parent_path, parent_identity)
                || file_identity_at(&parent, target_name, path).ok() != Some(new_identity)
            {
                return Err(LambError::IndeterminatePublication {
                    operation: Box::new(operation),
                });
            }
            match remove_if_identity_at(&parent, target_name, path, new_identity)
                .and_then(|_| parent.sync_all().map_err(|e| io_error(parent_path, e)))
            {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(LambError::PersistenceCleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            }
        } else {
            Ok(())
        }
    }
}

fn next_generation_id(created_at_unix_seconds: u64) -> Result<String> {
    let sequence = NEXT_GENERATION
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| LambError::Config("calibration generation counter exhausted".into()))?;
    Ok(format!(
        "cal-{}-{}-{sequence}",
        created_at_unix_seconds,
        std::process::id()
    ))
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
#[allow(dead_code)]
fn write_metadata(
    path: &Path,
    metadata: &CalibrationMetadata,
    hook: &mut DurabilityHook<'_>,
) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    serde_json::to_writer_pretty(&mut f, metadata)
        .map_err(|e| LambError::Config(format!("failed to serialize calibration metadata: {e}")))?;
    f.write_all(b"\n").map_err(|e| io_error(path, e))?;
    hook(DurabilityCheckpoint::MetadataWritten)?;
    f.sync_all().map_err(|e| io_error(path, e))?;
    hook(DurabilityCheckpoint::MetadataSynced)
}
fn canonical_wav_data_bytes(frames: u64) -> Result<u32> {
    let data = frames
        .checked_mul(4)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| LambError::Validation("WAV data too large".into()))?;
    36u32
        .checked_add(data)
        .ok_or_else(|| LambError::Validation("WAV size overflow".into()))?;
    Ok(data)
}

fn write_wav(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
    hook: &mut DurabilityHook<'_>,
) -> Result<()> {
    let data = u32::try_from(
        samples
            .len()
            .checked_mul(4)
            .ok_or_else(|| LambError::Validation("WAV data too large".into()))?,
    )
    .map_err(|_| LambError::Validation("WAV data too large".into()))?;
    let bytes_per_second = sample_rate
        .checked_mul(4)
        .ok_or_else(|| LambError::Validation("WAV rate overflow".into()))?;
    let riff = 36u32
        .checked_add(data)
        .ok_or_else(|| LambError::Validation("WAV size overflow".into()))?;
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    let mut h = [0u8; WAV_HEADER_BYTES];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&riff.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&bytes_per_second.to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    f.write_all(&h).map_err(|e| io_error(path, e))?;
    for sample in samples {
        f.write_all(&sample.to_le_bytes())
            .map_err(|e| io_error(path, e))?;
    }
    hook(DurabilityCheckpoint::SampleWritten)?;
    f.sync_all().map_err(|e| io_error(path, e))?;
    hook(DurabilityCheckpoint::SampleSynced)
}
fn validate_generation(
    generation: &File,
    dir: &Path,
    input: &ConfiguredInputIdentity,
    live_input: &ResolvedLiveInputIdentity,
    id: &str,
    sample_rate: Option<u32>,
    maximum_frames: u64,
) -> Result<CalibrationMetadata> {
    let metadata = read_metadata_at(generation, "metadata.json", &dir.join("metadata.json"))?;
    if metadata.frames > maximum_frames
        || metadata.version != 2
        || metadata.calibration_id != id
        || metadata.input_id != input.input_id()
        || metadata.configured_input.as_ref() != Some(input)
        || metadata.resolved_live_input.as_ref() != Some(live_input)
        || sample_rate.is_some_and(|rate| rate != metadata.sample_rate)
    {
        return Err(LambError::Validation(
            "calibration metadata does not match configured and live input".into(),
        ));
    }
    let wav = dir.join("sample.wav");
    let sample_abs_peak = validate_wav_at(
        generation,
        "sample.wav",
        &wav,
        metadata.sample_rate,
        metadata.frames,
    )?;
    if metadata.observed_peak > sample_abs_peak {
        return Err(LambError::Validation(
            "calibration observed peak exceeds retained samples".into(),
        ));
    }
    Ok(metadata)
}
fn read_metadata(path: &Path) -> Result<CalibrationMetadata> {
    const MAX_METADATA_BYTES: u64 = 64 * 1024;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    let file_metadata = file.metadata().map_err(|e| io_error(path, e))?;
    if !file_metadata.file_type().is_file() {
        return Err(LambError::Validation(
            "calibration metadata is not a regular file".into(),
        ));
    }
    let len = file_metadata.len();
    if len == 0 || len > MAX_METADATA_BYTES {
        return Err(LambError::Validation(
            "invalid calibration metadata size".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(path, e))?;
    if bytes.len() as u64 != len || bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(LambError::Validation(
            "calibration metadata changed while reading".into(),
        ));
    }
    let metadata: CalibrationMetadata = serde_json::from_slice(&bytes)
        .map_err(|e| LambError::Config(format!("invalid calibration metadata: {e}")))?;
    let canonical_identity = match metadata.version {
        1 => {
            let legacy = metadata.legacy_input.as_ref().ok_or_else(|| {
                LambError::Validation("metadata v1 lacks its legacy input identity".into())
            })?;
            legacy.validate_canonical()?;
            metadata.input_id == legacy.input_id()
        }
        2 => {
            let input = metadata.configured_input.as_ref().ok_or_else(|| {
                LambError::Validation("metadata v2 lacks configured input identity".into())
            })?;
            input.validate_canonical()?;
            if let Some(live_input) = metadata.resolved_live_input.as_ref() {
                live_input.validate_canonical()?;
            }
            metadata.input_id == input.input_id()
        }
        _ => false,
    };
    if !canonical_identity
        || !safe_id(&metadata.calibration_id)
        || metadata.detector_version.is_empty()
        || metadata.detector_version.len() > 128
        || metadata.input_id.len() != 64
        || !metadata
            .input_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || metadata.sample_rate == 0
        || metadata.frames == 0
        || metadata.complete_windows == 0
        || metadata.complete_windows > metadata.frames
        || metadata.partial_final_frames > metadata.frames
        || metadata.dropped_frames != 0
        || !metadata.threshold_dbfs.is_finite()
        || !(-120.0..=0.0).contains(&metadata.threshold_dbfs)
        || !metadata.p95_rms.is_finite()
        || metadata.p95_rms < 0.0
        || !metadata.observed_peak.is_finite()
        || metadata.observed_peak < 0.0
        || metadata.p95_rms > metadata.observed_peak
        || derived_threshold_from_amplitude(metadata.p95_rms).map_or(true, |threshold| {
            threshold.to_bits() != metadata.threshold_dbfs.to_bits()
        })
    {
        return Err(LambError::Validation(
            "invalid calibration metadata values".into(),
        ));
    }
    Ok(metadata)
}
fn validate_wav(path: &Path, rate: u32, frames: u64) -> Result<f32> {
    let mut h = [0; WAV_HEADER_BYTES];
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io_error(path, e))?;
    let file_metadata = file.metadata().map_err(|e| io_error(path, e))?;
    if !file_metadata.file_type().is_file() {
        return Err(LambError::Validation(
            "calibration WAV is not a regular file".into(),
        ));
    }
    file.read_exact(&mut h).map_err(|e| io_error(path, e))?;
    let expected_data = frames
        .checked_mul(4)
        .ok_or_else(|| LambError::Validation("WAV frame overflow".into()))?;
    let expected_data_u32 = u32::try_from(expected_data)
        .map_err(|_| LambError::Validation("WAV data too large".into()))?;
    let expected_riff = 36u32
        .checked_add(expected_data_u32)
        .ok_or_else(|| LambError::Validation("WAV size overflow".into()))?;
    let expected_byte_rate = rate
        .checked_mul(4)
        .ok_or_else(|| LambError::Validation("WAV rate overflow".into()))?;
    let expected_file_len = expected_data
        .checked_add(WAV_HEADER_BYTES as u64)
        .ok_or_else(|| LambError::Validation("WAV length overflow".into()))?;
    if &h[0..4] != b"RIFF"
        || u32::from_le_bytes([h[4], h[5], h[6], h[7]]) != expected_riff
        || &h[8..12] != b"WAVE"
        || &h[12..16] != b"fmt "
        || u32::from_le_bytes([h[16], h[17], h[18], h[19]]) != 16
        || u16::from_le_bytes([h[20], h[21]]) != 3
        || u16::from_le_bytes([h[22], h[23]]) != 1
        || u32::from_le_bytes([h[24], h[25], h[26], h[27]]) != rate
        || u32::from_le_bytes([h[28], h[29], h[30], h[31]]) != expected_byte_rate
        || u16::from_le_bytes([h[32], h[33]]) != 4
        || u16::from_le_bytes([h[34], h[35]]) != 32
        || &h[36..40] != b"data"
        || u32::from_le_bytes([h[40], h[41], h[42], h[43]]) != expected_data_u32
        || file_metadata.len() != expected_file_len
    {
        return Err(LambError::Validation(
            "invalid calibration WAV geometry".into(),
        ));
    }
    let mut sample_abs_peak = 0.0f32;
    let mut remaining = expected_data;
    let mut payload = [0u8; 4096];
    while remaining != 0 {
        let chunk_len = usize::try_from(remaining.min(payload.len() as u64))
            .map_err(|_| LambError::Validation("WAV payload length overflow".into()))?;
        file.read_exact(&mut payload[..chunk_len])
            .map_err(|e| io_error(path, e))?;
        for bytes in payload[..chunk_len].as_chunks::<4>().0 {
            let sample = f32::from_le_bytes(*bytes);
            if !sample.is_finite() {
                return Err(LambError::Validation(
                    "calibration WAV contains a non-finite sample".into(),
                ));
            }
            sample_abs_peak = sample_abs_peak.max(sample.abs());
        }
        remaining -= chunk_len as u64;
    }
    Ok(sample_abs_peak)
}
#[derive(Clone, Copy)]
struct ConfigTarget {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

impl ConfigTarget {
    fn identity(self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

fn c_name(name: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    let path = Path::new(name);
    let mut components = path.components();
    let is_single_normal = matches!(components.next(), Some(Component::Normal(component)) if component == name)
        && components.next().is_none();
    if !is_single_normal {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "descriptor-relative child name must be exactly one normal path component",
        ));
    }
    std::ffi::CString::new(name.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))
}

fn open_directory_at(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_absolute_root(path: &Path) -> Result<File> {
    let root = std::ffi::CString::new("/").expect("root has no NUL");
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io_error(path, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_or_create_directory(path: &Path, hook: &mut DurabilityHook<'_>) -> Result<File> {
    let mut directory = if path.is_absolute() {
        open_absolute_root(path)?
    } else {
        open_directory(Path::new("."))?
    };
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(LambError::Validation(
                "directory path is not normalized".into(),
            ));
        };
        match open_directory_at(&directory, name) {
            Ok(next) => directory = next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let c = c_name(name).map_err(|e| io_error(path, e))?;
                if unsafe { libc::mkdirat(directory.as_raw_fd(), c.as_ptr(), 0o777) } != 0 {
                    return Err(io_error(path, std::io::Error::last_os_error()));
                }
                directory.sync_all().map_err(|e| io_error(path, e))?;
                let child = open_directory_at(&directory, name).map_err(|e| io_error(path, e))?;
                hook(DurabilityCheckpoint::CreatedParentSynced)?;
                verify_child_at(&directory, name, &child, path)?;
                directory = child;
            }
            Err(error) => return Err(io_error(path, error)),
        }
    }
    Ok(directory)
}

fn open_existing_directory(path: &Path) -> Result<File> {
    let mut directory = if path.is_absolute() {
        open_absolute_root(path)?
    } else {
        open_directory(Path::new("."))?
    };
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(LambError::Validation(
                "directory path is not normalized".into(),
            ));
        };
        directory = open_directory_at(&directory, name).map_err(|e| io_error(path, e))?;
    }
    Ok(directory)
}

fn open_or_create_child(
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
    hook: &mut DurabilityHook<'_>,
) -> Result<File> {
    match open_directory_at(parent, name) {
        Ok(dir) => Ok(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name_c = c_name(name).map_err(|e| io_error(display, e))?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o777) } != 0 {
                return Err(io_error(display, std::io::Error::last_os_error()));
            }
            parent.sync_all().map_err(|e| io_error(display, e))?;
            let child = open_directory_at(parent, name).map_err(|e| io_error(display, e))?;
            hook(DurabilityCheckpoint::CreatedParentSynced)?;
            verify_child_at(parent, name, &child, display)?;
            Ok(child)
        }
        Err(error) => Err(io_error(display, error)),
    }
}

fn same_directory(first: &File, second: &File) -> Result<bool> {
    let first = first
        .metadata()
        .map_err(|e| io_error("calibration directory", e))?;
    let second = second
        .metadata()
        .map_err(|e| io_error("calibration directory", e))?;
    Ok((first.dev(), first.ino()) == (second.dev(), second.ino()))
}

fn verify_child_at(
    parent: &File,
    name: &std::ffi::OsStr,
    expected: &File,
    display: &Path,
) -> Result<()> {
    let current = open_directory_at(parent, name).map_err(|e| io_error(display, e))?;
    if same_directory(&current, expected)? {
        Ok(())
    } else {
        Err(LambError::Config(
            "calibration ancestry changed after creation".into(),
        ))
    }
}

fn verify_state_ancestry(
    root_path: &Path,
    root: &File,
    input_name: &str,
    input: &File,
    generation_name: &str,
    generation: &File,
) -> Result<()> {
    let reopened_root = open_existing_directory(root_path)?;
    if !same_directory(&reopened_root, root)? {
        return Err(LambError::Config(
            "calibration root changed during preparation".into(),
        ));
    }
    verify_child_at(root, input_name.as_ref(), input, root_path)?;
    verify_child_at(input, generation_name.as_ref(), generation, root_path)
}

fn create_new_directory_at(parent: &File, name: &std::ffi::OsStr, display: &Path) -> Result<File> {
    let name_c = c_name(name).map_err(|e| io_error(display, e))?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o777) } != 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    open_directory_at(parent, name).map_err(|e| io_error(display, e))
}

fn open_generation(root: &Path, input: &str, generation: &str) -> Result<File> {
    let root = open_existing_directory(root)?;
    let input =
        open_directory_at(&root, input.as_ref()).map_err(|e| io_error(root_path(&root), e))?;
    open_directory_at(&input, generation.as_ref()).map_err(|e| io_error(root_path(&input), e))
}

fn root_path(_dir: &File) -> &Path {
    Path::new("calibration state")
}

fn open_file_at(
    parent: &File,
    name: &str,
    display: &Path,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File> {
    let name = c_name(name.as_ref()).map_err(|e| io_error(display, e))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        Err(io_error(display, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn write_metadata_at(
    parent: &File,
    name: &str,
    display: &Path,
    metadata: &CalibrationMetadata,
    hook: &mut DurabilityHook<'_>,
) -> Result<()> {
    let mut f = open_file_at(
        parent,
        name,
        display,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    serde_json::to_writer_pretty(&mut f, metadata)
        .map_err(|e| LambError::Config(format!("failed to serialize calibration metadata: {e}")))?;
    f.write_all(b"\n").map_err(|e| io_error(display, e))?;
    hook(DurabilityCheckpoint::MetadataWritten)?;
    f.sync_all().map_err(|e| io_error(display, e))?;
    hook(DurabilityCheckpoint::MetadataSynced)
}

fn write_wav_at(
    parent: &File,
    name: &str,
    display: &Path,
    samples: &[f32],
    sample_rate: u32,
    hook: &mut DurabilityHook<'_>,
) -> Result<()> {
    let path = descriptor_child_path(parent, name).map_err(|error| io_error(display, error))?;
    write_wav(&path, samples, sample_rate, hook).map_err(|error| relabel_io(error, display))
}

fn read_metadata_at(parent: &File, name: &str, display: &Path) -> Result<CalibrationMetadata> {
    read_metadata(&descriptor_child_path(parent, name).map_err(|error| io_error(display, error))?)
        .map_err(|error| relabel_io(error, display))
}

fn validate_wav_at(
    parent: &File,
    name: &str,
    display: &Path,
    rate: u32,
    frames: u64,
) -> Result<f32> {
    validate_wav(
        &descriptor_child_path(parent, name).map_err(|error| io_error(display, error))?,
        rate,
        frames,
    )
    .map_err(|error| relabel_io(error, display))
}

fn descriptor_child_path(parent: &File, name: &str) -> std::io::Result<PathBuf> {
    let validated_name = c_name(Path::new(name).as_os_str())?;
    let validated_name = validated_name.to_str().map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "name is not valid UTF-8")
    })?;

    Ok(PathBuf::from(format!(
        "/proc/self/fd/{}/{}",
        parent.as_raw_fd(),
        validated_name
    )))
}

fn relabel_io(error: LambError, _display: &Path) -> LambError {
    error
}

fn open_directory(path: &Path) -> Result<File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path_c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| LambError::Validation("path contains NUL".into()))?;
    let fd = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(io_error(path, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn normal_file_name(path: &Path) -> Result<&std::ffi::OsStr> {
    let Some(name) = path.file_name() else {
        return Err(LambError::Config("path has no filename".into()));
    };
    if matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) {
        Ok(name)
    } else {
        Err(LambError::Validation("path filename is not normal".into()))
    }
}

fn directory_identity(dir: &File, display: &Path) -> Result<(u64, u64)> {
    let m = dir.metadata().map_err(|e| io_error(display, e))?;
    Ok((m.dev(), m.ino()))
}

fn parent_path_matches(path: &Path, expected: (u64, u64)) -> bool {
    open_existing_directory(path)
        .and_then(|dir| directory_identity(&dir, path))
        .ok()
        == Some(expected)
}

struct AtMetadata {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
}

fn metadata_at(
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
) -> Result<Option<AtMetadata>> {
    let name = c_name(name).map_err(|e| io_error(display, e))?;
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(Some(AtMetadata {
            dev: stat.st_dev,
            ino: stat.st_ino,
            mode: stat.st_mode,
            uid: stat.st_uid,
            gid: stat.st_gid,
        }))
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(io_error(display, error))
        }
    }
}

fn capture_config_target_at(
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
) -> Result<Option<ConfigTarget>> {
    let Some(metadata) = metadata_at(parent, name, display)? else {
        return Ok(None);
    };
    if metadata.mode & libc::S_IFMT != libc::S_IFREG {
        return Err(LambError::Validation(format!(
            "config target {} must be a non-symlink regular file",
            display.display()
        )));
    }
    Ok(Some(ConfigTarget {
        dev: metadata.dev,
        ino: metadata.ino,
        mode: metadata.mode & 0o7777,
        uid: metadata.uid,
        gid: metadata.gid,
    }))
}

fn target_matches_at(
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
    expected: Option<ConfigTarget>,
) -> Result<bool> {
    Ok(
        match (capture_config_target_at(parent, name, display), expected) {
            (Ok(None), None) => true,
            (Ok(Some(actual)), Some(expected)) => actual.identity() == expected.identity(),
            _ => false,
        },
    )
}

fn adjacent_name(target: &std::ffi::OsStr, tag: &str, sequence: u64) -> Result<std::ffi::OsString> {
    Ok(format!(".{}.{}.{}.tmp", target.to_string_lossy(), tag, sequence).into())
}

fn create_unique_adjacent_at(
    parent: &File,
    target: &std::ffi::OsStr,
    tag: &str,
    display: &Path,
    names: &mut dyn FnMut() -> Result<u64>,
) -> Result<(std::ffi::OsString, PathBuf, File)> {
    for _ in 0..128 {
        let name = adjacent_name(target, tag, names()?)?;
        match open_file_at(
            parent,
            name.to_string_lossy().as_ref(),
            display,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => {
                return Ok((
                    name.clone(),
                    display.parent().unwrap_or(Path::new("")).join(&name),
                    file,
                ))
            }
            Err(LambError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue
            }
            Err(error) => return Err(error),
        }
    }
    Err(LambError::Config(
        "could not create a unique adjacent temporary file after 128 attempts".into(),
    ))
}

fn file_identity_at(parent: &File, name: &std::ffi::OsStr, display: &Path) -> Result<(u64, u64)> {
    let Some(m) = metadata_at(parent, name, display)? else {
        return Err(io_error(
            display,
            std::io::Error::from(std::io::ErrorKind::NotFound),
        ));
    };
    if m.mode & libc::S_IFMT != libc::S_IFREG {
        return Err(LambError::Validation(format!(
            "{} is not a non-symlink regular file",
            display.display()
        )));
    }
    Ok((m.dev, m.ino))
}

fn remove_if_identity_at(
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
    expected: (u64, u64),
) -> Result<()> {
    if file_identity_at(parent, name, display)? != expected {
        return Err(LambError::Config(format!(
            "refusing to remove identity-changed file {}",
            display.display()
        )));
    }
    let name = c_name(name).map_err(|e| io_error(display, e))?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    Ok(())
}

fn cleanup_owned_error_at(
    operation: LambError,
    parent: &File,
    name: &std::ffi::OsStr,
    display: &Path,
    expected: (u64, u64),
) -> Result<()> {
    match remove_if_identity_at(parent, name, display, expected) {
        Ok(()) => Err(operation),
        Err(cleanup) => Err(LambError::PersistenceCleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
    }
}

fn rename_no_replace_or_exchange_at(
    parent: &File,
    source: &std::ffi::OsStr,
    destination: &std::ffi::OsStr,
    display: &Path,
    exchange: bool,
) -> Result<()> {
    let source = c_name(source).map_err(|e| io_error(display, e))?;
    let destination = c_name(destination).map_err(|e| io_error(display, e))?;
    let flags = if exchange {
        libc::RENAME_EXCHANGE
    } else {
        libc::RENAME_NOREPLACE
    };
    if unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    Ok(())
}

#[allow(dead_code)]
fn capture_config_target(path: &Path) -> Result<Option<ConfigTarget>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path, error)),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(LambError::Validation(format!(
            "config target {} must be a non-symlink regular file",
            path.display()
        )));
    }
    Ok(Some(ConfigTarget {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mode: metadata.mode() & 0o7777,
        uid: metadata.uid(),
        gid: metadata.gid(),
    }))
}

#[allow(dead_code)]
fn target_matches(path: &Path, expected: Option<ConfigTarget>) -> Result<bool> {
    match (capture_config_target(path), expected) {
        (Ok(None), None) => Ok(true),
        (Ok(Some(actual)), Some(expected)) => Ok(actual.identity() == expected.identity()),
        (Ok(_), _) => Ok(false),
        (Err(_), _) => Ok(false),
    }
}

fn adjacent_path(path: &Path, tag: &str, sequence: u64) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| LambError::Config("path has no parent".into()))?;
    let name = path
        .file_name()
        .ok_or_else(|| LambError::Config("path has no filename".into()))?
        .to_string_lossy();
    Ok(parent.join(format!(".{name}.{tag}.{sequence}.tmp")))
}

fn unique_adjacent(path: &Path, tag: &str) -> Result<PathBuf> {
    let sequence = NEXT_TEMP
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| LambError::Config("temporary-name counter exhausted".into()))?;
    adjacent_path(path, tag, sequence)
}

#[allow(dead_code)]
fn create_unique_adjacent(
    path: &Path,
    tag: &str,
    name_source: &mut dyn FnMut() -> Result<u64>,
) -> Result<(PathBuf, File)> {
    const MAX_ATTEMPTS: usize = 128;
    for _ in 0..MAX_ATTEMPTS {
        let candidate = adjacent_path(path, tag, name_source()?)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(&candidate, error)),
        }
    }
    Err(LambError::Config(format!(
        "could not create a unique adjacent temporary file after {MAX_ATTEMPTS} attempts"
    )))
}

fn dummy_metadata() -> CalibrationMetadata {
    CalibrationMetadata {
        version: 0,
        calibration_id: String::new(),
        configured_input: None,
        resolved_live_input: None,
        input_id: String::new(),
        detector_version: String::new(),
        threshold_dbfs: 0.0,
        p95_rms: 0.0,
        observed_peak: 0.0,
        complete_windows: 0,
        partial_final_frames: 0,
        dropped_frames: 0,
        frames: 0,
        sample_rate: 0,
        created_at_unix_seconds: 0,
        legacy_input: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_child_path_rejects_traversal_and_accepts_simple_names() {
        let parent = open_directory(Path::new("/")).expect("open root");

        for name in [".", "..", "/absolute", "with/slash", "./dot"] {
            assert!(
                descriptor_child_path(&parent, name).is_err(),
                "expected invalid descriptor child name: {name}"
            );
        }

        let sample =
            descriptor_child_path(&parent, "sample.wav").expect("sample.wav should be accepted");
        assert_eq!(
            sample,
            PathBuf::from(format!("/proc/self/fd/{}/sample.wav", parent.as_raw_fd())),
        );
    }
}
