use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk4::{gio, glib};

const CHILD_ROOT: &str = "ECLIPSE_SETTINGS_TEST_ROOT";

const TEST_APP_ID: &str = "io.github.kuenec.Eclipse.Settings.Test";

const DISPLAY: &str = ":1";

const STARTUP: Duration = Duration::from_secs(10);

const WAIT: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(10);

pub(crate) fn child_root() -> Option<PathBuf> {
    std::env::var_os(CHILD_ROOT).map(PathBuf::from)
}

pub(crate) fn root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-settings-{tag}"));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(config_path(&root).parent().expect("a config directory"))
        .expect("create the test root");
    root
}

pub(crate) fn config_path(root: &Path) -> PathBuf {
    root.join("config").join("eclipse").join("config.json")
}

pub(crate) fn eclipse_path(root: &Path) -> PathBuf {
    root.join("bin").join("eclipse")
}

struct Broadway {
    daemon: Child,
    runtime: PathBuf,
}

impl Broadway {
    fn start(tag: &str) -> Self {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .expect("XDG_RUNTIME_DIR must name a directory for the Broadway display socket");
        let runtime = base.join(format!("eclipse-settings-{}-{tag}", std::process::id()));
        fs::remove_dir_all(&runtime).ok();
        fs::create_dir_all(&runtime).expect("create the Broadway runtime directory");
        fs::set_permissions(&runtime, Permissions::from_mode(0o700))
            .expect("make the Broadway runtime directory private");
        let daemon = Command::new("gtk4-broadwayd")
            .arg(DISPLAY)
            .arg("--unixsocket")
            .arg(runtime.join("http.socket"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start gtk4-broadwayd, GTK 4's Broadway display server");
        let broadway = Self { daemon, runtime };
        let deadline = Instant::now() + STARTUP;
        while !broadway.listening() {
            assert!(
                Instant::now() < deadline,
                "gtk4-broadwayd did not open its display socket in {STARTUP:?}"
            );
            std::thread::sleep(POLL);
        }
        broadway
    }

    fn listening(&self) -> bool {
        fs::read_dir(&self.runtime)
            .expect("list the Broadway runtime directory")
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("broadway") && name.ends_with(".socket")
            })
    }
}

impl Drop for Broadway {
    fn drop(&mut self) {
        self.daemon.kill().ok();
        self.daemon.wait().ok();
        fs::remove_dir_all(&self.runtime).ok();
    }
}

pub(crate) fn run_child(test: &str, tag: &str, root: &Path) -> Output {
    let broadway = Broadway::start(tag);
    let output = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args(["--exact", test, "--test-threads=1"])
        .env(CHILD_ROOT, root)
        .env("GDK_BACKEND", "broadway")
        .env("BROADWAY_DISPLAY", DISPLAY)
        .env("XDG_RUNTIME_DIR", &broadway.runtime)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("GSETTINGS_BACKEND", "memory")
        .env("GTK_A11Y", "none")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .output()
        .expect("run the test binary again as a GTK child");
    drop(broadway);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the GTK child failed: {:?}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stderr.contains("-CRITICAL"),
        "the GTK child logged a critical warning:\n{stderr}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed"),
        "the GTK child did not run {test}"
    );
    output
}

pub(crate) fn app() -> adw::Application {
    let app = adw::Application::builder()
        .application_id(TEST_APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(gio::Cancellable::NONE)
        .expect("register the test application");
    app
}

pub(crate) fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        if !context.iteration(false) {
            std::thread::sleep(POLL);
        }
    }
}

pub(crate) fn settle() {
    let context = glib::MainContext::default();
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        if !context.iteration(false) {
            std::thread::sleep(POLL);
        }
    }
}
