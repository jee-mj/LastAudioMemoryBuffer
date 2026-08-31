use lamb::app_config::default_config_text;
use std::fs;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

struct SocketWatch {
    socket: std::path::PathBuf,
}

impl SocketWatch {
    fn arm(socket: &std::path::Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    fn wait(self, child: &mut ChildGuard, label: &str) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            if let Some(status) = child.child_mut().try_wait().unwrap() {
                panic!("{label} exited before control socket readiness: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{label} did not publish a connectable control socket");
    }
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: Some(command.spawn().unwrap()),
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }

    fn id(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn wait_success_bounded(&mut self) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child_mut().try_wait().unwrap() {
                assert!(status.success(), "daemon exited with {status}");
                self.child.take();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not exit within {PROCESS_TIMEOUT:?}");
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_daemon(
    exe: &str,
    config: &std::path::Path,
    runtime: &std::path::Path,
    skip_runtime_validation: bool,
) -> ChildGuard {
    let mut command = Command::new(exe);
    command
        .arg("daemon")
        .arg("--config")
        .arg(config)
        .env("XDG_RUNTIME_DIR", runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if skip_runtime_validation {
        command.env("LAMB_SKIP_RUNTIME_VALIDATION", "1");
    }
    ChildGuard::spawn(&mut command)
}

fn output_bounded(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let pid = child.id();
    let (completed_tx, completed_rx) = mpsc::sync_channel(1);
    let watchdog = thread::spawn(move || {
        if completed_rx.recv_timeout(PROCESS_TIMEOUT).is_err() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });
    let output = child.wait_with_output().unwrap();
    let _ = completed_tx.send(());
    watchdog.join().unwrap();
    output
}

fn legacy_config(socket: &std::path::Path, backend: &str, channels: u32) -> String {
    let output = socket.parent().unwrap().join("output");
    format!(
        r#"
configVersion = 1
user = "test"
backend = "{backend}"
target = "studio-input"
channels = {channels}
capturePorts = [
  {{ source = "capture_AUX0", name = "mic" }},
  {{ source = "capture_AUX1", name = "gtr" }},
]
channelMap = ["mic", "gtr"]
seconds = 30
sampleRate = 48000
sampleFormat = "F32LE"
dontRemix = true
outputDir = "{}"
maxActiveSnapshots = 1
allowQueuedRecall = false
controlSocketPath = "{}"
controlPermissions = "0600"
chunkFrames = 25

[memory]
headroom = 1.2

[export]
mode = "per-channel"
format = "wav"
splitWhenOverBytes = 1073741824
"#,
        output.display(),
        socket.display()
    )
}

#[test]
fn daemon_with_missing_app_config_starts_unconfigured_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let missing_config = temp.path().join("missing/lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let socket_watch = SocketWatch::arm(&socket);

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &missing_config, &runtime, false);

    socket_watch.wait(&mut child, "missing config daemon");

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status = output_bounded(status_command);
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("config file not found"), "{body}");

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    let stop = output_bounded(stop_command);
    assert!(stop.status.success());
    child.wait_success_bounded();
}

#[test]
fn daemon_with_default_app_config_starts_idle_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&config, default_config_text()).unwrap();
    let socket_watch = SocketWatch::arm(&socket);

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, false);

    socket_watch.wait(&mut child, "default config daemon");

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status = output_bounded(status_command);
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("no active profile configured"), "{body}");

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    let stop = output_bounded(stop_command);
    assert!(stop.status.success());
    child.wait_success_bounded();
}

#[test]
fn daemon_with_invalid_app_config_starts_unconfigured_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("bad.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&config, "not = [valid\n").unwrap();
    let socket_watch = SocketWatch::arm(&socket);

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, false);

    socket_watch.wait(&mut child, "invalid config daemon");

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status = output_bounded(status_command);
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("failed to parse"), "{body}");

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    let stop = output_bounded(stop_command);
    assert!(stop.status.success());
    child.wait_success_bounded();
}

#[test]
fn app_auto_start_fault_keeps_listener_inspectable() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            "[daemon]\nstartMode = \"auto\"\nactiveProfile = \"missing-profile\"\ncontrolSocketPath = \"{}\"\n\n[profiles]\n",
            socket.display()
        ),
    )
    .unwrap();
    let watch = SocketWatch::arm(&socket);
    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, false);
    watch.wait(&mut child, "app auto-start fault daemon");

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status_output = output_bounded(status_command);
    assert!(status_output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("missing-profile"));
    assert!(child.child_mut().try_wait().unwrap().is_none());

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    assert!(output_bounded(stop_command).status.success());
    child.wait_success_bounded();
}

