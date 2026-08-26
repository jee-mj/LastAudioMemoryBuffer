use crate::activity::ThresholdSource;
use crate::calibration::{ConfiguredDeviceSelector, InputBackend, LiveDeviceKeyKind, StaleReason};
use crate::capture_runtime::DEFAULT_MAXIMUM_PATH_BYTES;
use crate::dump::{CommittedPersistenceRef, FrameRange, LossBreakdown};
use crate::error::{io_error, LambError, Result};
use crate::persistence_workspace::FilePlan;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum ControlRequest {
    Recall,
    Clear,
    Status,
    Stop,
    StartCapture {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        #[serde(default)]
        activate: bool,
    },
    StopCapture,
    Reload,
    Dump,
    Threshold {
        request: ThresholdRequest,
    },
}

/// Profile threshold operations are nested so the outer command remains
/// extensible without flattening unrelated command arguments into one wire
/// namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum ThresholdRequest {
    Calibrate {
        profile: String,
        channel: String,
        seconds: u32,
    },
    Set {
        profile: String,
        channel: String,
        dbfs: f64,
    },
    Show {
        profile: String,
    },
    Reset {
        profile: String,
        channel: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlResponse {
    pub ok: bool,
    pub message: String,
    pub status: Option<DaemonStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_outcome: Option<PersistenceOutcomeResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_report: Option<ThresholdReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThresholdReport {
    pub profile: String,
    pub active_profile: bool,
    pub capturing: bool,
    pub channels: Vec<ThresholdChannelReport>,
    pub message: String,
}

/// Per-configured-channel threshold state.  This deliberately separates
/// persisted artifact facts from current-live evaluation: an offline Show can
/// describe the former without claiming that a calibrated threshold is usable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThresholdChannelReport {
    pub channel: String,
    pub detector: String,
    pub detector_version: String,
    pub configured_input: ConfiguredInputReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored: Option<StoredThresholdReport>,
    pub artifact_status: CalibrationReportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_live_identity: Option<LiveInputReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_identity_matches: Option<bool>,
    pub calibration_evaluation: CalibrationEvaluation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_threshold_dbfs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfiguredInputReport {
    pub backend: InputBackend,
    pub selector: ConfiguredDeviceSelector,
    pub source: String,
    pub input_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveInputReport {
    pub backend: InputBackend,
    pub key_kind: LiveDeviceKeyKind,
    pub key_value: String,
    pub resolved_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredThresholdReport {
    pub threshold_dbfs: f64,
    pub source: ThresholdSource,
    pub updated_at_unix_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum CalibrationReportStatus {
    NotConfigured,
    NotApplicable,
    Complete,
    Stale { reason: StaleReason },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationEvaluation {
    NotResolved,
    Valid,
    Stale { reason: StaleReason },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistenceOutcomeResponse {
    Written {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        export_start_frame: u64,
        export_frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
        output_directory: PathBuf,
        files: Vec<PathBuf>,
    },
    SkippedSilent {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
    SkippedByPolicy {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
    NoNewAudio {
        #[serde(default)]
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistenceOutcomeResponseWire {
    Written {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        #[serde(default)]
        export_start_frame: Option<u64>,
        #[serde(default)]
        export_frames: Option<u64>,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
        output_directory: PathBuf,
        files: Vec<PathBuf>,
    },
    SkippedSilent {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
    SkippedByPolicy {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
    NoNewAudio {
        #[serde(default)]
        lost_frames: u64,
        #[serde(default)]
        retention_lost_frames: u64,
        #[serde(default)]
        cleared_frames: u64,
        #[serde(default)]
        capture_dropped_frames: u64,
    },
}

fn written_export_range(
    start_frame: u64,
    end_frame: u64,
    export_start_frame: Option<u64>,
    export_frames: Option<u64>,
) -> std::result::Result<(u64, u64), &'static str> {
    if end_frame < start_frame {
        return Err("written consumed range end precedes start");
    }
    match (export_start_frame, export_frames) {
        (None, None) => Ok((start_frame, end_frame - start_frame)),
        (Some(_), None) | (None, Some(_)) => {
            Err("written export range requires both export_start_frame and export_frames")
        }
        (Some(export_start_frame), Some(export_frames)) => {
            if export_start_frame < start_frame {
                return Err("written export range starts before consumed range");
            }
            let export_end = export_start_frame
                .checked_add(export_frames)
                .ok_or("written export range end overflow")?;
            if export_end != end_frame {
                return Err("written export range does not end at consumed range end");
            }
            Ok((export_start_frame, export_frames))
        }
    }
}

impl<'de> Deserialize<'de> for PersistenceOutcomeResponse {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(
            match PersistenceOutcomeResponseWire::deserialize(deserializer)? {
                PersistenceOutcomeResponseWire::Written {
                    start_frame,
                    end_frame,
                    frames,
                    export_start_frame,
                    export_frames,
                    duration_seconds,
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                    output_directory,
                    files,
                } => {
                    let (export_start_frame, export_frames) = written_export_range(
                        start_frame,
                        end_frame,
                        export_start_frame,
                        export_frames,
                    )
                    .map_err(<D::Error as serde::de::Error>::custom)?;
                    Self::Written {
                        start_frame,
                        end_frame,
                        frames,
                        export_start_frame,
                        export_frames,
                        duration_seconds,
                        lost_frames,
                        retention_lost_frames,
                        cleared_frames,
                        capture_dropped_frames,
                        output_directory,
                        files,
                    }
                }
                PersistenceOutcomeResponseWire::SkippedSilent {
                    start_frame,
                    end_frame,
                    frames,
                    duration_seconds,
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                } => Self::SkippedSilent {
                    start_frame,
                    end_frame,
                    frames,
                    duration_seconds,
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                },
                PersistenceOutcomeResponseWire::SkippedByPolicy {
                    start_frame,
                    end_frame,
                    frames,
                    duration_seconds,
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                } => Self::SkippedByPolicy {
                    start_frame,
                    end_frame,
                    frames,
                    duration_seconds,
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                },
                PersistenceOutcomeResponseWire::NoNewAudio {
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                } => Self::NoNewAudio {
                    lost_frames,
                    retention_lost_frames,
                    cleared_frames,
                    capture_dropped_frames,
                },
            },
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonStatus {
    pub state: String,
    pub active_export_count: u32,
    pub pending_recall_count: u32,
    pub buffer_capacity_seconds: f64,
    pub retained_seconds: f64,
    pub dropped_frames: u64,
    pub target: Option<String>,
    pub resolved_target: Option<String>,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub format: String,
    pub last_error: Option<String>,
}

/// Streams a committed prepared-persistence response without taking ownership
/// of the workspace-backed output plan.
pub fn write_persistence_response<W: Write>(
    writer: &mut W,
    ok: bool,
    message: &str,
    status: &DaemonStatus,
    sample_rate: u32,
    outcome: CommittedPersistenceRef<'_>,
) -> Result<()> {
    let response = BorrowedPersistenceResponse::new(ok, message, status, sample_rate, outcome)?;
    serde_json::to_writer(&mut *writer, &response)
        .map_err(|error| LambError::Control(error.to_string()))?;
    writer
        .write_all(b"\n")
        .map_err(|error| LambError::Control(error.to_string()))
}

#[derive(Serialize)]
struct BorrowedPersistenceResponse<'a> {
    ok: bool,
    message: &'a str,
    status: &'a DaemonStatus,
    persistence_outcome: BorrowedPersistenceOutcome<'a>,
}

impl<'a> BorrowedPersistenceResponse<'a> {
    fn new(
        ok: bool,
        message: &'a str,
        status: &'a DaemonStatus,
        sample_rate: u32,
        outcome: CommittedPersistenceRef<'a>,
    ) -> Result<Self> {
        if sample_rate == 0 {
            return Err(LambError::Control(
                "persistence response sample rate is zero".to_string(),
            ));
        }
        Ok(Self {
            ok,
            message,
            status,
            persistence_outcome: BorrowedPersistenceOutcome::new(sample_rate, outcome)?,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BorrowedPersistenceOutcome<'a> {
    Written {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        export_start_frame: u64,
        export_frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        retention_lost_frames: u64,
        cleared_frames: u64,
        capture_dropped_frames: u64,
        output_directory: &'a str,
        files: BorrowedFiles<'a>,
    },
    SkippedSilent {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        retention_lost_frames: u64,
        cleared_frames: u64,
        capture_dropped_frames: u64,
    },
    SkippedByPolicy {
        start_frame: u64,
        end_frame: u64,
        frames: u64,
        duration_seconds: f64,
        lost_frames: u64,
        retention_lost_frames: u64,
        cleared_frames: u64,
        capture_dropped_frames: u64,
    },
    NoNewAudio {
        lost_frames: u64,
        retention_lost_frames: u64,
        cleared_frames: u64,
        capture_dropped_frames: u64,
    },
}

impl<'a> BorrowedPersistenceOutcome<'a> {
    fn new(sample_rate: u32, outcome: CommittedPersistenceRef<'a>) -> Result<Self> {
        match outcome {
            CommittedPersistenceRef::Written {
                range,
                export_range,
                frames,
                losses,
                output,
            } => {
                validate_geometry(range, export_range, frames)?;
                let output_directory = output.output_directory.to_str().ok_or_else(|| {
                    LambError::Control(
                        "persistence response output directory is not UTF-8".to_string(),
                    )
                })?;
                BorrowedFiles::validate(output.files)?;
                Ok(Self::Written {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    export_start_frame: export_range.start,
                    export_frames: export_range.end - export_range.start,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames: losses.lost_frames(),
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                    output_directory,
                    files: BorrowedFiles(output.files),
                })
            }
            CommittedPersistenceRef::SkippedSilent {
                range,
                frames,
                losses,
            } => {
                validate_consumed_geometry(range, frames)?;
                let losses = loss_fields(losses);
                Ok(Self::SkippedSilent {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames: losses.lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                })
            }
            CommittedPersistenceRef::SkippedByPolicy {
                range,
                frames,
                losses,
            } => {
                validate_consumed_geometry(range, frames)?;
                let losses = loss_fields(losses);
                Ok(Self::SkippedByPolicy {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames,
                    duration_seconds: frames as f64 / f64::from(sample_rate),
                    lost_frames: losses.lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                })
            }
            CommittedPersistenceRef::NoNewAudio { losses } => {
                let losses = loss_fields(losses);
                Ok(Self::NoNewAudio {
                    lost_frames: losses.lost_frames,
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                })
            }
        }
    }
}

fn loss_fields(losses: LossBreakdown) -> LossFields {
    LossFields {
        lost_frames: losses.lost_frames(),
        retention_lost_frames: losses.retention_lost_frames,
        cleared_frames: losses.cleared_frames,
        capture_dropped_frames: losses.capture_dropped_frames,
    }
}

struct LossFields {
    lost_frames: u64,
    retention_lost_frames: u64,
    cleared_frames: u64,
    capture_dropped_frames: u64,
}

fn validate_consumed_geometry(range: FrameRange, frames: u64) -> Result<()> {
    if range.end < range.start || range.end - range.start != frames {
        return Err(LambError::Control(
            "persistence response consumed range is inconsistent".to_string(),
        ));
    }
    Ok(())
}

fn validate_geometry(range: FrameRange, export_range: FrameRange, frames: u64) -> Result<()> {
    validate_consumed_geometry(range, frames)?;
    if export_range.start < range.start
        || export_range.end != range.end
        || export_range.end < export_range.start
    {
        return Err(LambError::Control(
            "persistence response export range is inconsistent".to_string(),
        ));
    }
    Ok(())
}

struct BorrowedFiles<'a>(FilePlan<'a>);

impl<'a> BorrowedFiles<'a> {
    fn validate(files: FilePlan<'a>) -> Result<()> {
        for file in files.iter() {
            file.final_path().to_str().ok_or_else(|| {
                LambError::Control("persistence response file path is not UTF-8".to_string())
            })?;
        }
        Ok(())
    }
}

impl Serialize for BorrowedFiles<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for file in self.0.iter() {
            let path = file.final_path().to_str().ok_or_else(|| {
                serde::ser::Error::custom("persistence response file path is not UTF-8")
            })?;
            sequence.serialize_element(path)?;
        }
        sequence.end()
    }
}

pub fn client_send_simple(socket: &Path, command: &str) -> Result<()> {
    let request = match command {
        "recall" => ControlRequest::Recall,
        "clear" => ControlRequest::Clear,
        "stop" => ControlRequest::Stop,
        other => {
            return Err(LambError::Control(format!(
                "unknown simple command {other}"
            )))
        }
    };
    let response = send_request(socket, &request)?;
    if response.ok {
        Ok(())
    } else {
        Err(LambError::Control(response.message))
    }
}

pub fn client_status(socket: &Path, json: bool) -> Result<()> {
    let response = send_request(socket, &ControlRequest::Status)?;
    if !response.ok {
        return Err(LambError::Control(response.message));
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&response.status).unwrap()
        );
    } else if let Some(status) = response.status {
        println!("lamb: {}", status.state);
        println!("  sample_rate: {}", status.sample_rate);
        println!("  channels: {}", status.channel_count);
        println!("  retained_seconds: {:.3}", status.retained_seconds);
        println!("  dropped_frames: {}", status.dropped_frames);
    }
    Ok(())
}

pub fn send_request(socket: &Path, request: &ControlRequest) -> Result<ControlResponse> {
    let mut stream = UnixStream::connect(socket).map_err(|source| io_error(socket, source))?;
    let line = serde_json::to_string(request).map_err(|err| LambError::Control(err.to_string()))?;
    writeln!(stream, "{line}").map_err(|source| io_error(socket, source))?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|source| io_error(socket, source))?;
    serde_json::from_str(&response)
        .map_err(|err| LambError::Control(format!("invalid response: {err}")))
}

/// Deserializes a persistence response directly to a caller-owned writer.
///
/// The seed retains only response scalars and one bounded byte buffer. Written
/// file paths are rendered in wire order while their sequence is visited.
pub struct PersistenceClientResponseSeed<'a, W: Write> {
    writer: &'a mut W,
    buffer: Vec<u8>,
}

impl<'a, W: Write> PersistenceClientResponseSeed<'a, W> {
    pub fn new(writer: &'a mut W) -> Self {
        Self {
            writer,
            buffer: Vec::with_capacity(DEFAULT_MAXIMUM_PATH_BYTES as usize),
        }
    }
}

impl<'de, W: Write> DeserializeSeed<'de> for PersistenceClientResponseSeed<'_, W> {
    type Value = std::result::Result<(), String>;

    fn deserialize<D>(mut self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(PersistenceResponseVisitor {
            writer: self.writer,
            buffer: &mut self.buffer,
        })
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum PersistenceResponseField {
    Ok,
    Message,
    Status,
    PersistenceOutcome,
    ThresholdReport,
    #[serde(other)]
    Other,
}

struct PersistenceResponseVisitor<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> Visitor<'de> for PersistenceResponseVisitor<'_, W> {
    type Value = std::result::Result<(), String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a persistence control response")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut ok = None;
        let mut message = None;
        let mut persistence_outcome = false;
        let mut rendered_outcome = false;

        while let Some(field) = map.next_key()? {
            match field {
                PersistenceResponseField::Ok => {
                    set_once(&mut ok, map.next_value()?, "ok")?;
                }
                PersistenceResponseField::Message => {
                    set_once(&mut message, map.next_value()?, "message")?;
                }
                PersistenceResponseField::PersistenceOutcome => {
                    if persistence_outcome {
                        return Err(de::Error::duplicate_field("persistence_outcome"));
                    }
                    let response_ok = ok.ok_or_else(|| de::Error::missing_field("ok"))?;
                    if message.is_none() {
                        return Err(de::Error::missing_field("message"));
                    }
                    persistence_outcome = true;
                    if response_ok {
                        rendered_outcome = map.next_value_seed(PersistenceOutcomeOptionSeed {
                            writer: self.writer,
                            buffer: self.buffer,
                        })?;
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                PersistenceResponseField::Status
                | PersistenceResponseField::ThresholdReport
                | PersistenceResponseField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        let response_ok = ok.ok_or_else(|| de::Error::missing_field("ok"))?;
        let message = message.ok_or_else(|| de::Error::missing_field("message"))?;
        if !response_ok {
            return Ok(Err(message));
        }
        if !rendered_outcome {
            self.writer
                .write_all(message.as_bytes())
                .and_then(|()| self.writer.write_all(b"\n"))
                .map_err(de::Error::custom)?;
        } else {
            self.writer.write_all(b"\n").map_err(de::Error::custom)?;
        }
        Ok(Ok(()))
    }
}

struct BoundedStringSeed<'a> {
    buffer: &'a mut Vec<u8>,
    label: &'static str,
}

impl<'de> DeserializeSeed<'de> for BoundedStringSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(BoundedStringVisitor {
            buffer: self.buffer,
            label: self.label,
        })
    }
}

struct BoundedStringVisitor<'a> {
    buffer: &'a mut Vec<u8>,
    label: &'static str,
}

impl Visitor<'_> for BoundedStringVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} no longer than {DEFAULT_MAXIMUM_PATH_BYTES} bytes",
            self.label
        )
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value.len() > DEFAULT_MAXIMUM_PATH_BYTES as usize {
            return Err(E::custom(format_args!(
                "{} exceeds maximum path bytes ({DEFAULT_MAXIMUM_PATH_BYTES})",
                self.label
            )));
        }
        self.buffer.clear();
        self.buffer.extend_from_slice(value.as_bytes());
        Ok(())
    }

    fn visit_borrowed_str<E>(self, value: &'_ str) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }
}

struct PersistenceOutcomeOptionSeed<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> DeserializeSeed<'de> for PersistenceOutcomeOptionSeed<'_, W> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_option(PersistenceOutcomeOptionVisitor {
            writer: self.writer,
            buffer: self.buffer,
        })
    }
}

struct PersistenceOutcomeOptionVisitor<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> Visitor<'de> for PersistenceOutcomeOptionVisitor<'_, W> {
    type Value = bool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a persistence outcome or null")
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(false)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(false)
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        PersistenceOutcomeSeed {
            writer: self.writer,
            buffer: self.buffer,
        }
        .deserialize(deserializer)?;
        Ok(true)
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistenceClientOutcomeKind {
    Written,
    SkippedSilent,
    SkippedByPolicy,
    NoNewAudio,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum PersistenceOutcomeField {
    Kind,
    StartFrame,
    EndFrame,
    Frames,
    ExportStartFrame,
    ExportFrames,
    DurationSeconds,
    LostFrames,
    RetentionLostFrames,
    ClearedFrames,
    CaptureDroppedFrames,
    OutputDirectory,
    Files,
    #[serde(other)]
    Other,
}

#[derive(Default)]
struct PersistenceOutcomeScalars {
    kind: Option<PersistenceClientOutcomeKind>,
    start_frame: Option<u64>,
    end_frame: Option<u64>,
    frames: Option<u64>,
    export_start_frame: Option<u64>,
    export_frames: Option<u64>,
    duration_seconds: Option<f64>,
    lost_frames: Option<u64>,
    retention_lost_frames: Option<u64>,
    cleared_frames: Option<u64>,
    capture_dropped_frames: Option<u64>,
    output_directory: bool,
}

struct PersistenceOutcomeSeed<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> DeserializeSeed<'de> for PersistenceOutcomeSeed<'_, W> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(PersistenceOutcomeVisitor {
            writer: self.writer,
            buffer: self.buffer,
        })
    }
}

struct PersistenceOutcomeVisitor<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> Visitor<'de> for PersistenceOutcomeVisitor<'_, W> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a persistence outcome")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = PersistenceOutcomeScalars::default();
        let mut rendered_files = false;
        while let Some(field) = map.next_key()? {
            if rendered_files {
                return Err(de::Error::custom(
                    "files must be the final persistence field",
                ));
            }
            match field {
                PersistenceOutcomeField::Kind => {
                    set_once(&mut values.kind, map.next_value()?, "kind")?;
                }
                PersistenceOutcomeField::StartFrame => {
                    set_once(&mut values.start_frame, map.next_value()?, "start_frame")?;
                }
                PersistenceOutcomeField::EndFrame => {
                    set_once(&mut values.end_frame, map.next_value()?, "end_frame")?;
                }
                PersistenceOutcomeField::Frames => {
                    set_once(&mut values.frames, map.next_value()?, "frames")?;
                }
                PersistenceOutcomeField::ExportStartFrame => set_once(
                    &mut values.export_start_frame,
                    map.next_value()?,
                    "export_start_frame",
                )?,
                PersistenceOutcomeField::ExportFrames => set_once(
                    &mut values.export_frames,
                    map.next_value()?,
                    "export_frames",
                )?,
                PersistenceOutcomeField::DurationSeconds => set_once(
                    &mut values.duration_seconds,
                    map.next_value()?,
                    "duration_seconds",
                )?,
                PersistenceOutcomeField::LostFrames => {
                    set_once(&mut values.lost_frames, map.next_value()?, "lost_frames")?;
                }
                PersistenceOutcomeField::RetentionLostFrames => set_once(
                    &mut values.retention_lost_frames,
                    map.next_value()?,
                    "retention_lost_frames",
                )?,
                PersistenceOutcomeField::ClearedFrames => set_once(
                    &mut values.cleared_frames,
                    map.next_value()?,
                    "cleared_frames",
                )?,
                PersistenceOutcomeField::CaptureDroppedFrames => set_once(
                    &mut values.capture_dropped_frames,
                    map.next_value()?,
                    "capture_dropped_frames",
                )?,
                PersistenceOutcomeField::OutputDirectory => {
                    if values.output_directory {
                        return Err(de::Error::duplicate_field("output_directory"));
                    }
                    map.next_value_seed(BoundedStringSeed {
                        buffer: self.buffer,
                        label: "output directory",
                    })?;
                    values.output_directory = true;
                }
                PersistenceOutcomeField::Files => {
                    render_written_header::<W, A::Error>(self.writer, self.buffer, &values)?;
                    self.buffer.clear();
                    map.next_value_seed(PersistenceFilesSeed {
                        writer: self.writer,
                        buffer: self.buffer,
                    })?;
                    rendered_files = true;
                }
                PersistenceOutcomeField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        match values
            .kind
            .ok_or_else(|| de::Error::missing_field("kind"))?
        {
            PersistenceClientOutcomeKind::Written if !rendered_files => {
                Err(de::Error::missing_field("files"))
            }
            PersistenceClientOutcomeKind::Written => Ok(()),
            kind if rendered_files => Err(de::Error::custom(format_args!(
                "files are invalid for {}",
                outcome_kind_name(kind)
            ))),
            kind => render_non_written::<W, A::Error>(self.writer, kind, &values),
        }
    }
}

fn set_once<T, E: de::Error>(
    destination: &mut Option<T>,
    value: T,
    field: &'static str,
) -> std::result::Result<(), E> {
    if destination.replace(value).is_some() {
        Err(E::duplicate_field(field))
    } else {
        Ok(())
    }
}

fn required<T: Copy, E: de::Error>(
    value: Option<T>,
    field: &'static str,
) -> std::result::Result<T, E> {
    value.ok_or_else(|| E::missing_field(field))
}

fn outcome_kind_name(kind: PersistenceClientOutcomeKind) -> &'static str {
    match kind {
        PersistenceClientOutcomeKind::Written => "written",
        PersistenceClientOutcomeKind::SkippedSilent => "skipped_silent",
        PersistenceClientOutcomeKind::SkippedByPolicy => "skipped_by_policy",
        PersistenceClientOutcomeKind::NoNewAudio => "no_new_audio",
    }
}

fn render_written_header<W: Write, E: de::Error>(
    writer: &mut W,
    output_directory: &[u8],
    values: &PersistenceOutcomeScalars,
) -> std::result::Result<(), E> {
    if !matches!(values.kind, Some(PersistenceClientOutcomeKind::Written)) {
        return Err(E::custom("files require a written persistence outcome"));
    }
    if !values.output_directory {
        return Err(E::custom("output_directory must precede files"));
    }
    let start_frame = required::<_, E>(values.start_frame, "start_frame")?;
    let end_frame = required::<_, E>(values.end_frame, "end_frame")?;
    let frames = required::<_, E>(values.frames, "frames")?;
    let export_start_frame = required::<_, E>(values.export_start_frame, "export_start_frame")?;
    let export_frames = required::<_, E>(values.export_frames, "export_frames")?;
    written_export_range(
        start_frame,
        end_frame,
        Some(export_start_frame),
        Some(export_frames),
    )
    .map_err(E::custom)?;
    let duration_seconds = required::<_, E>(values.duration_seconds, "duration_seconds")?;
    let lost_frames = required::<_, E>(values.lost_frames, "lost_frames")?;
    let retention = required::<_, E>(values.retention_lost_frames, "retention_lost_frames")?;
    let cleared = required::<_, E>(values.cleared_frames, "cleared_frames")?;
    let dropped = required::<_, E>(values.capture_dropped_frames, "capture_dropped_frames")?;
    let output_directory = std::str::from_utf8(output_directory).map_err(E::custom)?;
    write!(
        writer,
        "written {frames} frames ({duration_seconds:.3} seconds), source frames {start_frame}..{end_frame}"
    )
    .map_err(E::custom)?;
    render_loss_warning::<_, E>(writer, lost_frames, retention, cleared, dropped)?;
    write!(writer, "\noutput directory: {output_directory}").map_err(E::custom)
}

fn render_non_written<W: Write, E: de::Error>(
    writer: &mut W,
    kind: PersistenceClientOutcomeKind,
    values: &PersistenceOutcomeScalars,
) -> std::result::Result<(), E> {
    let lost_frames = required::<_, E>(values.lost_frames, "lost_frames")?;
    let retention = required::<_, E>(values.retention_lost_frames, "retention_lost_frames")?;
    let cleared = required::<_, E>(values.cleared_frames, "cleared_frames")?;
    let dropped = required::<_, E>(values.capture_dropped_frames, "capture_dropped_frames")?;
    match kind {
        PersistenceClientOutcomeKind::SkippedSilent
        | PersistenceClientOutcomeKind::SkippedByPolicy => {
            let start_frame = required::<_, E>(values.start_frame, "start_frame")?;
            let end_frame = required::<_, E>(values.end_frame, "end_frame")?;
            let frames = required::<_, E>(values.frames, "frames")?;
            let duration_seconds = required::<_, E>(values.duration_seconds, "duration_seconds")?;
            let label = if matches!(kind, PersistenceClientOutcomeKind::SkippedSilent) {
                "skipped exact-zero audio"
            } else {
                "skipped by policy"
            };
            write!(
                writer,
                "{label}: {frames} frames ({duration_seconds:.3} seconds), source frames {start_frame}..{end_frame}"
            )
            .map_err(E::custom)?;
        }
        PersistenceClientOutcomeKind::NoNewAudio => {
            writer.write_all(b"no new audio").map_err(E::custom)?;
        }
        PersistenceClientOutcomeKind::Written => unreachable!(),
    }
    render_loss_warning::<_, E>(writer, lost_frames, retention, cleared, dropped)
}

fn render_loss_warning<W: Write, E: de::Error>(
    writer: &mut W,
    lost: u64,
    retention: u64,
    cleared: u64,
    dropped: u64,
) -> std::result::Result<(), E> {
    if lost == 0 {
        return Ok(());
    }
    write!(
        writer,
        "\nwarning: {lost} frames were lost before persistence ("
    )
    .map_err(E::custom)?;
    let mut separator = "";
    for (label, count) in [
        ("retention", retention),
        ("cleared", cleared),
        ("capture-dropped", dropped),
    ] {
        if count > 0 {
            write!(writer, "{separator}{label} {count}").map_err(E::custom)?;
            separator = ", ";
        }
    }
    writer.write_all(b")").map_err(E::custom)
}

struct PersistenceFilesSeed<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> DeserializeSeed<'de> for PersistenceFilesSeed<'_, W> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(PersistenceFilesVisitor {
            writer: self.writer,
            buffer: self.buffer,
        })
    }
}

struct PersistenceFilesVisitor<'a, W> {
    writer: &'a mut W,
    buffer: &'a mut Vec<u8>,
}

impl<'de, W: Write> Visitor<'de> for PersistenceFilesVisitor<'_, W> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a sequence of bounded persistence paths")
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence
            .next_element_seed(BoundedStringSeed {
                buffer: self.buffer,
                label: "persistence file path",
            })?
            .is_some()
        {
            self.writer.write_all(b"\n").map_err(de::Error::custom)?;
            self.writer
                .write_all(self.buffer)
                .map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

struct ResponseLineReader<R> {
    inner: R,
    finished: bool,
}

impl<R: BufRead> Read for ResponseLineReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.finished || output.is_empty() {
            return Ok(0);
        }
        let available = self.inner.fill_buf()?;
        if available.is_empty() {
            self.finished = true;
            return Ok(0);
        }
        let line_bytes = available
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(available.len());
        let reached_newline = line_bytes < available.len();
        let copied = line_bytes.min(output.len());
        output[..copied].copy_from_slice(&available[..copied]);
        self.inner.consume(copied);
        if copied == line_bytes && reached_newline {
            self.inner.consume(1);
            self.finished = true;
        }
        Ok(copied)
    }
}

