use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lamb::activity::ThresholdSource;
use lamb::calibration::{ConfiguredDeviceSelector, InputBackend, LiveDeviceKeyKind, StaleReason};
use lamb::control::{
    CalibrationEvaluation, CalibrationReportStatus, ConfiguredInputReport, ControlRequest,
    ControlResponse, LiveInputReport, StoredThresholdReport, ThresholdChannelReport,
    ThresholdReport, ThresholdRequest,
};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const CHILD_TIMEOUT: Duration = Duration::from_secs(5);

struct TestServer {
    finished: mpsc::Receiver<()>,
    thread: JoinHandle<()>,
}

impl TestServer {
    fn finish(self) {
        self.finished
            .recv_timeout(IO_TIMEOUT)
            .expect("test server did not finish");
        self.thread.join().unwrap();
    }
}

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lamb"));
    command.args(args).env_remove("XDG_RUNTIME_DIR");
    command
}

fn response(ok: bool, message: &str, report: Option<ThresholdReport>) -> ControlResponse {
    ControlResponse {
        ok,
        message: message.to_string(),
        status: None,
        persistence_outcome: None,
        threshold_report: report,
    }
}

fn serve_one(socket: &Path, expected: ControlRequest, response: ControlResponse) -> TestServer {
    let listener = UnixListener::bind(socket).unwrap();
    let (finished_tx, finished) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        let mut ready = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut ready, 1, IO_TIMEOUT.as_millis() as libc::c_int) },
            1,
            "client did not connect to test server"
        );
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        stream.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        assert_eq!(
            serde_json::from_str::<ControlRequest>(request.trim_end()).unwrap(),
            expected
        );
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&response).unwrap()
        )
        .unwrap();
        finished_tx.send(()).unwrap();
    });
    TestServer { finished, thread }
}