#[test]
fn invalid_legacy_config_keeps_configured_socket_alive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::write(&config, legacy_config(&socket, "pipewire", 4)).unwrap();
    let watch = SocketWatch::arm(&socket);
    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, false);
    watch.wait(&mut child, "invalid legacy daemon");
    let pid = child.id();

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status_output = output_bounded(status_command);
    assert!(status_output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert_eq!(status["errorClass"], "permanent");
    assert_eq!(status["retryPolicy"], "manual");
    assert_eq!(status["retryAttempt"], 0);
    assert_eq!(status["nextRetryAt"], serde_json::Value::Null);
    assert!(status["lastError"]
        .as_str()
        .unwrap()
        .contains("channels conflicts with capturePorts"));
    assert_eq!(child.id(), pid);
    assert!(child.child_mut().try_wait().unwrap().is_none());

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    assert!(output_bounded(stop_command).status.success());
    child.wait_success_bounded();
    assert!(!socket.exists());
}

#[test]
fn schema_invalid_legacy_config_keeps_derived_socket_alive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let text = legacy_config(&socket, "fake", 2).replace("seconds = 30", "seconds = \"thirty\"");
    fs::write(&config, text).unwrap();
    let watch = SocketWatch::arm(&socket);
    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, true);
    watch.wait(&mut child, "schema-invalid legacy daemon");

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status_output = output_bounded(status_command);
    assert!(status_output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["daemonState"], "degraded");
    assert_eq!(status["captureState"], "faulted");
    assert_eq!(status["errorClass"], "permanent");
    assert!(status["lastError"].as_str().unwrap().contains("seconds"));
    assert!(child.child_mut().try_wait().unwrap().is_none());

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    assert!(output_bounded(stop_command).status.success());
    child.wait_success_bounded();
    assert!(!socket.exists());
}

#[test]
fn valid_legacy_fake_capture_can_stop_without_stopping_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let mut text = legacy_config(&socket, "fake", 2);
    text = text.replace(
        "capturePorts = [\n  { source = \"capture_AUX0\", name = \"mic\" },\n  { source = \"capture_AUX1\", name = \"gtr\" },\n]",
        "capturePorts = []",
    );
    fs::write(&config, text).unwrap();
    let watch = SocketWatch::arm(&socket);
    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = spawn_daemon(exe, &config, &runtime, true);
    watch.wait(&mut child, "valid legacy fake daemon");
    let pid = child.id();

    let mut status_command = Command::new(exe);
    status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let status_output = output_bounded(status_command);
    assert!(status_output.status.success());
    let status: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["daemonState"], "ready");
    assert_eq!(status["captureState"], "running");
    assert_eq!(status["state"], "capturing");
    assert_eq!(status["target"], "studio-input");
    assert_eq!(status["resolved_target"], "fake");
    assert_eq!(status["resolvedTarget"], "fake");
    assert_eq!(status["sample_rate"], 48000);
    assert_eq!(status["channel_count"], 2);
    assert_eq!(status["format"], "F32LE");

    let mut stop_capture_command = Command::new(exe);
    stop_capture_command
        .arg("stop-capture")
        .arg("--socket")
        .arg(&socket);
    let stop_capture = output_bounded(stop_capture_command);
    assert!(stop_capture.status.success());
    assert_eq!(child.id(), pid);
    assert!(child.child_mut().try_wait().unwrap().is_none());

    let mut stopped_status_command = Command::new(exe);
    stopped_status_command
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json");
    let stopped_output = output_bounded(stopped_status_command);
    assert!(stopped_output.status.success());
    let stopped: serde_json::Value = serde_json::from_slice(&stopped_output.stdout).unwrap();
    assert_eq!(stopped["daemonState"], "ready");
    assert_eq!(stopped["captureState"], "stopped");
    assert_eq!(stopped["state"], "stopped");
    assert_eq!(stopped["target"], "studio-input");
    assert_eq!(stopped["resolved_target"], serde_json::Value::Null);
    assert_eq!(stopped["resolvedTarget"], serde_json::Value::Null);
    assert_eq!(stopped["sample_rate"], 0);
    assert_eq!(stopped["channel_count"], 0);
    assert_eq!(stopped["format"], "F32LE");

    let mut stop_command = Command::new(exe);
    stop_command.arg("stop").arg("--socket").arg(&socket);
    assert!(output_bounded(stop_command).status.success());
    child.wait_success_bounded();
    assert!(!socket.exists());
}

#[test]
fn child_guard_kills_and_reaps_on_drop() {
    let mut command = Command::new("sh");
    command.arg("-c").arg("sleep 30");
    let pid;
    {
        let child = ChildGuard::spawn(&mut command);
        pid = child.id();
    }
    let deadline = std::time::Instant::now() + PROCESS_TIMEOUT;
    while std::time::Instant::now() < deadline {
        let exists = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        if !exists {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("ChildGuard did not reap child {pid}");
}
