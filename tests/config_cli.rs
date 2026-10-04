#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::fs::{self, Permissions};
use std::os::unix::fs::{symlink, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const RUN_LIMIT: Duration = Duration::from_secs(60);

const FIXTURE: &str = r#"{
  "zeta_unknown": 1,
  "touch_mode": "fake-off",
  "allow_gamepad_permission": false,
  "alpha_unknown": {"b": 2, "a": 1},
  "fflags": {"FFlagGameBasicSettingsFramerateCap5": "True"}
}
"#;

struct Sandbox(PathBuf);

impl Sandbox {
    fn create(tag: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("config-cli-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("config").join("eclipse"))
            .expect("create the config directory");
        Self(root)
    }

    fn with_config(tag: &str, text: &str) -> Self {
        let sandbox = Self::create(tag);
        fs::write(sandbox.config(), text).expect("write config.json");
        sandbox
    }

    fn config(&self) -> PathBuf {
        self.0.join("config").join("eclipse").join("config.json")
    }

    fn eclipse_config(&self, arguments: &[&str]) -> Output {
        self.eclipse(
            Path::new(env!("CARGO_BIN_EXE_eclipse")),
            &[&["config"], arguments].concat(),
        )
    }

    fn eclipse(&self, executable: &Path, arguments: &[&str]) -> Output {
        bounded_child::output(
            Command::new(executable)
                .args(arguments)
                .env("XDG_CONFIG_HOME", self.0.join("config"))
                .env("XDG_DATA_HOME", self.0.join("data"))
                .env("XDG_CACHE_HOME", self.0.join("cache"))
                .env("ECLIPSE_APP_DATA_DIR", self.0.join("app-data"))
                .env_remove("FLATPAK_ID")
                .env_remove("RUST_LOG")
                .env_remove("LD_PRELOAD")
                .env_remove("ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE")
                .env_remove("ECLIPSE_CLIENT_APP_SETTINGS_PATH"),
            RUN_LIMIT,
        )
    }

    fn config_text(&self) -> String {
        fs::read_to_string(self.config()).expect("read config.json")
    }

    fn install_eclipse(&self) -> PathBuf {
        let lib = self.0.join("lib");
        let bin = self.0.join("bin");
        fs::create_dir_all(&lib).expect("create the install's lib directory");
        fs::create_dir_all(&bin).expect("create the install's bin directory");
        fs::hard_link(env!("CARGO_BIN_EXE_eclipse"), lib.join("eclipse"))
            .expect("link the eclipse binary into the install");
        symlink("../lib/eclipse", bin.join("eclipse")).expect("link bin/eclipse to lib/eclipse");
        bin.join("eclipse")
    }

    fn settings_app(&self) -> PathBuf {
        self.0.join("lib").join("eclipse-settings")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let removed = fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.expect("remove the sandbox");
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn an_invalid_config_fails_validation_at_its_position_and_shows_what_applies() {
    let sandbox = Sandbox::with_config(
        "invalid",
        r#"{"touch_mode": 5, "graphics_optimization_mode": "performance"}"#,
    );
    let config = sandbox.config();

    let output = sandbox.eclipse_config(&[]);

    let stdout = text(&output.stdout);
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    let position = format!("{}:1:16: touch_mode: ", config.display());
    assert!(stderr.starts_with(&position), "{stderr}");
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let shown = stdout
        .strip_prefix(&format!("# {}\n", config.display()))
        .unwrap_or_else(|| panic!("{stdout}"));
    let shown: serde_json::Value = serde_json::from_str(shown).expect("the settings are JSON");
    assert_eq!(shown["touch_mode"], "off");
    assert_eq!(shown["graphics_optimization_mode"], "performance");
}

#[test]
fn keys_eclipse_does_not_use_are_named_once_and_pass_validation() {
    let sandbox = Sandbox::with_config(
        "unused",
        r#"{"use_console_experience": true, "touch_mode": "fake_off", "enable_mobile_home_screen": false}"#,
    );

    let output = sandbox.eclipse_config(&[]);

    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "{}: Eclipse does not use these keys: \"use_console_experience\", \
             \"enable_mobile_home_screen\"\n",
            sandbox.config().display()
        )
    );
}

#[test]
fn set_changes_only_that_line_and_setting_it_again_says_so() {
    let sandbox = Sandbox::with_config("set", FIXTURE);
    let config = sandbox.config();

    let first = sandbox.eclipse_config(&["set", "touch_mode", "on"]);
    let written = sandbox.config_text();
    let again = sandbox.eclipse_config(&["set", "touch_mode", "on"]);

    assert_eq!(first.status.code(), Some(0), "{}", text(&first.stderr));
    assert_eq!(
        text(&first.stdout),
        format!("touch_mode is now \"on\" in {}\n", config.display())
    );
    assert_eq!(
        written,
        FIXTURE.replace(r#""touch_mode": "fake-off""#, r#""touch_mode": "on""#)
    );
    assert_eq!(again.status.code(), Some(0), "{}", text(&again.stderr));
    assert_eq!(
        text(&again.stdout),
        format!("touch_mode is already \"on\" in {}\n", config.display())
    );
    assert_eq!(sandbox.config_text(), written);
}

#[test]
fn set_creates_a_missing_config_and_reads_the_value_as_json() {
    let sandbox = Sandbox::create("create");

    let output = sandbox.eclipse_config(&["set", "enable_gamemode", "false"]);

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        sandbox.config_text(),
        "{\n  \"enable_gamemode\": false\n}\n"
    );
}

#[test]
fn unset_removes_the_key_and_names_the_default() {
    let sandbox = Sandbox::with_config("unset", FIXTURE);

    let output = sandbox.eclipse_config(&["unset", "touch_mode"]);

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "touch_mode is removed from {}; Eclipse uses the default (\"off\")\n",
            sandbox.config().display()
        )
    );
    assert_eq!(
        sandbox.config_text(),
        FIXTURE.replace("  \"touch_mode\": \"fake-off\",\n", "")
    );
}

