use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        stdout.contains("exported"),
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

    let exported: Vec<_> = fs::read_dir(&out).unwrap().collect();
    assert!(!exported.is_empty(), "dump did not export files");
    let names: Vec<String> = exported
        .iter()
        .filter_map(|entry| {
            let path = entry.as_ref().unwrap().path();
            path.file_name().map(|n| n.to_string_lossy().to_string())
        })
        .collect();
    let joined = names.join(" ");
    assert!(
        joined.contains(".wav"),
        "dump should export .wav files, got: {joined}"
    );
    assert!(
        joined.contains("mic"),
        "expected 'mic' in filenames, got: {joined}"
    );
    assert!(
        joined.contains("gtr"),
        "expected 'gtr' in filenames, got: {joined}"
    );
}
