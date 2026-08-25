use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lamb::control::{send_request, ControlRequest, ControlResponse, PersistenceOutcomeResponse};

#[test]
fn persistence_written_response_round_trips_with_source_frame_metadata() {
    let response = ControlResponse {
        ok: true,
        message: "written".to_string(),
        status: None,
        persistence_outcome: Some(PersistenceOutcomeResponse::Written {
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
            output_directory: PathBuf::from("/tmp/out/20260818T120000"),
            files: vec![PathBuf::from("/tmp/out/20260818T120000/mic.wav")],
        }),
    };

    let encoded = serde_json::to_string(&response).unwrap();
    assert!(encoded.contains(r#""kind":"written""#), "{encoded}");
    let decoded: ControlResponse = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, response);
}

#[test]
fn persistence_non_written_responses_round_trip_as_successes() {
    let responses = [
        ControlResponse {
            ok: true,
            message: "skipped silent".to_string(),
            status: None,
            persistence_outcome: Some(PersistenceOutcomeResponse::SkippedSilent {
                start_frame: 350,
                end_frame: 450,
                frames: 100,
                duration_seconds: 1.0,
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            }),
        },
        ControlResponse {
            ok: true,
            message: "no new audio".to_string(),
            status: None,
            persistence_outcome: Some(PersistenceOutcomeResponse::NoNewAudio {
                lost_frames: 0,
                retention_lost_frames: 0,
                cleared_frames: 0,
                capture_dropped_frames: 0,
            }),
        },
    ];

    for response in responses {
        let encoded = serde_json::to_string(&response).unwrap();
        let decoded: ControlResponse = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded, response);
    }
}

#[test]
fn control_response_without_persistence_outcome_remains_compatible() {
    let response: ControlResponse =
        serde_json::from_str(r#"{"ok":true,"message":"status","status":null}"#).unwrap();

    assert_eq!(response.persistence_outcome, None);
}

#[test]
fn recall_then_dump_share_one_capture_session_cursor() {
    assert_persistence_commands_share_cursor(ControlRequest::Recall, ControlRequest::Dump);
}

#[test]
fn dump_then_recall_share_one_capture_session_cursor() {
    assert_persistence_commands_share_cursor(ControlRequest::Dump, ControlRequest::Recall);
}

fn assert_persistence_commands_share_cursor(first: ControlRequest, second: ControlRequest) {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let mut daemon = spawn_fake_daemon(temp.path(), &socket, &out);
    wait_for_socket(&mut daemon, &socket);
    wait_for_retained_audio(&mut daemon, &socket);

    let first_response = send_request(&socket, &first).unwrap();
    let second_response = send_request(&socket, &second).unwrap();
    let stop_response = send_request(&socket, &ControlRequest::Stop).unwrap();
    assert!(stop_response.ok, "stop failed: {}", stop_response.message);
    let _ = daemon.wait();

    assert!(
        first_response.ok,
        "first command failed: {first_response:?}"
    );
    assert!(
        second_response.ok,
        "second command failed: {second_response:?}"
    );
    let first_end = match first_response.persistence_outcome {
        Some(PersistenceOutcomeResponse::Written {
            start_frame,
            end_frame,
            frames,
            export_start_frame,
            export_frames,
            files,
            ..
        }) => {
            assert_eq!(end_frame - start_frame, frames);
            assert!(export_start_frame >= start_frame);
            assert_eq!(export_start_frame + export_frames, end_frame);
            assert!(!files.is_empty());
            assert!(files
                .iter()
                .all(|path| path.starts_with(&out) && path.is_file()));
            end_frame
        }
        outcome => panic!("first command should write captured audio, got {outcome:?}"),
    };
    match second_response.persistence_outcome {
        Some(PersistenceOutcomeResponse::NoNewAudio { .. }) => {}
        Some(PersistenceOutcomeResponse::Written {
            start_frame,
            retention_lost_frames,
            ..
        }) => {
            assert!(
                start_frame >= first_end,
                "persistence ranges must never overlap: {start_frame} < {first_end}"
            );
            let gap = start_frame - first_end;
            if gap > 0 {
                assert!(
                    retention_lost_frames >= gap,
                    "a {gap}-frame gap must be reported as retention loss, got {retention_lost_frames}"
                );
            }
        }
        outcome => panic!("second command should report new or no audio, got {outcome:?}"),
    }
}

fn spawn_fake_daemon(root: &Path, socket: &Path, out: &Path) -> Child {
    let config = root.join("shared-cursor.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 5
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 1

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_lamb"))
        .arg("daemon")
        .arg("--config")
        .arg(config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn wait_for_socket(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");
}

fn wait_for_retained_audio(child: &mut Child, socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_observation = "no status response received".to_string();
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "daemon exited before reporting retained audio: {status}; last observation: {last_observation}"
            );
        }
        match send_request(socket, &ControlRequest::Status) {
            Ok(response) if !response.ok => {
                last_observation = format!("status command failed: {}", response.message);
            }
            Ok(response) => match response.status {
                Some(status) if status.retained_seconds > 0.0 => return,
                Some(status) => {
                    last_observation = format!(
                        "state={}, retained_seconds={}",
                        status.state, status.retained_seconds
                    );
                }
                None => last_observation = "status response omitted daemon status".to_string(),
            },
            Err(error) => last_observation = format!("status request failed: {error}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "daemon did not report retained audio before the 5-second deadline; last observation: {last_observation}"
    );
}

#[test]
fn fake_daemon_status_recall_clear_stop() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(status.status.success());
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("capturing"), "{body}");

    let recall = Command::new(exe)
        .arg("recall")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(recall.status.success());
    assert!(
        String::from_utf8_lossy(&recall.stdout).contains("written"),
        "recall should render its persistence outcome"
    );

    let clear = Command::new(exe)
        .arg("clear")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(clear.status.success());

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();

    let exported: Vec<_> = fs::read_dir(&out).unwrap().collect();
    assert!(!exported.is_empty(), "recall did not export files");
}

#[test]
fn fake_daemon_runtime_validation_does_not_require_pipewire_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();
}

