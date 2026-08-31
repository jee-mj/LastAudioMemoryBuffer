use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use lamb::control::{send_request, CaptureState, ControlRequest, DaemonState, ErrorClass};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn conflicting_channels_and_capture_ports_toml() -> &'static str {
    r#"
configVersion = 1
user = "test"
backend = "pipewire"
target = "studio-input"
channels = 4
capturePorts = [
  { source = "capture_AUX0", name = "mic" },
  { source = "capture_AUX1", name = "gtr" },
]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "__OUTPUT__"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "__SOCKET__"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
}

fn valid_fake_toml() -> &'static str {
    r#"
configVersion = 1
user = "test"
backend = "fake"
channels = 2
channelMap = ["left", "right"]
seconds = 5
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "__OUTPUT__"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "__SOCKET__"
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
}

fn legacy_toml_without_socket(socket_value: Option<&str>) -> String {
    let mut text = r#"
configVersion = 1
user = "test"
backend = "fake"
channels = 2
seconds = 5
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "__OUTPUT__"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlPermissions = "0600"

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#
    .to_string();
    if let Some(value) = socket_value {
        text.push_str("\ncontrolSocketPath = ");
        text.push_str(value);
        text.push('\n');
    }
    text
}

fn invalid_profile_toml() -> &'static str {
    r#"
[daemon]
startMode = "auto"
activeProfile = "studio"
controlSocketPath = "__SOCKET__"

[profiles.studio]
backend = "unsupported"
"#
}

struct SocketWatch {
    socket: PathBuf,
}

struct PendingChild {
    child: Option<Child>,
}

impl PendingChild {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }

    fn take(mut self) -> Child {
        self.child.take().unwrap()
    }
}

impl Drop for PendingChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl SocketWatch {
    fn arm(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    fn wait(self, child: &mut Child) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("daemon exited before socket readiness: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not publish {}", self.socket.display());
    }
}

struct DaemonProcessFixture {
    _temp: tempfile::TempDir,
    runtime: PathBuf,
    socket: PathBuf,
    config: PathBuf,
    child: Child,
}