pub fn send_persistence_request_streaming<W: Write>(
    socket: &Path,
    request: &ControlRequest,
    writer: &mut W,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket).map_err(|source| io_error(socket, source))?;
    let line = serde_json::to_string(request).map_err(|err| LambError::Control(err.to_string()))?;
    writeln!(stream, "{line}").map_err(|source| io_error(socket, source))?;
    let reader = BufReader::new(stream);
    let mut line_reader = ResponseLineReader {
        inner: reader,
        finished: false,
    };
    let mut deserializer = serde_json::Deserializer::from_reader(&mut line_reader);
    let response = PersistenceClientResponseSeed::new(writer)
        .deserialize(&mut deserializer)
        .map_err(|err| LambError::Control(format!("invalid response: {err}")))?;
    deserializer
        .end()
        .map_err(|err| LambError::Control(format!("invalid response: {err}")))?;
    response.map_err(LambError::Control)
}

pub fn client_dump(socket: &Path) -> Result<()> {
    client_persistence(socket, ControlRequest::Dump)
}

pub fn client_recall(socket: &Path) -> Result<()> {
    client_persistence(socket, ControlRequest::Recall)
}

fn client_persistence(socket: &Path, request: ControlRequest) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    send_persistence_request_streaming(socket, &request, &mut output)
}

