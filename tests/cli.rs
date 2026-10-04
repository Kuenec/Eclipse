#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::ffi::OsStr;
use std::io::{Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use sha2::Digest as _;

const RUN_LIMIT: Duration = Duration::from_secs(60);
const SHORT_RUNTIME_PARENT: &str = "/tmp";

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

fn arguments_of(pid: i32) -> Vec<String> {
    let command_line = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    command_line
        .split(|&byte| byte == 0)
        .filter(|argument| !argument.is_empty())
        .skip(1)
        .map(|argument| String::from_utf8_lossy(argument).into_owned())
        .collect()
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
fn an_unknown_run_option_is_named_and_nothing_starts() {
    let root = sandbox("unknown-run-option");
    let app_data = root.join("app-data");

    let output = eclipse(
        &root,
        &app_data,
        &[OsStr::new("run"), OsStr::new("--check-updates")],
    );
    let set_up = app_data.exists();
    std::fs::remove_dir_all(&root).ok();

    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert_eq!(
        stderr,
        "eclipse run: unknown option `--check-updates`; give a file whose name starts with `-` \
         as `./--check-updates`\nusage: eclipse run [--check-update | APK | DIRECTORY]\n"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(!set_up, "nothing is prepared for a launch");
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
fn a_run_logs_the_clients_output_and_how_it_ended() {
    let root = sandbox("supervised-run");
    let app_data = root.join("app-data");
    let missing = root.join("missing.apk");

    let output = eclipse(&root, &app_data, &[OsStr::new("run"), missing.as_os_str()]);
    let logs = app_data.join("logs");
    let latest = std::fs::read_link(logs.join("eclipse.log"));
    let log = std::fs::read_to_string(logs.join("eclipse.log"));
    std::fs::remove_dir_all(&root).ok();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stdout.contains("Roblox Fast Flags staged at"), "{stdout}");
    let latest = latest.expect("eclipse.log links to the run");
    let latest = latest.to_str().unwrap();
    assert!(
        latest.starts_with("eclipse-") && latest.ends_with("Z.log"),
        "{latest}"
    );
    let log = log.expect("read the run log");
    let missing = missing.display().to_string();
    let error_line = stderr
        .lines()
        .find(|line| line.starts_with("eclipse run: ") && line.contains(&missing))
        .unwrap_or_else(|| panic!("{stderr}"));
    let error = error_line.trim_start_matches("eclipse run: ");
    assert!(
        log.contains(&format!("  INFO stderr: {error_line}\n")),
        "{log}"
    );
    assert!(
        log.contains(&format!(" ERROR eclipse::status: {error}\n")),
        "{log}"
    );
    assert!(
        log.ends_with(&format!(
            "  INFO eclipse::supervisor: Roblox stopped after Eclipse reported why\nLast \
             error: {error}\n"
        )),
        "{log}"
    );
}

#[test]
fn a_launch_refused_by_the_running_client_stages_nothing_and_starts_no_log() {
    let root = sandbox("refused-by-lock");
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let client = std::fs::File::create(runtime.join("client.lock")).unwrap();
    client.lock().unwrap();
    let missing = root.join("missing.apk");

    let output = eclipse(&root, &app_data, &[OsStr::new("run"), missing.as_os_str()]);
    let staged = runtime.join("ClientAppSettings.json").exists();
    let shim = runtime.join("libeclipse_client_settings_path.so").exists();
    let logs = app_data.join("logs").exists();
    drop(client);
    std::fs::remove_dir_all(&root).ok();

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(!staged && !shim, "a refused launch stages nothing");
    assert!(!logs, "a refused launch starts no run log");
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
    let assets_extracted = app_data.join("files").exists();
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
    assert!(!assets_extracted, "a refused install prepares nothing");
    let after_exit = stderr(&after_exit);
    assert!(
        !after_exit.contains("Roblox is running in Eclipse"),
        "{after_exit}"
    );
}

#[test]
fn a_terminal_install_prepares_the_client_for_its_next_launch() {
    let Some(paths) =
        eclipse::apk::ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
    else {
        eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
        return;
    };
    let root = sandbox("install-prepares");
    let app_data = root.join("app-data");
    let mut install = vec![OsStr::new("install"), paths.base.as_os_str()];
    install.extend(paths.native_split.iter().map(|split| split.as_os_str()));

    let output = eclipse(&root, &app_data, &install);
    assert!(output.status.success(), "{}", stderr(&output));
    let mut set = eclipse::apk::store::Store::at(root.join("data").join("eclipse").join("roblox"))
        .verified_current()
        .expect("the installed set opens")
        .expect("Roblox is installed");
    let libs = root
        .join("cache")
        .join("eclipse")
        .join("native-libs")
        .join(set.version_code().to_string());
    let status = eclipse::status::StatusSink::terminal();
    let libs_written = set
        .native_libs_mut()
        .extract_native_libs(eclipse::apk::TARGET_ABI, &libs, &status)
        .expect("the installed native libs are checked");
    let assets_written = set
        .base_mut()
        .extract_assets(&app_data.join("files").join("assets"), &status)
        .expect("the installed assets are checked");
    std::fs::remove_dir_all(&root).ok();

    assert_eq!(libs_written, 0, "the install extracted the native libs");
    assert_eq!(assets_written, 0, "the install extracted the assets");
}

#[test]
fn pruning_waits_while_another_process_holds_the_client_lock() {
    let Some(paths) =
        eclipse::apk::ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
    else {
        eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
        return;
    };
    let root = sandbox("prune-waits-for-client");
    let app_data = root.join("app-data");
    let unkept = root.join("data").join("eclipse").join("roblox").join("1");
    let mut install = vec![OsStr::new("install"), paths.base.as_os_str()];
    install.extend(paths.native_split.iter().map(|split| split.as_os_str()));

    let first = eclipse(&root, &app_data, &install);
    std::fs::create_dir_all(&unkept).unwrap();
    let client = std::fs::File::create(app_data.join("runtime").join("client.lock")).unwrap();
    client.lock().unwrap();
    let while_running = eclipse(&root, &app_data, &install);
    let kept_while_running = unkept.exists();
    drop(client);
    let after_exit = eclipse(&root, &app_data, &install);
    let kept_after_exit = unkept.exists();
    std::fs::remove_dir_all(&root).ok();

    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        !while_running.status.success(),
        "{}",
        stderr(&while_running)
    );
    assert!(
        kept_while_running,
        "nothing is removed while Roblox may still open it"
    );
    assert!(after_exit.status.success(), "{}", stderr(&after_exit));
    assert!(
        !kept_after_exit,
        "the install after Roblox closed removes what is not kept"
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

#[test]
fn a_launch_while_roblox_runs_is_handed_to_it_with_no_link_in_its_arguments() {
    let root = sandbox("hand-off");
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let running = std::fs::File::create(runtime.join("client.lock")).unwrap();
    running.lock().unwrap();
    let settings = runtime.join("ClientAppSettings.json");
    std::fs::write(&settings, b"{\"FFlagEclipseRunning\":true}\n").unwrap();
    let staged = std::fs::metadata(&settings).unwrap().modified().unwrap();

    let runtime_dir =
        Path::new(SHORT_RUNTIME_PARENT).join(format!("ec-{}-xdg", std::process::id()));
    std::fs::remove_dir_all(&runtime_dir).ok();
    let socket_dir = runtime_dir.join("eclipse");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&socket_dir)
        .unwrap();
    let lock_root = app_data.canonicalize().unwrap();
    let digest = sha2::Sha256::digest(lock_root.as_os_str().as_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let listener = UnixListener::bind(socket_dir.join(format!("control-{hash}.sock"))).unwrap();
    let running_client = std::thread::spawn(move || {
        (0..3)
            .map(|_| {
                let (mut stream, _) = listener.accept().unwrap();
                let launcher = rustix::net::sockopt::socket_peercred(&stream).unwrap().pid;
                let arguments = arguments_of(launcher.as_raw_nonzero().get());
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                stream.write_all(br#""accepted""#).unwrap();
                (arguments, request)
            })
            .collect::<Vec<_>>()
    });

    let access_code = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
    let click = format!(
        "roblox-player:1+launchmode:play+gameinfo:SECRET_TICKET+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestPrivateGame%26placeId%3D1818%26accessCode%3D{access_code}%26linkCode%3D98765"
    );
    let launches = [
        &["run"][..],
        &["open", "1818"],
        &["__handle-roblox-player-url", &click],
    ]
    .map(|args| {
        bounded_child::output(
            Command::new(env!("CARGO_BIN_EXE_eclipse"))
                .args(args)
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("ECLIPSE_APP_DATA_DIR", &app_data)
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("WAYLAND_DISPLAY", "eclipse-test-no-display")
                .env_remove("DISPLAY")
                .env_remove("LD_PRELOAD")
                .env_remove("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE")
                .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH"),
            RUN_LIMIT,
        )
    });
    let requests = running_client.join().unwrap();
    let settings_after = std::fs::read(&settings).unwrap();
    let staged_after = std::fs::metadata(&settings).unwrap().modified().unwrap();
    let shim_staged = runtime.join("libeclipse_client_settings_path.so").exists();
    drop(running);
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&runtime_dir).ok();

    for output in &launches {
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}{}", stderr(output));
        assert!(!stdout.contains("# Booting the ART VM"), "{stdout}");
    }
    let (arguments, requests): (Vec<_>, Vec<_>) = requests.into_iter().unzip();
    assert_eq!(arguments, [["run"], ["__launch-link"], ["__launch-link"]]);
    assert_eq!(
        requests,
        [
            b"\x01{\"show\":{}}".to_vec(),
            b"\x01{\"open\":{\"link\":\"roblox://placeId=1818\"}}".to_vec(),
            format!(
                "\x01{{\"open\":{{\"link\":\"roblox://placeId=1818&accessCode={access_code}&linkCode=98765\"}}}}"
            )
            .into_bytes(),
        ]
    );
    assert_eq!(settings_after, b"{\"FFlagEclipseRunning\":true}\n");
    assert_eq!(staged_after, staged);
    assert!(!shim_staged, "a handed-off launch never restarts itself");
}

const FAILURE_WINDOW_STUB: &str = "#!/bin/sh\n\
    dir=${0%/*}\n\
    {\n\
    printf '%s\\n' \"$@\"\n\
    printf 'GSK_RENDERER=%s\\n' \"${GSK_RENDERER-unset}\"\n\
    printf 'XDG_ACTIVATION_TOKEN=%s\\n' \"${XDG_ACTIVATION_TOKEN-unset}\"\n\
    printf 'DESKTOP_STARTUP_ID=%s\\n' \"${DESKTOP_STARTUP_ID-unset}\"\n\
    } > \"$dir/window.tmp\"\n\
    mv \"$dir/window.tmp\" \"$dir/window\"\n\
    while [ ! -e \"$dir/close\" ]; do sleep 0.05; done\n";

struct Launched {
    eclipse: std::process::Child,
    close: PathBuf,
}

impl Drop for Launched {
    fn drop(&mut self) {
        std::fs::write(&self.close, b"").ok();
        if let Ok(None) = self.eclipse.try_wait() {
            self.eclipse.kill().ok();
            self.eclipse.wait().ok();
        }
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + RUN_LIMIT;
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_failed_window_launch_frees_roblox_and_opens_the_failure_window_on_its_saved_report() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = sandbox("failure-window");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let eclipse = bin.join("eclipse");
    if std::fs::hard_link(env!("CARGO_BIN_EXE_eclipse"), &eclipse).is_err() {
        std::fs::copy(env!("CARGO_BIN_EXE_eclipse"), &eclipse).unwrap();
    }
    let window = bin.join("eclipse-settings");
    std::fs::write(&window, FAILURE_WINDOW_STUB).unwrap();
    std::fs::set_permissions(&window, std::fs::Permissions::from_mode(0o755)).unwrap();
    let app_data = root.join("app-data");
    let runtime = app_data.join("runtime");
    std::fs::create_dir_all(runtime.join("ClientAppSettings.json")).unwrap();
    let runtime_dir = Path::new(SHORT_RUNTIME_PARENT).join(format!("ec-{}-fw", std::process::id()));
    std::fs::remove_dir_all(&runtime_dir).ok();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&runtime_dir)
        .unwrap();

    let eclipse = Command::new(&eclipse)
        .arg("run")
        .env("HOME", &root)
        .env("USER", "eclipse-tester")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("ECLIPSE_APP_DATA_DIR", &app_data)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_ACTIVATION_TOKEN", "eclipse-test-token")
        .env_remove("DESKTOP_STARTUP_ID")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("DISPLAY")
        .env_remove("LD_PRELOAD")
        .env_remove("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE")
        .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH")
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(root.join("stdout")).unwrap())
        .stderr(std::fs::File::create(root.join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let mut launch = Launched {
        eclipse,
        close: bin.join("close"),
    };
    wait_for("the failure window opens", || bin.join("window").exists());
    let lock = std::fs::File::create(runtime.join("client.lock")).unwrap();
    let relaunch_possible = lock.try_lock().is_ok();
    drop(lock);
    let reports: Vec<PathBuf> = std::fs::read_dir(app_data.join("logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".report.txt"))
        .collect();
    let report = std::fs::read_to_string(&reports[0]).unwrap();
    std::fs::write(&launch.close, b"").unwrap();
    let mut exited = None;
    wait_for("Eclipse exits after the window closes", || {
        exited = launch.eclipse.try_wait().unwrap();
        exited.is_some()
    });
    drop(launch);
    let opened = std::fs::read_to_string(bin.join("window")).unwrap();
    let stderr = std::fs::read_to_string(root.join("stderr")).unwrap();
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&runtime_dir).ok();

    assert_eq!(exited.unwrap().code(), Some(1), "{stderr}");
    assert!(
        relaunch_possible,
        "Roblox's lock is free while the window is open"
    );
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(
        opened,
        format!(
            "--start-failure-report={}\nGSK_RENDERER=cairo\nXDG_ACTIVATION_TOKEN=unset\n\
             DESKTOP_STARTUP_ID=unset\n",
            reports[0].display()
        )
    );
    assert!(
        report.starts_with("Outcome: cannot write ~/app-data/runtime/ClientAppSettings.json"),
        "{report}"
    );
    let (summary, rest) = report.split_once("\n\n").unwrap();
    assert!(
        summary.contains("\nProblem: Roblox is not installed. "),
        "{summary}"
    );
    assert!(
        summary
            .lines()
            .last()
            .is_some_and(|line| line.starts_with("Log: ~/app-data/logs/eclipse-")),
        "{summary}"
    );
    assert!(rest.starts_with("```text\nEclipse\n"), "{rest}");
    assert!(report.len() <= 60_000, "{} bytes", report.len());
    assert_eq!(
        stderr.matches("eclipse run: cannot write ").count(),
        1,
        "{stderr}"
    );
    assert!(!stderr.contains("cannot open a window"), "{stderr}");
}

#[test]
fn the_log_directory_is_printed_for_the_settings_window() {
    let root = sandbox("log-dir");
    let app_data = root.join("app-data").join(OsStr::from_bytes(b"odd \xe9"));

    let output = eclipse(&root, &app_data, &[OsStr::new("__log-dir")]);
    let created = app_data.exists();
    std::fs::remove_dir_all(&root).ok();

    assert!(output.status.success(), "{output:?}");
    let mut expected = app_data.join("logs").as_os_str().as_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(output.stdout, expected);
    assert!(!created, "printing the directory creates nothing");
}

#[test]
fn controller_access_is_one_line_and_fails_when_controllers_cannot_work() {
    let root = sandbox("controller-access");

    let output = eclipse(
        &root,
        &root.join("app-data"),
        &[OsStr::new("__controller-access")],
    );
    std::fs::remove_dir_all(&root).ok();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert_eq!(
        output.status.success(),
        stdout == "input devices are visible\n",
        "{stdout}"
    );
}
