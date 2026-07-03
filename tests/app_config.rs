use lamb::app_config::{
    default_config_path_from_env, default_config_text, load_optional_config, write_default_config,
    AppConfig, ConfigLoadState,
};
use std::collections::BTreeMap;
use std::fs;

#[test]
fn default_config_text_parses_as_manual_unconfigured() {
    let cfg: AppConfig = toml::from_str(default_config_text()).unwrap();

    assert_eq!(cfg.daemon.start_mode, "manual");
    assert_eq!(cfg.daemon.active_profile, None);
    assert_eq!(cfg.profiles, BTreeMap::new());
}

#[test]
fn missing_config_loads_default_unconfigured_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing.toml");

    let loaded = load_optional_config(&path).unwrap();

    assert_eq!(loaded.state, ConfigLoadState::Missing);
    assert_eq!(loaded.error, None);
    assert_eq!(loaded.config.daemon.start_mode, "manual");
    assert_eq!(loaded.config.daemon.active_profile, None);
    assert!(loaded.config.profiles.is_empty());
}

#[test]
fn invalid_config_loads_default_unconfigured_state_with_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bad.toml");
    fs::write(&path, "not = [valid\n").unwrap();

    let loaded = load_optional_config(&path).unwrap();

    assert_eq!(loaded.state, ConfigLoadState::Invalid);
    assert_eq!(loaded.config.daemon.start_mode, "manual");
    assert_eq!(loaded.config.daemon.active_profile, None);
    assert!(loaded.config.profiles.is_empty());
    assert!(loaded.error.unwrap().contains("failed to parse"));
}

#[test]
fn default_config_path_prefers_xdg_config_home() {
    let temp = tempfile::tempdir().unwrap();
    let path = default_config_path_from_env(Some(temp.path().into()), None).unwrap();

    assert_eq!(path, temp.path().join("lamb/lamb.toml"));
}

#[test]
fn default_config_path_falls_back_to_home_dot_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = default_config_path_from_env(None, Some(temp.path().into())).unwrap();

    assert_eq!(path, temp.path().join(".config/lamb/lamb.toml"));
}

#[test]
fn write_default_config_refuses_overwrite_without_force() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("lamb.toml");
    fs::write(&path, "existing = true\n").unwrap();

    let err = write_default_config(&path, false).unwrap_err().to_string();

    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "existing = true\n");
}

#[test]
fn write_default_config_force_overwrites_and_creates_parent_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/lamb/lamb.toml");

    write_default_config(&path, true).unwrap();

    assert_eq!(fs::read_to_string(&path).unwrap(), default_config_text());
}