#[cfg(test)]
fn format_loss_causes(retention: u64, cleared: u64, dropped: u64) -> String {
    let mut causes = Vec::new();
    if retention > 0 {
        causes.push(format!("retention {retention}"));
    }
    if cleared > 0 {
        causes.push(format!("cleared {cleared}"));
    }
    if dropped > 0 {
        causes.push(format!("capture-dropped {dropped}"));
    }
    causes.join(", ")
}

#[cfg(test)]
fn format_persistence_outcome(outcome: &PersistenceOutcomeResponse) -> String {
    match outcome {
        PersistenceOutcomeResponse::Written {
            start_frame,
            end_frame,
            frames,
            export_start_frame: _,
            export_frames: _,
            duration_seconds,
            lost_frames,
            retention_lost_frames,
            cleared_frames,
            capture_dropped_frames,
            output_directory,
            files,
        } => {
            let mut lines = vec![format!(
                "written {frames} frames ({duration_seconds:.3} seconds), source frames {start_frame}..{end_frame}"
            )];
            if *lost_frames > 0 {
                lines.push(format!(
                    "warning: {lost_frames} frames were lost before persistence ({})",
                    format_loss_causes(
                        *retention_lost_frames,
                        *cleared_frames,
                        *capture_dropped_frames
                    )
                ));
            }
            lines.push(format!("output directory: {}", output_directory.display()));
            lines.extend(files.iter().map(|file| file.display().to_string()));
            lines.join("\n")
        }
        PersistenceOutcomeResponse::SkippedSilent {
            start_frame,
            end_frame,
            frames,
            duration_seconds,
            lost_frames,
            retention_lost_frames,
            cleared_frames,
            capture_dropped_frames,
        } => {
            let mut lines = vec![format!(
                "skipped exact-zero audio: {frames} frames ({duration_seconds:.3} seconds), source frames {start_frame}..{end_frame}"
            )];
            if *lost_frames > 0 {
                lines.push(format!(
                    "warning: {lost_frames} frames were lost before persistence ({})",
                    format_loss_causes(
                        *retention_lost_frames,
                        *cleared_frames,
                        *capture_dropped_frames
                    )
                ));
            }
            lines.join("\n")
        }
        PersistenceOutcomeResponse::SkippedByPolicy {
            start_frame,
            end_frame,
            frames,
            duration_seconds,
            lost_frames,
            retention_lost_frames,
            cleared_frames,
            capture_dropped_frames,
        } => {
            let mut lines = vec![format!(
                "skipped by policy: {frames} frames ({duration_seconds:.3} seconds), source frames {start_frame}..{end_frame}"
            )];
            if *lost_frames > 0 {
                lines.push(format!(
                    "warning: {lost_frames} frames were lost before persistence ({})",
                    format_loss_causes(
                        *retention_lost_frames,
                        *cleared_frames,
                        *capture_dropped_frames
                    )
                ));
            }
            lines.join("\n")
        }
        PersistenceOutcomeResponse::NoNewAudio {
            lost_frames,
            retention_lost_frames,
            cleared_frames,
            capture_dropped_frames,
        } => {
            if *lost_frames == 0 {
                "no new audio".to_string()
            } else {
                format!(
                    "no new audio\nwarning: {lost_frames} frames were lost before persistence ({})",
                    format_loss_causes(
                        *retention_lost_frames,
                        *cleared_frames,
                        *capture_dropped_frames
                    )
                )
            }
        }
    }
}

