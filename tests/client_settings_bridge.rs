#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::io::{PipeReader, PipeWriter, Read as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const RUN_LIMIT: Duration = Duration::from_secs(60);

const BRIDGE_INACTIVE: &str = "client-settings bridge did not load";
const MISSING_APK: &str = "missing.apk";
const SEARCH_PATH_SEPARATOR: &str = "contains a colon or semicolon";
const UNSUPERVISED: &str = "must be started by Eclipse's supervisor";

fn sandbox_without_config(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-settings-bridge-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("create the sandbox directory");
    root
}

fn config_path(root: &Path) -> PathBuf {
    root.join("config").join("eclipse").join("config.json")
}

fn sandbox_with_config(tag: &str, config: &[u8]) -> PathBuf {
    let root = sandbox_without_config(tag);
    let path = config_path(&root);
    std::fs::create_dir_all(path.parent().expect("a config directory"))
        .expect("create the sandbox config directory");
    std::fs::write(path, config).expect("write config.json");
    root
}

fn sandbox(tag: &str) -> PathBuf {
    sandbox_with_config(tag, br#"{"fflags":{"FFlagEclipseBridgeTest":true}}"#)
}

fn staged_settings(app_data: &Path) -> serde_json::Value {
    let staged = std::fs::read(app_data.join("runtime").join("ClientAppSettings.json"))
        .expect("read the staged client settings");
    serde_json::from_slice(&staged).expect("parse the staged client settings")
}

fn missing_apk_command(root: &Path, app_data: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eclipse"));
    command
        .arg("run")
        .arg(root.join(MISSING_APK))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("ECLIPSE_APP_DATA_DIR", app_data)
        .env_remove("LD_PRELOAD")
        .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH");
    command
}

fn run_missing_apk(root: &Path, app_data: &Path, redirect_active: bool) -> Output {
    let mut command = missing_apk_command(root, app_data);
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
        assert!(!stderr.contains(MISSING_APK), "{tag}: {stderr}");
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
    assert!(!stderr.contains(MISSING_APK), "{stderr}");
}

struct TestSupervisor {
    records: PipeReader,
    exit: PipeReader,
    ends: [PipeWriter; 2],
}

impl TestSupervisor {
    fn attach(command: &mut Command) -> Self {
        let (records, records_end) = std::io::pipe().expect("create the record pipe");
        let (exit, exit_end) = std::io::pipe().expect("create the exit pipe");
        let inherited = [records_end.as_raw_fd(), exit_end.as_raw_fd()];
        command.env(
            "ECLIPSE_SUPERVISOR_FDS",
            format!("{},{}", inherited[0], inherited[1]),
        );
        unsafe {
            command.pre_exec(move || {
                for fd in inherited {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Self {
            records,
            exit,
            ends: [records_end, exit_end],
        }
    }

    fn collect(mut self) -> (String, String) {
        drop(self.ends);
        let (mut records, mut exit) = (String::new(), String::new());
        self.records
            .read_to_string(&mut records)
            .expect("read the client's log records");
        self.exit
            .read_to_string(&mut exit)
            .expect("read the client's exit record");
        (records, exit)
    }
}

#[test]
fn a_restarted_run_without_the_preloaded_bridge_stops_and_tells_its_supervisor_why() {
    let root = sandbox("not-preloaded");
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(&runtime).expect("create the runtime directory");
    let mut command = missing_apk_command(&root, &app_data);
    command
        .env("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE", "1")
        .env(
            "ECLIPSE_CLIENT_APP_SETTINGS_PATH",
            runtime.join("ClientAppSettings.json"),
        );
    let supervisor = TestSupervisor::attach(&mut command);
    let output = bounded_child::output(&mut command, RUN_LIMIT);
    let (records, exit) = supervisor.collect();
    std::fs::remove_dir_all(&root).ok();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let unreadable = "/data/local/tmp/ClientAppSettings.json is not readable";
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(BRIDGE_INACTIVE), "{stderr}");
    assert!(stderr.contains(unreadable), "{stderr}");
    assert!(!stderr.contains(MISSING_APK), "{stderr}");
    assert!(!stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert!(
        records
            .lines()
            .any(|line| line.contains(" ERROR eclipse::status: ") && line.contains(unreadable)),
        "{records}"
    );
    assert_eq!(exit, "\"failure_shown\"\n");
}

#[test]
fn a_restarted_run_without_its_supervisor_stops_before_staging_settings() {
    let root = sandbox("unsupervised");
    let app_data = root.join("app-data");
    let settings = app_data.join("runtime").join("ClientAppSettings.json");
    let mut command = missing_apk_command(&root, &app_data);
    command
        .env("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE", "1")
        .env("ECLIPSE_CLIENT_APP_SETTINGS_PATH", &settings)
        .env_remove("ECLIPSE_SUPERVISOR_FDS");
    let output = bounded_child::output(&mut command, RUN_LIMIT);
    let staged = settings.exists();
    std::fs::remove_dir_all(&root).ok();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains(UNSUPERVISED), "{stderr}");
    assert!(!stderr.contains(MISSING_APK), "{stderr}");
    assert!(!staged, "an unsupervised client stages no settings");
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

#[test]
fn a_bad_setting_is_reported_once_and_the_other_settings_still_reach_roblox() {
    let root = sandbox_with_config(
        "bad-setting",
        br#"{"touch_mode": 5, "fflags": {"FFlagEclipseBridgeTest": true}}"#,
    );
    let app_data = root.join("app-data");
    let output = run_missing_apk(&root, &app_data, false);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let staged = staged_settings(&app_data);
    let log = std::fs::read_to_string(app_data.join("logs").join("eclipse.log"));
    std::fs::remove_dir_all(&root).ok();

    let problem = format!("{}:1:16: touch_mode: ", config_path(&root).display());
    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert_eq!(stderr.matches(&problem).count(), 1, "{stderr}");
    assert!(log.expect("read eclipse.log").contains(&problem));
    assert_eq!(staged["FFlagEclipseBridgeTest"], serde_json::json!(true));
}

#[test]
fn an_unreadable_config_stages_only_the_default_flag() {
    let root = sandbox_with_config("syntax-error", br#"{"fflags": {"#);
    let app_data = root.join("app-data");
    let output = run_missing_apk(&root, &app_data, false);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let staged = staged_settings(&app_data);
    std::fs::remove_dir_all(&root).ok();

    let problem = format!("{}:1:12: invalid JSON: ", config_path(&root).display());
    assert_eq!(stderr.matches(&problem).count(), 1, "{stderr}");
    assert_eq!(
        staged,
        serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "True"})
    );
}

#[test]
fn keys_eclipse_does_not_use_are_logged_on_one_line() {
    let root = sandbox_with_config(
        "sober-config",
        br#"{"use_opengl": false, "touch_mode": "fake_off", "use_console_experience": true}"#,
    );
    let app_data = root.join("app-data");
    let output = run_missing_apk(&root, &app_data, false);
    let log = std::fs::read_to_string(app_data.join("logs").join("eclipse.log"));
    std::fs::remove_dir_all(&root).ok();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let log = log.expect("read eclipse.log");
    let naming: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("use_opengl") || line.contains("use_console_experience"))
        .collect();
    let [line] = naming.as_slice() else {
        panic!("expected one line naming the unused keys:\n{log}\n{stderr}");
    };
    assert!(
        line.contains(r#""use_opengl", "use_console_experience""#),
        "{line}"
    );
    assert!(!stderr.contains("use_opengl"), "{stderr}");
}

#[test]
fn a_read_only_config_is_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt as _;

    let bytes = br#"{"touch_mode": "on", "fflags": {"FFlagEclipseBridgeTest": true}}"#;
    let root = sandbox_with_config("read-only-config", bytes);
    let config = config_path(&root);
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o444))
        .expect("make config.json read-only");
    let output = run_missing_apk(&root, &root.join("app-data"), false);
    let after = std::fs::read(&config);
    let mode = std::fs::metadata(&config).map(|metadata| metadata.permissions().mode() & 0o7777);
    std::fs::remove_dir_all(&root).ok();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert_eq!(after.expect("read config.json"), bytes);
    assert_eq!(mode.expect("stat config.json"), 0o444);
}

#[test]
fn a_refused_launch_leaves_the_running_clients_fast_flags_alone() {
    let root = sandbox("refused-launch");
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(&runtime).expect("create the runtime directory");
    let running = br#"{"FFlagEclipseRunningClient":true}"#;
    let settings = runtime.join("ClientAppSettings.json");
    std::fs::write(&settings, running).expect("write the running client's settings");
    let client = std::fs::File::create(runtime.join("client.lock")).expect("create client.lock");
    client.lock().expect("hold client.lock");

    let output = run_missing_apk(&root, &app_data, false);
    let after = std::fs::read(&settings);
    drop(client);
    std::fs::remove_dir_all(&root).ok();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
    assert_eq!(after.expect("read the running client's settings"), running);
}