fn output_bounded(mut command: Command) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let pid = child.id();
    let (completed_tx, completed_rx) = mpsc::sync_channel(1);
    let watchdog = thread::spawn(move || {
        if completed_rx.recv_timeout(CHILD_TIMEOUT).is_err() {
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

fn run_default(args: &[&str], expected: ThresholdRequest, response: ControlResponse) -> Output {
    let runtime = tempfile::tempdir().unwrap();
    let socket_dir = runtime.path().join("lamb");
    std::fs::create_dir(&socket_dir).unwrap();
    let server = serve_one(
        &socket_dir.join("control.sock"),
        ControlRequest::Threshold { request: expected },
        response,
    );
    let mut child = command(args);
    child.env("XDG_RUNTIME_DIR", runtime.path());
    let output = output_bounded(child);
    server.finish();
    output
}

fn run_override(args: &[&str], expected: ThresholdRequest, response: ControlResponse) -> Output {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("threshold.sock");
    let server = serve_one(
        &socket,
        ControlRequest::Threshold { request: expected },
        response,
    );
    let mut full_args = args.to_vec();
    full_args.extend(["--socket", socket.to_str().unwrap()]);
    let output = output_bounded(command(&full_args));
    server.finish();
    output
}

fn assert_success(output: &Output, message: &str) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(message),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn every_threshold_form_uses_the_runtime_default_and_sends_exact_nested_requests() {
    let cases = [
        (
            vec![
                "threshold",
                "calibrate",
                "--profile",
                "studio",
                "--channel",
                "mic",
            ],
            ThresholdRequest::Calibrate {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                seconds: 5,
            },
        ),
        (
            vec![
                "threshold",
                "set",
                "--profile",
                "studio",
                "--channel",
                "mic",
                "--dbfs",
                "-42.5",
            ],
            ThresholdRequest::Set {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                dbfs: -42.5,
            },
        ),
        (
            vec!["threshold", "show", "--profile", "studio"],
            ThresholdRequest::Show {
                profile: "studio".to_string(),
            },
        ),
        (
            vec![
                "threshold",
                "reset",
                "--profile",
                "studio",
                "--channel",
                "mic",
            ],
            ThresholdRequest::Reset {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
            },
        ),
    ];

    for (args, expected) in cases {
        let output = run_default(&args, expected, response(true, "daemon accepted", None));
        assert_success(&output, "daemon accepted");
    }
}

#[test]
fn socket_override_works_for_every_form_and_calibration_accepts_exact_bounds() {
    let cases = [
        (
            vec![
                "threshold",
                "calibrate",
                "--profile",
                "studio",
                "--channel",
                "mic",
                "--seconds",
                "1",
            ],
            ThresholdRequest::Calibrate {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                seconds: 1,
            },
        ),
        (
            vec![
                "threshold",
                "calibrate",
                "--profile",
                "studio",
                "--channel",
                "mic",
                "--seconds",
                "30",
            ],
            ThresholdRequest::Calibrate {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                seconds: 30,
            },
        ),
        (
            vec![
                "threshold",
                "set",
                "--profile",
                "studio",
                "--channel",
                "mic",
                "--dbfs",
                "-42.5",
            ],
            ThresholdRequest::Set {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
                dbfs: -42.5,
            },
        ),
        (
            vec!["threshold", "show", "--profile", "studio"],
            ThresholdRequest::Show {
                profile: "studio".to_string(),
            },
        ),
        (
            vec![
                "threshold",
                "reset",
                "--profile",
                "studio",
                "--channel",
                "mic",
            ],
            ThresholdRequest::Reset {
                profile: "studio".to_string(),
                channel: "mic".to_string(),
            },
        ),
    ];

    for (args, expected) in cases {
        let output = run_override(&args, expected, response(true, "override accepted", None));
        assert_success(&output, "override accepted");
    }
}

#[test]
fn threshold_cli_rejects_every_missing_operation_argument_and_out_of_range_duration() {
    let invalid: &[&[&str]] = &[
        &["threshold", "calibrate", "--channel", "mic"],
        &["threshold", "calibrate", "--profile", "studio"],
        &["threshold", "set", "--channel", "mic", "--dbfs", "-42"],
        &["threshold", "set", "--profile", "studio", "--dbfs", "-42"],
        &[
            "threshold",
            "set",
            "--profile",
            "studio",
            "--channel",
            "mic",
        ],
        &["threshold", "show"],
        &[
            "threshold",
            "show",
            "--profile",
            "studio",
            "--channel",
            "mic",
        ],
        &["threshold", "reset", "--channel", "mic"],
        &["threshold", "reset", "--profile", "studio"],
        &[
            "threshold",
            "calibrate",
            "--profile",
            "studio",
            "--channel",
            "mic",
            "--seconds",
            "0",
        ],
        &[
            "threshold",
            "calibrate",
            "--profile",
            "studio",
            "--channel",
            "mic",
            "--seconds",
            "31",
        ],
    ];
    for args in invalid {
        let output = output_bounded(command(args));
        assert!(
            !output.status.success(),
            "args {args:?} unexpectedly succeeded"
        );
    }
}

#[test]
fn missing_runtime_directory_fails_before_connection_and_names_the_variable() {
    for args in [
        vec![
            "threshold",
            "calibrate",
            "--profile",
            "studio",
            "--channel",
            "mic",
        ],
        vec![
            "threshold",
            "set",
            "--profile",
            "studio",
            "--channel",
            "mic",
            "--dbfs",
            "-42",
        ],
        vec!["threshold", "show", "--profile", "studio"],
        vec![
            "threshold",
            "reset",
            "--profile",
            "studio",
            "--channel",
            "mic",
        ],
    ] {
        let output = output_bounded(command(&args));
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("XDG_RUNTIME_DIR"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn successful_typed_report_and_daemon_errors_are_visible_to_the_client() {
    let report = ThresholdReport {
        profile: "studio".to_string(),
        active_profile: true,
        capturing: true,
        channels: vec![
            ThresholdChannelReport {
                channel: "mic".to_string(),
                detector: "windowed-rms-peak".to_string(),
                detector_version: "windowed-rms-peak-v1".to_string(),
                configured_input: ConfiguredInputReport {
                    backend: InputBackend::Jack,
                    selector: ConfiguredDeviceSelector::JackSourceClient("system".to_string()),
                    source: "system:capture_1".to_string(),
                    input_id: "configured-input-v2".to_string(),
                },
                stored: Some(StoredThresholdReport {
                    threshold_dbfs: -41.25,
                    source: ThresholdSource::Calibrated,
                    updated_at_unix_seconds: 1_700_000_000,
                    age_seconds: Some(17),
                    calibration_id: Some("calibration-generation".to_string()),
                }),
                artifact_status: CalibrationReportStatus::Complete,
                current_live_identity: Some(LiveInputReport {
                    backend: InputBackend::Jack,
                    key_kind: LiveDeviceKeyKind::JackSourceClient,
                    key_value: "system".to_string(),
                    resolved_source: "system:capture_1".to_string(),
                }),
                configured_identity_matches: Some(true),
                calibration_evaluation: CalibrationEvaluation::Valid,
                effective_threshold_dbfs: Some(-41.25),
            },
            ThresholdChannelReport {
                channel: "missing-live".to_string(),
                detector: "exact-zero".to_string(),
                detector_version: "exact-zero-v1".to_string(),
                configured_input: ConfiguredInputReport {
                    backend: InputBackend::PipeWire,
                    selector: ConfiguredDeviceSelector::PipeWireAuto,
                    source: "auto".to_string(),
                    input_id: "configured-auto-v2".to_string(),
                },
                stored: Some(StoredThresholdReport {
                    threshold_dbfs: -55.0,
                    source: ThresholdSource::Manual,
                    updated_at_unix_seconds: 1_600_000_000,
                    age_seconds: None,
                    calibration_id: None,
                }),
                artifact_status: CalibrationReportStatus::Stale {
                    reason: StaleReason::MissingLiveIdentity,
                },
                current_live_identity: None,
                configured_identity_matches: None,
                calibration_evaluation: CalibrationEvaluation::Stale {
                    reason: StaleReason::MissingLiveIdentity,
                },
                effective_threshold_dbfs: None,
            },
        ],
        message: "typed threshold details".to_string(),
    };
    let success = run_override(
        &["threshold", "show", "--profile", "studio"],
        ThresholdRequest::Show {
            profile: "studio".to_string(),
        },
        response(true, "daemon report ready", Some(report.clone())),
    );
    assert_success(&success, "daemon report ready");
    let stdout = String::from_utf8(success.stdout).unwrap();
    let json_start = stdout.find('{').expect("pretty threshold report JSON");
    let actual: ThresholdReport = serde_json::from_str(&stdout[json_start..]).unwrap();
    assert_eq!(actual, report);

    let failure = run_override(
        &[
            "threshold",
            "reset",
            "--profile",
            "studio",
            "--channel",
            "missing",
        ],
        ThresholdRequest::Reset {
            profile: "studio".to_string(),
            channel: "missing".to_string(),
        },
        response(false, "daemon rejected missing channel", None),
    );
    assert!(!failure.status.success());
    assert!(
        String::from_utf8_lossy(&failure.stderr).contains("daemon rejected missing channel"),
        "stderr: {}",
        String::from_utf8_lossy(&failure.stderr)
    );
}
