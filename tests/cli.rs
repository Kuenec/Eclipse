#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const RUN_LIMIT: Duration = Duration::from_secs(60);

fn sandbox(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-cli-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("create the sandbox directory");
    root
}

fn eclipse(root: &Path, app_data: &Path, args: &[&OsStr]) -> Output {
    bounded_child::output(
        Command::new(env!("CARGO_BIN_EXE_eclipse"))
            .args(args)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("ECLIPSE_APP_DATA_DIR", app_data)
            .env_remove("LD_PRELOAD")
            .env_remove("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE")
            .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH"),
        RUN_LIMIT,
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_malformed_config_is_reported_with_its_position_and_the_launch_continues() {
    let root = sandbox("malformed-config");
    let config = root.join("config").join("eclipse").join("config.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, br#"{"use_opengl": true,}"#).unwrap();
    let missing = root.join("missing.apk");

    let output = eclipse(
        &root,
        &root.join("app-data"),
        &[OsStr::new("run"), missing.as_os_str()],
    );
    std::fs::remove_dir_all(&root).ok();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = stderr(&output);
    let position = format!("{}:1:21: ", config.display());
    assert!(stderr.contains(&position), "{stderr}");
    assert!(
        stderr.contains("Eclipse uses the default for every setting"),
        "{stderr}"
    );
    assert!(!stderr.contains("Android settings setup"), "{stderr}");
    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");
    assert!(!output.status.success(), "{stderr}");
}

#[test]
fn an_app_data_directory_that_cannot_be_created_is_named() {
    let root = sandbox("blocked-app-data");
    std::fs::write(root.join("blocker"), b"a file where a directory belongs").unwrap();
    let app_data = root.join("blocker").join("app-data");
    let missing = root.join("missing.apk");

    let output = eclipse(&root, &app_data, &[OsStr::new("run"), missing.as_os_str()]);
    std::fs::remove_dir_all(&root).ok();

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{stderr}");
    let expected = format!("cannot create {}", app_data.join("runtime").display());
    assert!(stderr.contains(&expected), "{stderr}");
}

#[test]
fn installs_and_updates_are_refused_while_roblox_runs() {
    let root = sandbox("client-running");
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let client = std::fs::File::create(runtime.join("client.lock")).unwrap();
    client.lock().unwrap();
    let missing = root.join("missing.apk");
    let store = root.join("data").join("eclipse").join("roblox");

    let install = eclipse(
        &root,
        &app_data,
        &[OsStr::new("install"), missing.as_os_str()],
    );
    let update = eclipse(
        &root,
        &app_data,
        &[OsStr::new("update"), OsStr::new("--play")],
    );
    let store_touched = store.exists();
    drop(client);
    let after_exit = eclipse(
        &root,
        &app_data,
        &[OsStr::new("install"), missing.as_os_str()],
    );
    std::fs::remove_dir_all(&root).ok();

    for output in [&install, &update] {
        let stderr = stderr(output);
        assert!(!output.status.success(), "{stderr}");
        assert!(stderr.contains("Roblox is running in Eclipse"), "{stderr}");
    }
    assert!(!store_touched, "a refused install leaves the store alone");
    let after_exit = stderr(&after_exit);
    assert!(
        !after_exit.contains("Roblox is running in Eclipse"),
        "{after_exit}"
    );
}

#[test]
fn arguments_that_are_not_utf8_reach_the_command() {
    let root = sandbox("non-utf8-argument");
    let latin1 = root.join(OsStr::from_bytes(b"R\xf6blox.xapk"));

    let outputs = ["install", "run"].map(|command| {
        eclipse(
            &root,
            &root.join("app-data"),
            &[OsStr::new(command), latin1.as_os_str()],
        )
    });
    std::fs::remove_dir_all(&root).ok();

    for output in outputs {
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}
