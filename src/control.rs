use crate::error::{io_error, LambError, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlResponse {
    pub ok: bool,
    pub message: String,
    pub status: Option<DaemonStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence_outcome: Option<PersistenceOutcomeResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistenceOutcomeResponse {
    Written {
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

pub fn client_dump(socket: &Path) -> Result<()> {
    client_persistence(socket, ControlRequest::Dump)
}

pub fn client_recall(socket: &Path) -> Result<()> {
    client_persistence(socket, ControlRequest::Recall)
}

fn client_persistence(socket: &Path, request: ControlRequest) -> Result<()> {
    let response = send_request(socket, &request)?;
    if !response.ok {
        return Err(LambError::Control(response.message));
    }

    match response.persistence_outcome {
        Some(outcome) => println!("{}", format_persistence_outcome(&outcome)),
        None => println!("{}", response.message),
    }
    Ok(())
}

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

fn format_persistence_outcome(outcome: &PersistenceOutcomeResponse) -> String {
    match outcome {
        PersistenceOutcomeResponse::Written {
            start_frame,
            end_frame,
            frames,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_written_persistence_outcome_with_paths_and_loss_warning() {
        let outcome = PersistenceOutcomeResponse::Written {
            start_frame: 100,
            end_frame: 350,
            frames: 250,
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
}
