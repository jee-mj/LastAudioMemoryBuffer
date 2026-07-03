use lamb::app_config::default_config_text;
use std::fs;
use std::process::Command;

#[test]
fn config_path_prints_explicit_path() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .args(["config", "path", "--path"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", path.display())
    );
}

#[test]
fn config_init_writes_default_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config/lamb.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .args(["config", "init", "--path"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), default_config_text());
}

#[test]
fn config_init_refuses_overwrite_without_force() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    fs::write(&path, "existing = true\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .args(["config", "init", "--path"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "existing = true\n");
}

#[test]
fn config_init_force_overwrites_existing_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    fs::write(&path, "existing = true\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .args(["config", "init", "--force", "--path"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), default_config_text());
}

#[test]
fn config_show_prints_existing_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    fs::write(&path, default_config_text()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lamb"))
        .args(["config", "show", "--path"])
        .arg(&path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        default_config_text()
    );
}