#[test]
fn daemon_expands_percent_t_control_socket_under_runtime_dir() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = []
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "%t/lamb/control.sock"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("daemon exited before creating expanded socket: {status}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        socket.exists(),
        "daemon did not create expanded control socket"
    );

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "<USERNAME>".to_string())
}

#[test]
fn dump_request_round_trips() {
    let request = lamb::control::ControlRequest::Dump;
    let encoded = serde_json::to_string(&request).unwrap();
    assert_eq!(encoded, r#"{"command":"dump"}"#);
    let decoded: lamb::control::ControlRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, lamb::control::ControlRequest::Dump);
}

#[test]
fn fake_daemon_dump_exports_files_with_iso8601_timestamp_and_channel_names() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 2
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 25

[memory]
headroom = 1.25

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "daemon did not create control socket");

    thread::sleep(Duration::from_millis(500));

    let dump = Command::new(exe)
        .arg("dump")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(
        dump.status.success(),
        "dump failed: stderr={}",
        String::from_utf8_lossy(&dump.stderr)
    );

    let stdout = String::from_utf8(dump.stdout).unwrap();
    assert!(
        stdout.contains("written"),
        "dump output unexpected: {stdout}"
    );

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();

    let exported: Vec<_> = fs::read_dir(&out)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        exported.len(),
        1,
        "dump should publish one directory, got {exported:?}"
    );
    assert!(exported[0].is_dir(), "dump output should be a directory");
    let timestamp = exported[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(timestamp.len(), 14, "timestamp should be ISO-8601 compact");
    assert!(timestamp.chars().all(|c| c.is_ascii_digit()));
    let names: Vec<String> = fs::read_dir(&exported[0])
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let joined = names.join(" ");
    assert!(
        joined.contains(".wav"),
        "dump should export WAV files, got: {joined}"
    );
    assert!(names.contains(&"mic.wav".to_string()), "got: {joined}");
    assert!(names.contains(&"gtr.wav".to_string()), "got: {joined}");
}

#[test]
fn tight_memory_max_fails_before_capture_or_socket_startup() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let out = temp.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let config = temp.path().join("lamb.toml");
    fs::write(
        &config,
        format!(
            r#"
configVersion = 1
user = "{}"
channels = 2
channelMap = ["mic", "gtr"]
seconds = 5
sampleRate = 100
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
backend = "fake"
chunkFrames = 1

[memory]
headroom = 1.25
max = 100

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 3900000000
"#,
            whoami(),
            out.display(),
            socket.display()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "daemon should refuse a plan that exceeds memory.max"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("memory"),
        "startup error should mention memory, got: {stderr}"
    );
    assert!(
        !socket.exists(),
        "control socket must not be created when memory validation fails"
    );
}
