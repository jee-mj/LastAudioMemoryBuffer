use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelExportMode {
    Always,
    Auto,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityDetectorKind {
    ExactZero,
    WindowedRmsPeak,
    FixedLevel,
    CalibratedNoiseFloor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityResult {
    Active,
    Inactive,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThresholdSource {
    Manual,
    Calibrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SilencePolicyPreset {
    AllChannelsExactZero,
    PerChannelExactZero,
}
