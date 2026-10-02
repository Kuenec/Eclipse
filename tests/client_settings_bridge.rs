#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const RUN_LIMIT: Duration = Duration::from_secs(60);

const BRIDGE_INACTIVE: &str = "client-settings bridge did not load";
const SEARCH_PATH_SEPARATOR: &str = "contains a colon or semicolon";

fn sandbox_without_config(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-settings-bridge-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("create the sandbox directory");
    root
}

fn sandbox(tag: &str) -> PathBuf {
    let root = sandbox_without_config(tag);
    let config = root.join("config").join("eclipse");
    std::fs::create_dir_all(&config).expect("create the sandbox config directory");
    std::fs::write(
        config.join("config.json"),
        br#"{"fflags":{"FFlagEclipseBridgeTest":true}}"#,
    )
    .expect("write a config with a Fast Flag");
    root
}

fn run_missing_apk(root: &Path, app_data: &Path, redirect_active: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eclipse"));
    command
        .arg("run")
        .arg(root.join("missing.apk"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("ECLIPSE_APP_DATA_DIR", app_data)
        .env_remove("LD_PRELOAD")
        .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH");
    if redirect_active {
        command.env("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE", "1");
    } else {
        command.env_remove("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE");
    }
    bounded_child::output(&mut command, RUN_LIMIT)
}

#[test]
fn app_data_paths_that_ld_library_path_would_split_are_refused_before_restarting() {
    for (tag, directory) in [("colon", "games:eclipse"), ("semicolon", "games;eclipse")] {
        let root = sandbox(tag);
        let output = run_missing_apk(&root, &root.join(directory).join("eclipse"), false);
        let stderr = String::from_utf8_lossy(&output.stderr);
        std::fs::remove_dir_all(&root).ok();

        assert!(!output.status.success(), "{tag}: {stderr}");
        assert!(stderr.contains(SEARCH_PATH_SEPARATOR), "{tag}: {stderr}");
        assert!(stderr.contains("ECLIPSE_APP_DATA_DIR"), "{tag}: {stderr}");
        assert!(!stderr.contains("eclipse ART startup"), "{tag}: {stderr}");
    }
}

#[test]
fn a_restarted_run_without_the_bridge_stops_before_starting_android() {
    let root = sandbox("inactive");
    let output = run_missing_apk(&root, &root.join("app-data"), true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    std::fs::remove_dir_all(&root).ok();

    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(BRIDGE_INACTIVE), "{stderr}");
    assert!(!stderr.contains("eclipse ART startup"), "{stderr}");
}

#[test]
fn plain_and_spaced_app_data_paths_load_the_bridge_and_continue() {
    for (tag, directory) in [("plain", "app-data"), ("space", "My Games")] {
        let root = sandbox(tag);
        let output = run_missing_apk(&root, &root.join(directory).join("eclipse"), false);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        std::fs::remove_dir_all(&root).ok();

        assert!(
            stdout.contains("Roblox Fast Flags staged at"),
            "{tag}: {stdout}"
        );
        assert!(!stderr.contains(SEARCH_PATH_SEPARATOR), "{tag}: {stderr}");
        assert!(!stderr.contains(BRIDGE_INACTIVE), "{tag}: {stderr}");
        assert!(
            !stderr.contains("Android settings setup"),
            "{tag}: {stderr}"
        );
    }
}

#[test]
fn without_user_fflags_a_spaced_app_data_path_stages_the_default_flag_and_continues() {
    let root = sandbox_without_config("defaults");
    let app_data = root.join("My Games").join("eclipse");
    let output = run_missing_apk(&root, &app_data, false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let staged = std::fs::read(app_data.join("runtime").join("ClientAppSettings.json"));
    std::fs::remove_dir_all(&root).ok();

    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert!(!stderr.contains(BRIDGE_INACTIVE), "{stderr}");
    assert!(!stderr.contains("Android settings setup"), "{stderr}");
    let staged: serde_json::Value =
        serde_json::from_slice(&staged.expect("read the staged client settings"))
            .expect("parse the staged client settings");
    assert_eq!(
        staged,
        serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "True"})
    );
}
