use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BRIDGE_INACTIVE: &str = "client-settings bridge did not load";
const PRELOAD_SEPARATOR: &str = "contains a space or colon";

fn sandbox(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-settings-bridge-{tag}"));
    std::fs::remove_dir_all(&root).ok();
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
    command.output().expect("spawn eclipse run")
}

#[test]
fn app_data_paths_that_ld_preload_would_split_are_refused_before_restarting() {
    for (tag, directory) in [("space", "My Games"), ("colon", "games:eclipse")] {
        let root = sandbox(tag);
        let output = run_missing_apk(&root, &root.join(directory).join("eclipse"), false);
        let stderr = String::from_utf8_lossy(&output.stderr);
        std::fs::remove_dir_all(&root).ok();

        assert!(!output.status.success(), "{tag}: {stderr}");
        assert!(stderr.contains(PRELOAD_SEPARATOR), "{tag}: {stderr}");
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
fn a_plain_app_data_path_loads_the_bridge_and_continues() {
    let root = sandbox("plain");
    let output = run_missing_apk(&root, &root.join("app-data"), false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    std::fs::remove_dir_all(&root).ok();

    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert!(!stderr.contains(PRELOAD_SEPARATOR), "{stderr}");
    assert!(!stderr.contains(BRIDGE_INACTIVE), "{stderr}");
    assert!(!stderr.contains("Android settings setup"), "{stderr}");
}