#[test]
fn a_bad_value_a_hand_edited_key_or_an_unknown_key_is_refused() {
    let sandbox = Sandbox::with_config("refused", FIXTURE);
    let config = sandbox.config().display().to_string();
    let cases = [
        (
            &["set", "touch_mode", "sideways"][..],
            "touch_mode: expected one of `off`, `on`, `fake-off`".to_owned(),
        ),
        (
            &["set", "fflags", "{}"],
            format!("`fflags` is edited by hand in {config}"),
        ),
        (
            &["unset", "webview_helper_path"],
            format!("`webview_helper_path` is edited by hand in {config}"),
        ),
        (
            &["set", "use_console_experience", "true"],
            "`use_console_experience` is not a setting; the settings are `touch_mode`, ".to_owned(),
        ),
        (
            &["set", "touch_mode"],
            "usage: eclipse config [set KEY VALUE | unset KEY]".to_owned(),
        ),
    ];

    for (arguments, reason) in cases {
        let output = sandbox.eclipse_config(arguments);

        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(
            stderr.starts_with(&format!("eclipse config: {reason}")),
            "{arguments:?}: {stderr}"
        );
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert_eq!(sandbox.config_text(), FIXTURE, "{arguments:?}");
    }
}

#[test]
fn a_config_managed_outside_eclipse_is_refused_and_left_alone() {
    let sandbox = Sandbox::create("managed");
    let config = sandbox.config();
    let target = sandbox.0.join("dotfiles.json");
    fs::write(&target, FIXTURE).expect("write the link target");
    symlink(&target, &config).expect("link config.json");

    let linked = sandbox.eclipse_config(&["set", "touch_mode", "on"]);
    let link_after = fs::read_link(&config).expect("config.json is still a link");
    let target_after = fs::read_to_string(&target).expect("read the link target");

    fs::remove_file(&config).expect("remove the link");
    fs::write(&config, FIXTURE).expect("write config.json");
    fs::set_permissions(&config, Permissions::from_mode(0o444)).expect("make it read-only");
    let read_only = sandbox.eclipse_config(&["unset", "touch_mode"]);
    let mode = fs::metadata(&config)
        .expect("stat config.json")
        .permissions()
        .mode()
        & 0o7777;

    for output in [&linked, &read_only] {
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("managed outside Eclipse"), "{stderr}");
    }
    assert_eq!(link_after, target);
    assert_eq!(target_after, FIXTURE);
    assert_eq!((sandbox.config_text(), mode), (FIXTURE.to_owned(), 0o444));
}

#[test]
fn settings_becomes_the_settings_app_installed_next_to_eclipse() {
    let sandbox = Sandbox::create("settings");
    let eclipse = sandbox.install_eclipse();
    let settings = sandbox.settings_app();
    fs::write(
        &settings,
        "#!/bin/sh\necho \"$PPID $#\" > \"${0%/*}/opened\"\n",
    )
    .expect("write the settings stub");
    fs::set_permissions(&settings, Permissions::from_mode(0o755)).expect("make it executable");

    let output = sandbox.eclipse(&eclipse, &["settings"]);

    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(
        fs::read_to_string(sandbox.0.join("lib").join("opened")).expect("the stub ran"),
        format!("{} 0\n", std::process::id())
    );
}

#[test]
fn settings_names_where_the_missing_settings_app_belongs() {
    let sandbox = Sandbox::create("no-settings");
    let eclipse = sandbox.install_eclipse();

    let output = sandbox.eclipse(&eclipse, &["settings"]);

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        format!(
            "eclipse settings: the settings app is not installed at {}; it ships with the \
             Flatpak\n",
            sandbox.settings_app().display()
        )
    );
}

#[test]
fn settings_takes_no_arguments() {
    let sandbox = Sandbox::create("settings-usage");

    let output = sandbox.eclipse(
        Path::new(env!("CARGO_BIN_EXE_eclipse")),
        &["settings", "--help"],
    );

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        text(&output.stderr),
        "eclipse settings: usage: eclipse settings\n"
    );
}
