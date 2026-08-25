use crate::activity::{ActivityDetectorKind, ChannelExportMode};
use crate::app_config::ActivityThresholdConfig;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCommand {
    Recall,
    Dump,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedLayout {
    FlatDetailed,
    TimestampDirectory,
    Custom {
        directory_pattern: String,
        filename_pattern: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelActivityPolicy {
    pub name: String,
    pub mode: ChannelExportMode,
    pub threshold: Option<ActivityThresholdConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedActivityPolicy {
    pub detector: ActivityDetectorKind,
    pub channels: Vec<ChannelActivityPolicy>,
    pub whole_export_exact_zero_gate: bool,
    pub trim_leading_silence: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedExportPolicy {
    pub output_dir: PathBuf,
    pub layout: ResolvedLayout,
    pub activity: ResolvedActivityPolicy,
}
