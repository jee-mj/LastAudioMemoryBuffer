use lamb::app_config::default_config_text;
use std::fs;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn daemon_with_missing_app_config_starts_unconfigured_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let missing_config = temp.path().join("missing/lamb.toml");
    fs::create_dir_all(&runtime).unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&missing_config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_or_exit(&socket, &mut child, "missing config daemon");

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("config file not found"), "{body}");

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
fn daemon_with_default_app_config_starts_idle_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("lamb.toml");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(&config, default_config_text()).unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_or_exit(&socket, &mut child, "default config daemon");

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("no active profile configured"), "{body}");

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
fn daemon_with_invalid_app_config_starts_unconfigured_control_socket() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    let socket = runtime.join("lamb/control.sock");
    let config = temp.path().join("bad.toml");
    fs::create_dir_all(&runtime).unwrap();
    fs::write(&config, "not = [valid\n").unwrap();

    let exe = env!("CARGO_BIN_EXE_lamb");
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_socket_or_exit(&socket, &mut child, "invalid config daemon");

    let status = Command::new(exe)
        .arg("status")
        .arg("--socket")
        .arg(&socket)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let body = String::from_utf8(status.stdout).unwrap();
    assert!(body.contains("unconfigured"), "{body}");
    assert!(body.contains("failed to parse"), "{body}");

    let stop = Command::new(exe)
        .arg("stop")
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert!(stop.status.success());
    let _ = child.wait();
}

fn wait_for_socket_or_exit(socket: &std::path::Path, child: &mut std::process::Child, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("{label} exited before creating socket: {status}; stderr: {stderr}");
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(socket.exists(), "{label} did not create control socket");
}