impl DaemonProcessFixture {
    fn spawn(config_text: Option<&str>, configured_socket: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let socket = runtime.join("lamb/control.sock");
        let config = temp.path().join("config/lamb.toml");
        let output = temp.path().join("output");
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        fs::create_dir(&output).unwrap();
        if let Some(text) = config_text {
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            let text = if configured_socket {
                text.replace("__SOCKET__", socket.to_str().unwrap())
            } else {
                text.to_string()
            }
            .replace("__OUTPUT__", output.to_str().unwrap());
            fs::write(&config, text).unwrap();
        }
        let watch = SocketWatch::arm(&socket);
        let child = Command::new(env!("CARGO_BIN_EXE_lamb"))
            .arg("daemon")
            .arg("--config")
            .arg(&config)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("LAMB_SKIP_RUNTIME_VALIDATION", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut pending = PendingChild::new(child);
        watch.wait(pending.child_mut());
        let child = pending.take();
        Self {
            _temp: temp,
            runtime,
            socket,
            config,
            child,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn socket_identity(&self) -> (u64, u64) {
        let metadata = fs::symlink_metadata(&self.socket).unwrap();
        (metadata.dev(), metadata.ino())
    }

    fn status_json(&self) -> serde_json::Value {
        let output = output_bounded(
            Command::new(env!("CARGO_BIN_EXE_lamb"))
                .arg("status")
                .arg("--socket")
                .arg(&self.socket)
                .arg("--json"),
        );
        assert!(
            output.status.success(),
            "status stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn command(&self, command: &str) -> Output {
        output_bounded(
            Command::new(env!("CARGO_BIN_EXE_lamb"))
                .arg(command)
                .arg("--socket")
                .arg(&self.socket),
        )
    }

    fn replace_config(&self, text: &str) {
        let replacement = self.config.with_extension("replacement");
        fs::write(
            &replacement,
            text.replace("__SOCKET__", self.socket.to_str().unwrap())
                .replace(
                    "__OUTPUT__",
                    self._temp.path().join("output").to_str().unwrap(),
                ),
        )
        .unwrap();
        fs::rename(replacement, &self.config).unwrap();
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }

    fn stop_and_wait(mut self) -> ExitStatus {
        let stop = self.command("stop");
        assert!(
            stop.status.success(),
            "stop stderr: {}",
            String::from_utf8_lossy(&stop.stderr)
        );
        self.wait_bounded()
    }

    fn wait_bounded(&mut self) -> ExitStatus {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not exit within {PROCESS_TIMEOUT:?}");
    }
}

impl Drop for DaemonProcessFixture {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn output_bounded(command: &mut Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: stdout_reader.join().unwrap(),
        stderr: stderr_reader.join().unwrap(),
    }
}

fn assert_permanent_no_retry(status: &serde_json::Value) {
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert_eq!(status["errorClass"], "permanent");
    assert_eq!(status["retryPolicy"], "manual");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert_eq!(status["activeProfile"], serde_json::Value::Null);
    assert_eq!(status["resolvedTarget"], serde_json::Value::Null);
    assert_eq!(status["active_export_count"], 0);
    assert_eq!(status["pending_recall_count"], 0);
    assert_eq!(status["buffer_capacity_seconds"], 0.0);
    assert_eq!(status["retained_seconds"], 0.0);
    assert_eq!(status["dropped_frames"], 0);
    assert_eq!(status["resolved_target"], serde_json::Value::Null);
    assert_eq!(status["sample_rate"], 0);
    assert_eq!(status["channel_count"], 0);
    assert!(status["last_error"].is_string());
    assert_eq!(status["lastError"], status["last_error"]);
}

#[test]
fn permanent_validation_failure_keeps_pid_and_socket_alive() {
    let mut fixture =
        DaemonProcessFixture::spawn(Some(conflicting_channels_and_capture_ports_toml()), true);
    let pid = fixture.pid();
    let socket = fixture.socket_identity();
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert_eq!(status["state"], "faulted");
    assert_eq!(status["target"], serde_json::Value::Null);
    assert_eq!(status["format"], "");
    thread::sleep(Duration::from_secs(6));
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn status_reports_validation_error_and_no_retry() {
    let fixture =
        DaemonProcessFixture::spawn(Some(conflicting_channels_and_capture_ports_toml()), true);
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert_eq!(status["state"], "faulted");
    assert_eq!(status["target"], serde_json::Value::Null);
    assert_eq!(status["format"], "");
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("channels conflicts with capturePorts"));

    let rejected = send_request(
        &fixture.socket,
        &ControlRequest::StartCapture {
            profile: None,
            activate: false,
        },
    )
    .unwrap();
    assert!(!rejected.ok);
    assert_eq!(
        rejected.error_context.error_class,
        Some(ErrorClass::Permanent)
    );
    assert_eq!(
        rejected.error_context.daemon_state,
        Some(DaemonState::Degraded)
    );
    assert_eq!(
        rejected.error_context.capture_state,
        Some(CaptureState::Faulted)
    );
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn invalid_profile_reports_permanent_no_retry() {
    let fixture = DaemonProcessFixture::spawn(Some(invalid_profile_toml()), true);
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert_eq!(status["state"], "faulted");
    assert_eq!(status["target"], fixture.config.display().to_string());
    assert_eq!(status["format"], "");
    assert_eq!(status["activeProfile"], serde_json::Value::Null);
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("backend must be jack or pipewire, got unsupported"));
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn corrected_reload_recovers_on_same_pid_and_socket() {
    let fixture =
        DaemonProcessFixture::spawn(Some(conflicting_channels_and_capture_ports_toml()), true);
    let pid = fixture.pid();
    let socket = fixture.socket_identity();
    fixture.replace_config(valid_fake_toml());
    let reload = fixture.command("reload");
    assert!(
        reload.status.success(),
        "{}",
        String::from_utf8_lossy(&reload.stderr)
    );
    let status = fixture.status_json();
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);
    assert_eq!(status["daemonState"], "ready");
    assert_eq!(status["captureState"], "running");
    assert_eq!(status["state"], "capturing");
    assert_eq!(status["errorClass"], serde_json::Value::Null);
    assert_eq!(status["lastError"], serde_json::Value::Null);
    assert_eq!(status["retryPolicy"], "none");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert_eq!(status["resolved_target"], "fake");
    assert_eq!(status["resolvedTarget"], "fake");
    assert_eq!(status["target"], serde_json::Value::Null);
    assert_eq!(status["activeProfile"], serde_json::Value::Null);
    assert_eq!(status["active_export_count"], 0);
    assert_eq!(status["pending_recall_count"], 0);
    assert_eq!(status["buffer_capacity_seconds"], 5.0);
    let retained_seconds = status["retained_seconds"].as_f64().unwrap();
    assert!(retained_seconds.is_finite());
    assert!((0.0..=5.0).contains(&retained_seconds));
    assert_eq!(status["dropped_frames"], 0);
    assert_eq!(status["sample_rate"], 48_000);
    assert_eq!(status["channel_count"], 2);
    assert_eq!(status["format"], "F32LE");

    assert!(fixture.command("stop-capture").status.success());
    let stopped = fixture.status_json();
    assert_eq!(stopped["daemonState"], "ready");
    assert_eq!(stopped["captureState"], "stopped");
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);

    assert!(fixture.command("start-capture").status.success());
    let restarted = fixture.status_json();
    assert_eq!(restarted["daemonState"], "ready");
    assert_eq!(restarted["captureState"], "running");
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn stop_capture_keeps_daemon_and_socket_alive() {
    let mut fixture = DaemonProcessFixture::spawn(Some(valid_fake_toml()), true);
    let pid = fixture.pid();
    let socket = fixture.socket_identity();
    assert!(fixture.command("stop-capture").status.success());
    let status = fixture.status_json();
    assert_eq!(status["daemonState"], "ready");
    assert_eq!(status["captureState"], "stopped");
    assert_eq!(status["state"], "stopped");
    assert_eq!(status["retryPolicy"], "none");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert_eq!(status["errorClass"], serde_json::Value::Null);
    assert_eq!(status["lastError"], serde_json::Value::Null);
    assert_eq!(status["target"], serde_json::Value::Null);
    assert_eq!(status["format"], "F32LE");
    assert_eq!(status["active_export_count"], 0);
    assert_eq!(status["pending_recall_count"], 0);
    assert_eq!(status["buffer_capacity_seconds"], 0.0);
    assert_eq!(status["retained_seconds"], 0.0);
    assert_eq!(status["dropped_frames"], 0);
    assert_eq!(status["resolved_target"], serde_json::Value::Null);
    assert_eq!(status["resolvedTarget"], serde_json::Value::Null);
    assert_eq!(status["sample_rate"], 0);
    assert_eq!(status["channel_count"], 0);
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn daemon_stop_exits_zero_and_removes_socket() {
    let fixture = DaemonProcessFixture::spawn(Some(valid_fake_toml()), true);
    let socket = fixture.socket.clone();
    let status = fixture.stop_and_wait();
    assert!(status.success());
    assert!(!socket.exists());
}

#[test]
fn malformed_config_uses_default_socket_and_stays_inspectable() {
    let mut fixture = DaemonProcessFixture::spawn(Some("not = [valid\n"), false);
    assert_eq!(fixture.socket, fixture.runtime.join("lamb/control.sock"));
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert_eq!(status["state"], "unconfigured");
    assert_eq!(status["target"], fixture.config.display().to_string());
    assert_eq!(status["format"], "");
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("failed to parse"));
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn missing_config_uses_default_socket_and_stays_inspectable() {
    let mut fixture = DaemonProcessFixture::spawn(None, false);
    assert_eq!(fixture.socket, fixture.runtime.join("lamb/control.sock"));
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert_eq!(status["state"], "unconfigured");
    assert_eq!(status["target"], fixture.config.display().to_string());
    assert_eq!(status["format"], "");
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("config file not found"));
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn legacy_config_without_socket_uses_default_socket_and_stays_inspectable() {
    let mut fixture = DaemonProcessFixture::spawn(Some(&legacy_toml_without_socket(None)), false);
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("controlSocketPath"));
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn legacy_config_with_non_string_socket_uses_default_socket_and_stays_inspectable() {
    let mut fixture =
        DaemonProcessFixture::spawn(Some(&legacy_toml_without_socket(Some("42"))), false);
    let status = fixture.status_json();
    assert_permanent_no_retry(&status);
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("controlSocketPath"));
    assert!(fixture.try_wait().is_none());
    assert!(fixture.stop_and_wait().success());
}

#[test]
fn malformed_legacy_default_socket_recovers_after_reload_without_rebinding() {
    let fixture = DaemonProcessFixture::spawn(Some(&legacy_toml_without_socket(None)), false);
    let pid = fixture.pid();
    let socket = fixture.socket_identity();
    fixture.replace_config(valid_fake_toml());
    let reload = fixture.command("reload");
    assert!(
        reload.status.success(),
        "{}",
        String::from_utf8_lossy(&reload.stderr)
    );
    let status = fixture.status_json();
    assert_eq!(fixture.pid(), pid);
    assert_eq!(fixture.socket_identity(), socket);
    assert_eq!(status["captureState"], "running");
    assert_eq!(status["resolvedTarget"], "fake");
    assert!(fixture.stop_and_wait().success());
}

fn bind_failure_config(socket: &Path, output_root: &Path) -> String {
    valid_fake_toml()
        .replace("__SOCKET__", socket.to_str().unwrap())
        .replace("__OUTPUT__", output_root.to_str().unwrap())
}

#[test]
fn initial_bind_failure_exits_78() {
    let temp = tempfile::tempdir().unwrap();
    let parent_file = temp.path().join("not-a-directory");
    fs::write(&parent_file, b"unchanged").unwrap();
    let socket = parent_file.join("control.sock");
    let config = temp.path().join("lamb.toml");
    let output_root = temp.path().join("output");
    fs::create_dir(&output_root).unwrap();
    let rendered = bind_failure_config(&socket, &output_root);
    assert!(!rendered.contains("__SOCKET__"));
    assert!(!rendered.contains("__OUTPUT__"));
    assert!(rendered.contains(output_root.to_str().unwrap()));
    fs::write(&config, rendered).unwrap();
    let output = output_bounded(
        Command::new(env!("CARGO_BIN_EXE_lamb"))
            .arg("daemon")
            .arg("--config")
            .arg(&config)
            .env("LAMB_SKIP_RUNTIME_VALIDATION", "1"),
    );
    assert_eq!(output.status.code(), Some(78));
    assert_eq!(fs::read(parent_file).unwrap(), b"unchanged");
}

#[test]
fn stale_regular_path_exits_78_without_deleting_file() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("control.sock");
    let config = temp.path().join("lamb.toml");
    let output_root = temp.path().join("output");
    fs::create_dir(&output_root).unwrap();
    fs::write(&socket, b"foreign regular file").unwrap();
    let rendered = bind_failure_config(&socket, &output_root);
    assert!(!rendered.contains("__SOCKET__"));
    assert!(!rendered.contains("__OUTPUT__"));
    assert!(rendered.contains(output_root.to_str().unwrap()));
    fs::write(&config, rendered).unwrap();
    let output = output_bounded(
        Command::new(env!("CARGO_BIN_EXE_lamb"))
            .arg("daemon")
            .arg("--config")
            .arg(&config)
            .env("LAMB_SKIP_RUNTIME_VALIDATION", "1"),
    );
    assert_eq!(output.status.code(), Some(78));
    assert_eq!(fs::read(socket).unwrap(), b"foreign regular file");
}

#[test]
fn ordinary_non_daemon_failure_remains_exit_1() {
    let temp = tempfile::tempdir().unwrap();
    let output = output_bounded(
        Command::new(env!("CARGO_BIN_EXE_lamb"))
            .arg("status")
            .arg("--socket")
            .arg(temp.path().join("missing.sock")),
    );
    assert_eq!(output.status.code(), Some(1));
}
