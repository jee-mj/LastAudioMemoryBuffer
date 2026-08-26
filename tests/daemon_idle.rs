use lamb::app_config::default_config_text;
use std::ffi::CString;
use std::fs;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

struct SocketWatch {
    socket: std::path::PathBuf,
    ready: mpsc::Receiver<bool>,
    thread: JoinHandle<()>,
}

impl SocketWatch {
    fn arm(socket: &std::path::Path) -> Self {
        let directory = socket.parent().unwrap();
        let directory = CString::new(directory.as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        assert!(fd >= 0);
        assert!(
            unsafe {
                libc::inotify_add_watch(fd, directory.as_ptr(), libc::IN_CREATE | libc::IN_MOVED_TO)
            } >= 0
        );
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut event = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let observed =
                unsafe { libc::poll(&mut event, 1, PROCESS_TIMEOUT.as_millis() as libc::c_int) }
                    == 1;
            unsafe {
                libc::close(fd as RawFd);
            }
            ready_tx.send(observed).unwrap();
        });
        Self {
            socket: socket.to_path_buf(),
            ready,
            thread,
        }
    }

    fn wait(self, child: &mut Child, label: &str) {
        let observed = self
            .ready
            .recv_timeout(PROCESS_TIMEOUT + Duration::from_secs(1))
            .expect("socket watch did not finish");
        self.thread.join().unwrap();
        if !observed || !self.socket.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{label} did not create control socket");
        }
    }
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

fn wait_child_bounded(mut child: Child) {
    let pid = child.id();
    let (completed_tx, completed_rx) = mpsc::sync_channel(1);
    let watchdog = thread::spawn(move || {
        if completed_rx.recv_timeout(PROCESS_TIMEOUT).is_err() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    });
    let status = child.wait().unwrap();
    let _ = completed_tx.send(());
    watchdog.join().unwrap();
    assert!(status.success(), "daemon exited with {status}");
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
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&missing_config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

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
    wait_child_bounded(child);
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
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

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
    wait_child_bounded(child);
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
    let mut child = Command::new(exe)
        .arg("daemon")
        .arg("--config")
        .arg(&config)
        .env("XDG_RUNTIME_DIR", &runtime)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

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
    wait_child_bounded(child);
}