pub fn client_start_capture(socket: &Path, profile: Option<String>, activate: bool) -> Result<()> {
    let request = ControlRequest::StartCapture { profile, activate };
    let response = send_request(socket, &request)?;
    if response.ok {
        Ok(())
    } else {
        Err(LambError::Control(response.message))
    }
}

pub fn client_stop_capture(socket: &Path) -> Result<()> {
    let response = send_request(socket, &ControlRequest::StopCapture)?;
    if response.ok {
        Ok(())
    } else {
        Err(LambError::Control(response.message))
    }
}

pub fn client_reload(socket: &Path) -> Result<()> {
    let response = send_request(socket, &ControlRequest::Reload)?;
    if response.ok {
        Ok(())
    } else {
        Err(LambError::Control(response.message))
    }
}

pub fn client_threshold(socket: &Path, request: ThresholdRequest) -> Result<()> {
    let response = send_request(socket, &ControlRequest::Threshold { request })?;
    if response.ok {
        println!("{}", response.message);
        if let Some(report) = response.threshold_report {
            let report = serde_json::to_string_pretty(&report)
                .map_err(|err| LambError::Control(err.to_string()))?;
            println!("{report}");
        }
        Ok(())
    } else {
        Err(LambError::Control(response.message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{ActivityDetectorKind, ChannelExportMode, FrozenExportDecision};
    use crate::capture_arena::{CaptureArena, CaptureRuntimeConfig};
    use crate::dump::{CommittedPersistenceRef, FrameRange, LossBreakdown};
    use crate::dump::{DumpCoordinator, PolicyPersistenceRequest};
    use crate::export_policy::{
        ChannelActivityPolicy, ExportCommand, ResolvedActivityPolicy, ResolvedExportPolicy,
        ResolvedLayout,
    };
    use crate::memory_plan::{SessionMemoryInputs, SessionMemoryPlan};
    use crate::persistence_workspace::{PersistenceWorkspace, PersistenceWorkspaceConfig};
    use crate::sample_ring::{RingConfig, SampleFormat};
    use std::time::Duration;

    fn persistence_status() -> DaemonStatus {
        DaemonStatus {
            state: "capturing".to_string(),
            active_export_count: 0,
            pending_recall_count: 0,
            buffer_capacity_seconds: 1.0,
            retained_seconds: 1.0,
            dropped_frames: 0,
            target: None,
            resolved_target: None,
            sample_rate: 100,
            channel_count: 1,
            format: "F32LE".to_string(),
            last_error: None,
        }
    }

    #[test]
    fn borrowed_persistence_json_serializes_non_written_outcomes_compatibly() {
        let status = persistence_status();
        let outcomes = [
            CommittedPersistenceRef::SkippedSilent {
                range: FrameRange { start: 10, end: 20 },
                frames: 10,
                losses: LossBreakdown::default(),
            },
            CommittedPersistenceRef::SkippedByPolicy {
                range: FrameRange { start: 20, end: 30 },
                frames: 10,
                losses: LossBreakdown::default(),
            },
            CommittedPersistenceRef::NoNewAudio {
                losses: LossBreakdown::default(),
            },
        ];
        for outcome in outcomes {
            let expected = match &outcome {
                CommittedPersistenceRef::SkippedSilent {
                    range,
                    frames,
                    losses,
                } => PersistenceOutcomeResponse::SkippedSilent {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames: *frames,
                    duration_seconds: 0.1,
                    lost_frames: losses.lost_frames(),
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                },
                CommittedPersistenceRef::SkippedByPolicy {
                    range,
                    frames,
                    losses,
                } => PersistenceOutcomeResponse::SkippedByPolicy {
                    start_frame: range.start,
                    end_frame: range.end,
                    frames: *frames,
                    duration_seconds: 0.1,
                    lost_frames: losses.lost_frames(),
                    retention_lost_frames: losses.retention_lost_frames,
                    cleared_frames: losses.cleared_frames,
                    capture_dropped_frames: losses.capture_dropped_frames,
                },
                CommittedPersistenceRef::NoNewAudio { losses } => {
                    PersistenceOutcomeResponse::NoNewAudio {
                        lost_frames: losses.lost_frames(),
                        retention_lost_frames: losses.retention_lost_frames,
                        cleared_frames: losses.cleared_frames,
                        capture_dropped_frames: losses.capture_dropped_frames,
                    }
                }
                CommittedPersistenceRef::Written { .. } => unreachable!(),
            };
            let mut bytes = Vec::new();
            write_persistence_response(&mut bytes, true, "persisted", &status, 100, outcome)
                .unwrap();
            assert_eq!(bytes.last(), Some(&b'\n'));
            let response: ControlResponse = serde_json::from_slice(&bytes).unwrap();
            assert!(response.ok);
            assert_eq!(response.message, "persisted");
            assert_eq!(response.status, Some(status.clone()));
            assert_eq!(response.persistence_outcome.unwrap(), expected);
        }
    }

    #[test]
    fn borrowed_persistence_json_written_round_trips_canonical_multi_output_plan() {
        let plan = SessionMemoryPlan::calculate(SessionMemoryInputs {
            retention_frames: 8,
            channels: 2,
            sample_rate: 100,
            sample_format: SampleFormat::F32Le,
            chunk_frames: 8,
            max_active_snapshots: 1,
            sample_bytes: 4,
            split_when_over_bytes: 1_000_000,
            control_queue_capacity: 2,
            worker_stack_bytes: 64 * 1024,
            capture_queue_slots: 8,
            capture_slot_frames: 8,
            capture_worker_stack_bytes: 256 * 1024,
            io_buffer_bytes_per_channel: 1024,
            maximum_path_bytes: 256,
            maximum_calibration_seconds: 0,
            headroom: 1.0,
        })
        .unwrap();
        let (arena, ingress) = CaptureArena::new(
            &plan,
            CaptureRuntimeConfig {
                ring: RingConfig {
                    channels: 2,
                    sample_rate: 100,
                    format: SampleFormat::F32Le,
                    chunk_frames: 8,
                    chunk_count: 1,
                    max_active_snapshots: 1,
                },
                queue_slots: 8,
                slot_frames: 8,
                sample_bytes: 4,
                worker_stack_bytes: 256 * 1024,
            },
        )
        .unwrap();
        ingress.try_push_interleaved(&[0.25; 8], 2).unwrap();
        let mut workspace = PersistenceWorkspace::new(
            &plan,
            PersistenceWorkspaceConfig {
                retention_frames: 8,
                channels: 2,
                sample_rate: 100,
                sample_format: SampleFormat::F32Le,
                chunk_frames: 8,
                sample_bytes: 4,
                split_when_over_bytes: 1_000_000,
                io_buffer_bytes_per_channel: 1024,
                maximum_path_bytes: 256,
            },
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        let policy = ResolvedExportPolicy::new(
            output.clone(),
            ResolvedLayout::FlatDetailed,
            ResolvedActivityPolicy {
                detector: ActivityDetectorKind::ExactZero,
                channels: ["channel-0", "channel-1"]
                    .map(|name| ChannelActivityPolicy {
                        name: name.to_string(),
                        mode: ChannelExportMode::Auto,
                        threshold: None,
                    })
                    .to_vec(),
                whole_export_exact_zero_gate: false,
                trim_leading_silence: false,
            },
        )
        .unwrap();
        let coordinator =
            DumpCoordinator::with_frozen_decision(FrozenExportDecision::new(&plan).unwrap());
        coordinator
            .persist_policy_with_delivery(
                &arena,
                &mut workspace,
                PolicyPersistenceRequest {
                    command: ExportCommand::Recall,
                    policy: &policy,
                    profile: "control-json",
                    staging_root: &root.path().join("staging"),
                    timestamp: "20260818T120000",
                },
                Duration::from_secs(2),
                Duration::from_secs(2),
                |outcome| {
                    let CommittedPersistenceRef::Written {
                        range,
                        export_range,
                        frames,
                        losses,
                        output: completed,
                    } = outcome
                    else {
                        panic!("must write")
                    };
                    assert_eq!(completed.files.len(), 2);
                    let expected_files: Vec<_> = completed
                        .files
                        .iter()
                        .map(|file| file.final_path().to_path_buf())
                        .collect();
                    let mut bytes = Vec::new();
                    write_persistence_response(
                        &mut bytes,
                        true,
                        "written fixture",
                        &persistence_status(),
                        100,
                        CommittedPersistenceRef::Written {
                            range,
                            export_range,
                            frames,
                            losses,
                            output: completed,
                        },
                    )
                    .unwrap();
                    assert!(bytes.ends_with(b"\n"));
                    let response: ControlResponse = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(response.threshold_report, None);
                    assert_eq!(response.status, Some(persistence_status()));
                    assert_eq!(response.message, "written fixture");
                    assert_eq!(
                        response.persistence_outcome,
                        Some(PersistenceOutcomeResponse::Written {
                            start_frame: 0,
                            end_frame: 4,
                            frames: 4,
                            export_start_frame: 0,
                            export_frames: 4,
                            duration_seconds: 0.04,
                            lost_frames: 0,
                            retention_lost_frames: 0,
                            cleared_frames: 0,
                            capture_dropped_frames: 0,
                            output_directory: output,
                            files: expected_files
                        })
                    );
                    Ok(())
                },
            )
            .unwrap();
    }

    #[test]
    fn borrowed_persistence_json_rejects_zero_rate_and_inconsistent_ranges() {
        let mut bytes = Vec::new();
        let zero_rate = write_persistence_response(
            &mut bytes,
            true,
            "x",
            &persistence_status(),
            0,
            CommittedPersistenceRef::NoNewAudio {
                losses: LossBreakdown::default(),
            },
        )
        .unwrap_err();
        assert!(zero_rate.to_string().contains("sample rate"));
        let bad_range = write_persistence_response(
            &mut bytes,
            true,
            "x",
            &persistence_status(),
            100,
            CommittedPersistenceRef::SkippedSilent {
                range: FrameRange { start: 2, end: 3 },
                frames: 2,
                losses: LossBreakdown::default(),
            },
        )
        .unwrap_err();
        assert!(bad_range.to_string().contains("consumed range"));
    }

    #[test]
    fn formats_written_persistence_outcome_with_paths_and_loss_warning() {
        let outcome = PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 350,
            frames: 250,
            export_start_frame: 100,
            export_frames: 250,
            duration_seconds: 2.5,
            lost_frames: 25,
            retention_lost_frames: 25,
            cleared_frames: 0,
            capture_dropped_frames: 0,
            output_directory: PathBuf::from("/tmp/out/20260818120000"),
            files: vec![
                PathBuf::from("/tmp/out/20260818120000/mic.wav"),
                PathBuf::from("/tmp/out/20260818120000/gtr.wav"),
            ],
        };

        assert_eq!(
            format_persistence_outcome(&outcome),
            concat!(
                "written 250 frames (2.500 seconds), source frames 100..350\n",
                "warning: 25 frames were lost before persistence (retention 25)\n",
                "output directory: /tmp/out/20260818120000\n",
                "/tmp/out/20260818120000/mic.wav\n",
                "/tmp/out/20260818120000/gtr.wav"
            )
        );
    }

    #[test]
    fn formats_written_loss_causes_individually_and_suppresses_zeroes() {
        let outcome = PersistenceOutcomeResponse::Written {
            start_frame: 0,
            end_frame: 100,
            frames: 100,
            export_start_frame: 0,
            export_frames: 100,
            duration_seconds: 1.0,
            lost_frames: 30,
            retention_lost_frames: 10,
            cleared_frames: 20,
            capture_dropped_frames: 0,
            output_directory: PathBuf::from("/tmp/out"),
            files: vec![],
        };

        let rendered = format_persistence_outcome(&outcome);
        assert!(
            rendered.contains(
                "warning: 30 frames were lost before persistence (retention 10, cleared 20)"
            ),
            "got: {rendered}"
        );
        assert!(!rendered.contains("capture-dropped"), "got: {rendered}");
    }

    #[test]
    fn formats_no_new_audio_with_mixed_loss_causes() {
        let rendered = format_persistence_outcome(&PersistenceOutcomeResponse::NoNewAudio {
            lost_frames: 7,
            retention_lost_frames: 0,
            cleared_frames: 3,
            capture_dropped_frames: 4,
        });
        assert!(
            rendered.contains(
                "no new audio\nwarning: 7 frames were lost before persistence (cleared 3, capture-dropped 4)"
            ),
            "got: {rendered}"
        );
    }

    #[test]
    fn formats_skipped_silent_persistence_outcome() {
        let outcome = PersistenceOutcomeResponse::SkippedSilent {
            start_frame: 350,
            end_frame: 450,
            frames: 100,
            duration_seconds: 1.0,
            lost_frames: 0,
            retention_lost_frames: 0,
            cleared_frames: 0,
            capture_dropped_frames: 0,
        };

        assert_eq!(
            format_persistence_outcome(&outcome),
            "skipped exact-zero audio: 100 frames (1.000 seconds), source frames 350..450"
        );
    }

    #[test]
    fn formats_no_new_audio_persistence_outcome() {
        assert_eq!(
            format_persistence_outcome(&PersistenceOutcomeResponse::NoNewAudio {
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            }),
            "no new audio"
        );
    }

    #[test]
    fn persistence_outcomes_round_trip_policy_skip_and_written_export_range() {
        let skipped = PersistenceOutcomeResponse::SkippedByPolicy {
            start_frame: 100,
            end_frame: 250,
            frames: 150,
            duration_seconds: 1.5,
            lost_frames: 0,
            retention_lost_frames: 0,
            cleared_frames: 0,
            capture_dropped_frames: 0,
        };
        assert_eq!(
            serde_json::from_str::<PersistenceOutcomeResponse>(
                &serde_json::to_string(&skipped).unwrap()
            )
            .unwrap(),
            skipped
        );

        let written = PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 250,
            frames: 150,
            export_start_frame: 120,
            export_frames: 130,
            duration_seconds: 1.5,
            lost_frames: 0,
            retention_lost_frames: 0,
            cleared_frames: 0,
            capture_dropped_frames: 0,
            output_directory: PathBuf::from("/tmp/out"),
            files: vec![],
        };
        let PersistenceOutcomeResponse::Written {
            start_frame,
            end_frame,
            export_start_frame,
            export_frames,
            ..
        } = serde_json::from_str::<PersistenceOutcomeResponse>(
            &serde_json::to_string(&written).unwrap(),
        )
        .unwrap()
        else {
            panic!("written response must round-trip");
        };
        assert!(export_start_frame >= start_frame);
        assert_eq!(export_start_frame + export_frames, end_frame);
    }

    #[test]
    fn old_written_json_defaults_export_range_to_the_consumed_range() {
        let old = r#"{"kind":"written","start_frame":100,"end_frame":250,"frames":150,"duration_seconds":1.5,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        assert_eq!(
            serde_json::from_str::<PersistenceOutcomeResponse>(old).unwrap(),
            PersistenceOutcomeResponse::Written {
                start_frame: 100,
                end_frame: 250,
                frames: 150,
                export_start_frame: 100,
                export_frames: 150,
                duration_seconds: 1.5,
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
                output_directory: PathBuf::from("/tmp/out"),
                files: vec![],
            }
        );
    }

    #[test]
    fn written_json_rejects_only_export_start_frame() {
        let payload = r#"{"kind":"written","start_frame":100,"end_frame":250,"frames":150,"export_start_frame":120,"duration_seconds":1.5,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("both"), "{error}");
    }

    #[test]
    fn written_json_rejects_only_export_frames() {
        let payload = r#"{"kind":"written","start_frame":100,"end_frame":250,"frames":150,"export_frames":130,"duration_seconds":1.5,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("both"), "{error}");
    }

    #[test]
    fn written_json_rejects_export_start_before_consumed_range() {
        let payload = r#"{"kind":"written","start_frame":100,"end_frame":250,"frames":150,"export_start_frame":99,"export_frames":151,"duration_seconds":1.5,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("starts before"), "{error}");
    }

    #[test]
    fn written_json_rejects_export_range_add_overflow() {
        let payload = r#"{"kind":"written","start_frame":0,"end_frame":1,"frames":1,"export_start_frame":18446744073709551615,"export_frames":1,"duration_seconds":1.0,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("overflow"), "{error}");
    }

    #[test]
    fn written_json_rejects_export_range_not_ending_at_consumed_end() {
        let payload = r#"{"kind":"written","start_frame":100,"end_frame":250,"frames":150,"export_start_frame":120,"export_frames":129,"duration_seconds":1.5,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("does not end"), "{error}");
    }

    #[test]
    fn written_json_rejects_reversed_consumed_range() {
        let payload = r#"{"kind":"written","start_frame":250,"end_frame":100,"frames":0,"duration_seconds":0.0,"lost_frames":0,"output_directory":"/tmp/out","files":[]}"#;
        let error = serde_json::from_str::<PersistenceOutcomeResponse>(payload).unwrap_err();
        assert!(error.to_string().contains("consumed range"), "{error}");
    }

    #[test]
    fn streaming_persistence_client_matches_owned_format_with_escaped_paths() {
        use serde::de::DeserializeSeed;

        let output_directory = PathBuf::from("/tmp/out/quoted\"directory");
        let files = vec![
            output_directory.join("first\nchannel.wav"),
            output_directory.join("second\\channel.wav"),
        ];
        let outcome = PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 350,
            frames: 250,
            export_start_frame: 100,
            export_frames: 250,
            duration_seconds: 2.5,
            lost_frames: 25,
            retention_lost_frames: 10,
            cleared_frames: 5,
            capture_dropped_frames: 10,
            output_directory,
            files,
        };
        let response = ControlResponse {
            ok: true,
            message: "written fixture".to_string(),
            status: None,
            persistence_outcome: Some(outcome.clone()),
            threshold_report: None,
        };
        let json = serde_json::to_vec(&response).unwrap();
        let mut output = Vec::new();
        let mut deserializer = serde_json::Deserializer::from_slice(&json);

        let _: () = PersistenceClientResponseSeed::new(&mut output)
            .deserialize(&mut deserializer)
            .unwrap()
            .unwrap();

        let expected = format!("{}\n", format_persistence_outcome(&outcome));
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[test]
    fn streaming_persistence_client_accepts_long_success_and_error_messages() {
        use serde::de::DeserializeSeed;

        let message = format!(
            "escaped quote \" newline\n backslash \\ {}",
            "x".repeat(DEFAULT_MAXIMUM_PATH_BYTES as usize)
        );
        for ok in [true, false] {
            let response = ControlResponse {
                ok,
                message: message.clone(),
                status: None,
                persistence_outcome: None,
                threshold_report: None,
            };
            let json = serde_json::to_vec(&response).unwrap();
            let mut output = Vec::new();
            let mut deserializer = serde_json::Deserializer::from_slice(&json);

            let result = PersistenceClientResponseSeed::new(&mut output)
                .deserialize(&mut deserializer)
                .unwrap();

            if ok {
                result.unwrap();
                assert_eq!(output, format!("{message}\n").as_bytes());
            } else {
                assert_eq!(result.unwrap_err(), message);
                assert!(output.is_empty());
            }
        }
    }

    #[test]
    fn streaming_persistence_client_rejects_malformed_and_oversized_paths() {
        use serde::de::DeserializeSeed;

        let malformed = br#"{"ok":true,"message":"x","status":null,"persistence_outcome":{"kind":"written","start_frame":0,"end_frame":1,"frames":1,"export_start_frame":0,"export_frames":1,"duration_seconds":0.01,"lost_frames":0,"retention_lost_frames":0,"cleared_frames":0,"capture_dropped_frames":0,"output_directory":"/tmp/out","files":["/tmp/bad\q"]}}"#;
        let mut output = Vec::new();
        let mut deserializer = serde_json::Deserializer::from_slice(malformed);
        assert!(PersistenceClientResponseSeed::new(&mut output)
            .deserialize(&mut deserializer)
            .is_err());

        let oversized = "x".repeat(
            usize::try_from(crate::capture_runtime::DEFAULT_MAXIMUM_PATH_BYTES).unwrap() + 1,
        );
        let json = format!(
            r#"{{"ok":true,"message":"x","status":null,"persistence_outcome":{{"kind":"written","start_frame":0,"end_frame":1,"frames":1,"export_start_frame":0,"export_frames":1,"duration_seconds":0.01,"lost_frames":0,"retention_lost_frames":0,"cleared_frames":0,"capture_dropped_frames":0,"output_directory":"/tmp/out","files":[{}]}}}}"#,
            serde_json::to_string(&oversized).unwrap()
        );
        let mut output = Vec::new();
        let mut deserializer = serde_json::Deserializer::from_str(&json);
        let error = PersistenceClientResponseSeed::new(&mut output)
            .deserialize(&mut deserializer)
            .unwrap_err();
        assert!(error.to_string().contains("maximum path bytes"), "{error}");
    }
}
