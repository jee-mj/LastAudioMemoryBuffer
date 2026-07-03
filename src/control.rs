use crate::error::{io_error, LambError, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

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
    let response = send_request(socket, &ControlRequest::Dump)?;
    if response.ok {
        println!("{}", response.message);
        Ok(())
    } else {
        Err(LambError::Control(response.message))
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
