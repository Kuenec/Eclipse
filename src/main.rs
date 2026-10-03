use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use eclipse::apk::store::{
    CheckOutcome, Committed, InstalledVersion, Release, Store, UpdateCheck, UpdateOutcome,
};
use eclipse::apk::{ApkSet, ApkSetPaths, VersionCode};
use eclipse::framework::ActivityStart;
use eclipse::graphics::launch_window::{LaunchWindow, WindowClosed};
use eclipse::links::LaunchTarget;
use eclipse::runtime::{ClientCacheDir, NativeLibRoot};
use eclipse::status::{StatusSink, StatusUpdate};
use eclipse::storage::Trim;

mod desktop_integration;
mod instance_control;
mod supervisor;

use instance_control::{ClientLock, HandOff, LaunchSlot, Request};
use supervisor::{ClientEnd, ExitRecord, Supervision};

const CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV: &str = "ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE";
const CLIENT_SETTINGS_PATH_ENV: &str = "ECLIPSE_CLIENT_APP_SETTINGS_PATH";
const ANDROID_CLIENT_SETTINGS_PATH: &str = "/data/local/tmp/ClientAppSettings.json";
const CLIENT_SETTINGS_PATH_SHIM_NAME: &str = "libeclipse_client_settings_path.so";
const CLIENT_SETTINGS_PATH_SHIM: &[u8] =
    include_bytes!(env!("ECLIPSE_CLIENT_SETTINGS_PATH_SHIM_SO"));
const MAXIMUM_FRAME_RATE_ROW_FLAG: &str = "FFlagGameBasicSettingsFramerateCap5";

const RUN_COMMAND: &str = "run";
const OPEN_COMMAND: &str = "open";
const LAUNCH_LINK_COMMAND: &str = "__launch-link";
const LAUNCH_LINK_ENV: &str = "ECLIPSE_LAUNCH_LINK";
const RUN_USAGE: &str = "usage: eclipse run [APK | DIRECTORY]";
const OPEN_USAGE: &str = "usage: eclipse open <LINK | PLACE ID>";
const OPEN_CONTEXT: &str = "eclipse open";
const HAND_OFF_CONTEXT: &str = "eclipse launch";
const SETTINGS_CONTEXT: &str = "eclipse Android settings setup";
const FRAME_LOG_CONTEXT: &str = "eclipse frame-time log";
const UNSUPERVISED: &str = "the Android client must be started by Eclipse's supervisor; start \
     Eclipse without ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE in its environment";
const BROWSER_LAUNCH_CONTEXT: &str = "eclipse browser launch";

const HELP: &str = "\
eclipse — run the Android Roblox build on Linux (open-source, Rust)

USAGE:
    eclipse <COMMAND>

COMMANDS:
    run [PATH]  Verify the Roblox client, boot the ART VM (Roblox on the classpath) and open
                the window. With no PATH, runs the installed client in a window that shows the
                download and any error: the first run downloads Roblox, and later runs check
                for a Roblox update at most every 6 hours, or 30 minutes after a failed check.
                PATH may be an APK file or a directory holding base.apk and
                split_config.x86_64.apk. Only one Roblox client runs at a time; running it
                again asks its window to come to the front.
    open <LINK>
                Start Roblox as `run` does and open a Roblox link or place ID: a game, server,
                private-server, friend or share link. While Roblox is starting, the link goes
                to it instead.
    install <PATH>...
                Verify and install the Roblox client: base.apk plus split_config.x86_64.apk,
                a directory holding them, or an .apks/.xapk/.apkm bundle.
    update [--play]
                Download and install the newest Roblox client from APKCombo, without any
                account. With --play, download it from Google Play with the account saved by
                play-login instead.
    play-login  Sign in to Google Play with your own Google account (once, for update --play).
    install-url-handler
                Register Eclipse for browser Play clicks (they open the link as `open` does).
    config      Show effective configuration and its path
    help        Show this help
    --version   Show version

NOTE: Eclipse runs only the official, unmodified Roblox client signed by Roblox Corporation.
    It never hosts or modifies it. `eclipse update` downloads Roblox's own release files from
    APKCombo (or Google Play with --play) and installs them only when Roblox's signature
    verifies; anything else is discarded.
";

fn main() -> ExitCode {
    let supervision = match supervisor::adopt() {
        Ok(supervision) => supervision,
        Err(error) => {
            eprintln!("eclipse: {error}");
            return ExitCode::FAILURE;
        }
    };
    let launch_link = take_launch_link();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let launch = match LaunchCommand::parse(&args, launch_link) {
        Ok(launch) => launch,
        Err(error) => {
            error.report();
            return ExitCode::FAILURE;
        }
    };
    if let Some(launch) = launch {
        if std::env::var_os(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV).is_some() {
            return run_client(launch, supervision);
        }
        if matches!(launch, LaunchCommand::Link(_)) && args[0] != LAUNCH_LINK_COMMAND {
            return restart_without_link_arguments(&launch);
        }
        return start_client(&launch);
    }
    let command = args.first().map(|command| command.to_string_lossy());
    let command = command.as_deref();
    if matches!(command, Some("__webview-test") | Some("__platform-test")) {
        if let Err(error) = eclipse::runtime::prepare_art_boot_environment() {
            eprintln!("eclipse ART startup: {error}");
            return ExitCode::FAILURE;
        }
    }

    eclipse::diagnostics::init(eclipse::diagnostics::LogSink::Stderr);

    tracing::debug!(version = eclipse::VERSION, command, "eclipse starting");
    match command {
        Some("--version") | Some("-V") => {
            println!("eclipse {}", eclipse::VERSION);
            ExitCode::SUCCESS
        }
        Some("install-url-handler") => match install_url_handler_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("eclipse install-url-handler: {error}");
                ExitCode::FAILURE
            }
        },
        Some("install") => match install_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("eclipse install: {error}");
                ExitCode::FAILURE
            }
        },
        Some("play-login") => match play_login_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("eclipse play-login: {error}");
                ExitCode::FAILURE
            }
        },
        Some("update") => match update_command(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("eclipse update: {error}");
                ExitCode::FAILURE
            }
        },
        Some("config") => show_config(),

        Some("__run-libroblox-init") => {
            let outcome = parse_libroblox_init_lib_dir(&args[1..]).and_then(|lib_dir| {
                eclipse::loader::init_run::run_libroblox_init(lib_dir).map_err(|e| e.to_string())
            });
            match outcome {
                Ok(completed) => {
                    println!("__run-libroblox-init: {completed} constructor(s) completed");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("__run-libroblox-init: {e}");
                    ExitCode::FAILURE
                }
            }
        }

        Some("__gl-test") => match eclipse::egl_engine::run_gl_test() {
            Ok(report) => {
                println!("__gl-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__gl-test: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__gl-test-anw") => match eclipse::egl_engine::run_gl_test_anw() {
            Ok(report) => {
                println!("__gl-test-anw: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__gl-test-anw: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__input-test") => match eclipse::loader::native_provider::run_input_test() {
            Ok(report) => {
                println!("__input-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__input-test: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__webview-test") => match run_webview_test() {
            Ok(report) => {
                println!("__webview-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__webview-test: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__platform-test") => match run_platform_test() {
            Ok(report) => {
                for line in report.to_string().lines() {
                    println!("__platform-test: {line}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__platform-test: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__audio-test") => match eclipse::loader::opensl::run_audio_test() {
            Ok(report) => {
                println!("__audio-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__audio-test: {e}");
                ExitCode::FAILURE
            }
        },
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(_) => {
            eprintln!("unknown command\n\n{HELP}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LaunchCommand {
    Run,
    RunFile(PathBuf),
    Link(LaunchTarget),
}

impl LaunchCommand {
    fn parse(
        arguments: &[OsString],
        launch_link: Option<OsString>,
    ) -> Result<Option<Self>, LaunchCommandError> {
        let Some((command, rest)) = arguments.split_first() else {
            return Ok(None);
        };
        let launch = match command.to_str() {
            Some(RUN_COMMAND) => match rest {
                [] => Self::Run,
                [path] => Self::RunFile(PathBuf::from(path)),
                _ => return Err(LaunchCommandError::Usage(RUN_USAGE)),
            },
            Some(OPEN_COMMAND) => match rest {
                [link] => Self::Link(link_target(OPEN_CONTEXT, link)?),
                _ => return Err(LaunchCommandError::Usage(OPEN_USAGE)),
            },
            Some(desktop_integration::BROWSER_HANDLER_COMMAND) => match rest {
                [link] => Self::Link(link_target(BROWSER_LAUNCH_CONTEXT, link)?),
                _ => {
                    return Err(LaunchCommandError::Link {
                        context: BROWSER_LAUNCH_CONTEXT,
                        message: "The Roblox link handler takes exactly one link.".to_owned(),
                    })
                }
            },
            Some(LAUNCH_LINK_COMMAND) => match (rest, launch_link) {
                ([], Some(link)) => Self::Link(link_target(OPEN_CONTEXT, &link)?),
                _ => {
                    return Err(LaunchCommandError::Link {
                        context: OPEN_CONTEXT,
                        message: format!(
                            "{LAUNCH_LINK_COMMAND} is internal and takes its link from \
                             {LAUNCH_LINK_ENV}; open links with `eclipse open <LINK>`"
                        ),
                    })
                }
            },
            _ => return Ok(None),
        };
        Ok(Some(launch))
    }

    fn launches_in_window(&self) -> bool {
        !matches!(self, Self::RunFile(_))
    }

    fn launch(&self) -> Launch {
        match self {
            Self::Run => Launch::Installed,
            Self::RunFile(_) => Launch::File,
            Self::Link(target) => Launch::Link(target.clone()),
        }
    }

    fn restart(&self, command: &mut std::process::Command) {
        match self {
            Self::Run => {
                command.arg(RUN_COMMAND);
            }
            Self::RunFile(path) => {
                command.arg(RUN_COMMAND).arg(path);
            }
            Self::Link(target) => {
                command
                    .arg(LAUNCH_LINK_COMMAND)
                    .env(LAUNCH_LINK_ENV, target.android_uri());
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LaunchCommandError {
    Usage(&'static str),
    Link {
        context: &'static str,
        message: String,
    },
}

impl LaunchCommandError {
    fn report(&self) {
        match self {
            Self::Usage(usage) => eprintln!("{usage}"),
            Self::Link { context, message } => {
                eprintln!("{context}: {message}");
                show_error_window(message, None);
            }
        }
    }
}

fn link_target(
    context: &'static str,
    link: &std::ffi::OsStr,
) -> Result<LaunchTarget, LaunchCommandError> {
    let invalid = |message: String| LaunchCommandError::Link { context, message };
    let link = link
        .to_str()
        .ok_or_else(|| invalid("The Roblox link is not valid UTF-8.".to_owned()))?;
    eclipse::links::parse(link).map_err(|error| invalid(error.to_string()))
}

fn take_launch_link() -> Option<OsString> {
    let link = std::env::var_os(LAUNCH_LINK_ENV)?;
    unsafe { std::env::remove_var(LAUNCH_LINK_ENV) };
    Some(link)
}

fn restart_without_link_arguments(launch: &LaunchCommand) -> ExitCode {
    use std::os::unix::process::CommandExt as _;

    let error = match std::env::current_exe() {
        Ok(current_exe) => {
            let mut eclipse = std::process::Command::new(current_exe);
            launch.restart(&mut eclipse);
            eclipse.exec()
        }
        Err(error) => error,
    };
    report_setup_failure(
        launch,
        HAND_OFF_CONTEXT,
        &format!("cannot restart Eclipse to keep the link out of its command line: {error}"),
    );
    ExitCode::FAILURE
}

fn start_client(launch: &LaunchCommand) -> ExitCode {
    match hand_off_before_restart(launch) {
        Ok(HandOff::Boot) => supervise(launch),
        Ok(HandOff::Delivered) => ExitCode::SUCCESS,
        Err(error) => {
            report_setup_failure(launch, HAND_OFF_CONTEXT, &error);
            ExitCode::FAILURE
        }
    }
}

fn hand_off_before_restart(launch: &LaunchCommand) -> Result<HandOff, String> {
    let launch = launch.launch();
    let Some(request) = launch.control_request() else {
        return Ok(HandOff::Boot);
    };
    let app_data_dir = eclipse::framework::app_data_dir().ok_or(NO_APP_DATA_DIR)?;
    let socket = instance_control::control_socket(&app_data_dir)?;
    hand_off(&launch, &request, &socket, &app_data_dir.join(RUNTIME_DIR))
}

fn hand_off(
    launch: &Launch,
    request: &Request,
    socket: &Path,
    runtime_dir: &Path,
) -> Result<HandOff, String> {
    let handed = instance_control::hand_off(socket, runtime_dir, request, |lock| {
        launch.already_running(lock)
    })?;
    if handed == HandOff::Delivered {
        println!("{}", launch.handed_off());
    }
    Ok(handed)
}

fn report_setup_failure(launch: &LaunchCommand, context: &str, error: &str) {
    eprintln!("{context}: {error}");
    if launch.launches_in_window() {
        show_error_window(error, None);
    }
}

fn client_settings_path() -> Result<PathBuf, String> {
    std::env::var_os(CLIENT_SETTINGS_PATH_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "the Android client-settings bridge did not load into the restarted Eclipse \
                 ({CLIENT_SETTINGS_PATH_ENV} is not set); start Eclipse without \
                 {CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV} in its environment"
            )
        })
}

fn verify_client_settings_redirect() -> Result<(), String> {
    std::fs::File::open(ANDROID_CLIENT_SETTINGS_PATH)
        .map(drop)
        .map_err(|error| {
            format!(
                "the Android client-settings bridge did not load into the restarted Eclipse \
                 ({ANDROID_CLIENT_SETTINGS_PATH} is not readable: {error}); see the ld.so \
                 message above, and keep Eclipse's app-data directory off noexec mounts or set \
                 ECLIPSE_APP_DATA_DIR to one that allows executable files"
            )
        })
}

fn supervise(launch: &LaunchCommand) -> ExitCode {
    let bridge = match ClientSettingsBridge::locate() {
        Ok(bridge) => bridge,
        Err(error) => {
            report_setup_failure(launch, SETTINGS_CONTEXT, &error);
            return ExitCode::FAILURE;
        }
    };
    let target = launch.launch();
    let run = match lock_run(&bridge.app_data_dir, &target) {
        Ok(RunStart::Locked(run)) => run,
        Ok(RunStart::HandedOff) => return ExitCode::SUCCESS,
        Err(error) => {
            report_setup_failure(launch, target.context(), &error);
            return ExitCode::FAILURE;
        }
    };
    let client = match bridge.client_command(launch) {
        Ok(client) => client,
        Err(error) => {
            report_setup_failure(launch, SETTINGS_CONTEXT, &error);
            return ExitCode::FAILURE;
        }
    };
    let output = supervisor::Output {
        stdout: std::io::stdout(),
        stderr: std::io::stderr(),
        echo_records: echo_client_records(),
    };
    match supervisor::run(client, run.lock, run.log, output) {
        Ok(finished) => {
            present_failure(launch, &target, &finished);
            finished.exit_code()
        }
        Err(error) => {
            report_setup_failure(launch, target.context(), &error);
            ExitCode::FAILURE
        }
    }
}

fn echo_client_records() -> bool {
    use std::io::IsTerminal as _;

    std::io::stderr().is_terminal() || std::env::var_os("RUST_LOG").is_some()
}

fn present_failure(launch: &LaunchCommand, target: &Launch, finished: &supervisor::Finished) {
    let Some(failure) = finished.failure() else {
        return;
    };
    eprintln!("{}: {failure}", target.context());
    if launch.launches_in_window() {
        show_error_window(&failure, Some(&finished.log));
    } else {
        eprintln!("Details are in {}", finished.log.display());
    }
}

struct ClientSettingsBridge {
    app_data_dir: PathBuf,
    runtime_dir: PathBuf,
}

impl ClientSettingsBridge {
    fn locate() -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt as _;

        let app_data_dir = eclipse::framework::app_data_dir().ok_or(NO_APP_DATA_DIR)?;
        let runtime_dir = app_data_dir.join(RUNTIME_DIR);
        std::fs::create_dir_all(&runtime_dir)
            .map_err(|error| format!("cannot create {}: {error}", runtime_dir.display()))?;
        let runtime_dir = runtime_dir
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", runtime_dir.display()))?;
        if runtime_dir
            .as_os_str()
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b':' | b';'))
        {
            return Err(format!(
                "the Android client-settings bridge directory {} contains a colon or semicolon, \
                 which LD_LIBRARY_PATH cannot carry; set ECLIPSE_APP_DATA_DIR to a directory \
                 without colons or semicolons",
                runtime_dir.display()
            ));
        }
        Ok(Self {
            app_data_dir,
            runtime_dir,
        })
    }

    fn client_command(&self, launch: &LaunchCommand) -> Result<std::process::Command, String> {
        stage_settings_shim(&self.runtime_dir)?;
        let current_exe = std::env::current_exe().map_err(|error| {
            format!("cannot locate the Eclipse executable to start Roblox: {error}")
        })?;
        let mut client = std::process::Command::new(current_exe);
        launch.restart(&mut client);
        client
            .env(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV, "1")
            .env(
                CLIENT_SETTINGS_PATH_ENV,
                self.runtime_dir.join(CLIENT_SETTINGS_FILE),
            )
            .env(
                "LD_LIBRARY_PATH",
                prepend_search_list_entry(
                    self.runtime_dir.as_os_str(),
                    std::env::var_os("LD_LIBRARY_PATH"),
                ),
            )
            .env(
                "LD_PRELOAD",
                prepend_search_list_entry(
                    std::ffi::OsStr::new(CLIENT_SETTINGS_PATH_SHIM_NAME),
                    std::env::var_os("LD_PRELOAD"),
                ),
            );
        Ok(client)
    }
}

fn run_client(launch: LaunchCommand, supervision: Option<Supervision>) -> ExitCode {
    if let Err(error) = client_settings_path() {
        report_setup_failure(&launch, SETTINGS_CONTEXT, &error);
        return match supervision {
            Some(supervision) => finish_android_process(ClientEnd::FailureShown, supervision.exit),
            None => ExitCode::FAILURE,
        };
    }
    let Some(Supervision {
        records,
        exit,
        run_log,
    }) = supervision
    else {
        report_setup_failure(&launch, launch.launch().context(), UNSUPERVISED);
        return ExitCode::FAILURE;
    };
    if let Err(error) = eclipse::loader::frame_log::arm_from_env() {
        report_setup_failure(&launch, FRAME_LOG_CONTEXT, &error.to_string());
        finish_android_process(ClientEnd::FailureShown, exit);
    }
    eclipse::diagnostics::init(eclipse::diagnostics::LogSink::Supervisor(records));
    tracing::debug!(version = eclipse::VERSION, "eclipse client starting");
    let loaded = eclipse_config::load();
    let end = match launch {
        LaunchCommand::Run => launch_in_window(&Launch::Installed, &loaded, &run_log),
        LaunchCommand::RunFile(path) => run_file(&path, &loaded),
        LaunchCommand::Link(target) => launch_in_window(&Launch::Link(target), &loaded, &run_log),
    };
    finish_android_process(end, exit)
}

fn prepend_search_list_entry(
    entry: &std::ffi::OsStr,
    inherited: Option<std::ffi::OsString>,
) -> std::ffi::OsString {
    let mut value = entry.to_os_string();
    if let Some(inherited) = inherited.filter(|inherited| !inherited.is_empty()) {
        value.push(":");
        value.push(inherited);
    }
    value
}

fn client_app_settings_json(fflags: &BTreeMap<String, serde_json::Value>) -> Vec<u8> {
    let mut settings = serde_json::Map::from_iter([(
        MAXIMUM_FRAME_RATE_ROW_FLAG.to_owned(),
        serde_json::Value::from("True"),
    )]);
    settings.extend(fflags.clone());
    format!("{:#}\n", serde_json::Value::Object(settings)).into_bytes()
}

fn stage_settings_shim(runtime_dir: &Path) -> Result<(), String> {
    let shim_is_current = std::fs::read(runtime_dir.join(CLIENT_SETTINGS_PATH_SHIM_NAME))
        .is_ok_and(|bytes| bytes.as_slice() == CLIENT_SETTINGS_PATH_SHIM);
    if !shim_is_current {
        replace_runtime_file(
            runtime_dir,
            CLIENT_SETTINGS_PATH_SHIM_NAME,
            CLIENT_SETTINGS_PATH_SHIM,
        )?;
    }
    Ok(())
}

fn write_client_settings(
    path: &Path,
    fflags: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let (Some(dir), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(format!(
            "{CLIENT_SETTINGS_PATH_ENV} does not name a file: {}",
            path.display()
        ));
    };
    replace_runtime_file(dir, name, &client_app_settings_json(fflags))
}

fn replace_runtime_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let path = dir.join(name);
    let write_error = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    let mut temp = eclipse_config::temp_file::TempFile::create(dir, name).map_err(write_error)?;
    temp.write_all(bytes).map_err(write_error)?;
    temp.persist(&path).map_err(write_error)
}

fn finish_android_process(end: ClientEnd, exit: ExitRecord) -> ! {
    use std::io::Write as _;

    if let Err(error) = exit.write(end) {
        eprintln!("eclipse: cannot tell Eclipse's supervisor how Roblox ended: {error}");
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    unsafe { libc::_exit(end.status()) }
}

fn report_config(loaded: &eclipse_config::Loaded, status: &StatusSink) {
    for problem in &loaded.problems {
        status.warning(problem.to_string());
    }
    if let Some(message) = loaded.unused_keys_message() {
        eclipse::diagnostics::record_status(tracing::Level::WARN, &message);
    }
}

fn show_config() -> ExitCode {
    let loaded = eclipse_config::load();
    if let Some(path) = &loaded.path {
        println!("# {}", path.display());
    }
    match serde_json::to_string_pretty(&loaded.config) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("eclipse config: {error}");
            return ExitCode::FAILURE;
        }
    }
    for problem in &loaded.problems {
        eprintln!("eclipse config: {problem}");
    }
    if loaded.problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

const NOT_INSTALLED: &str = "Roblox is not installed; run `eclipse update` to download it, or \
     install the APKs with `eclipse install <PATH>`";

const NO_APP_DATA_DIR: &str = "cannot resolve Eclipse's app-data directory; set HOME, \
     XDG_DATA_HOME, or ECLIPSE_APP_DATA_DIR";

const VERIFYING_SIGNATURE: &str = "Verifying the Roblox client's signature…";

const RUNTIME_DIR: &str = "runtime";

const CLIENT_SETTINGS_FILE: &str = "ClientAppSettings.json";

const PLAY_LOGIN_STEPS: &str = "\
Sign in to Google Play with your own Google account.

Risk: Eclipse talks to Google Play the way an Android device does and adds a device to the
account. Google's terms do not allow unofficial clients, so Google may restrict an account
that uses one. Use a secondary Google account, not your main one.

1. Open https://accounts.google.com/EmbeddedSetup in a web browser.
2. Sign in and accept the prompts until the page stops changing.
3. Open the browser's developer tools, find the accounts.google.com cookie named
   oauth_token (Storage or Application, then Cookies) and copy its value. It starts with
   oauth2_4/ and works only once.
";

fn install_command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    eclipse::runtime::android_cpu_baseline()?;
    if arguments.is_empty() {
        return Err("usage: eclipse install <APK | DIRECTORY | BUNDLE>...".into());
    }
    let sources: Vec<PathBuf> = arguments.iter().map(PathBuf::from).collect();
    let _client = lock_out_clients()?;
    let status = StatusSink::terminal();
    status.step("Verifying and installing the Roblox client…");
    let committed = Store::open()?.install(&sources, &status)?;
    status.outcome(format!(
        "installed Roblox {}",
        InstalledVersion::from(&committed.set)
    ));
    if let Some(leftover) = committed.leftover {
        status.warning(leftover.to_string());
    }
    Ok(())
}

fn play_login_command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    if !arguments.is_empty() {
        return Err("usage: eclipse play-login".into());
    }
    let account = eclipse::apk::play::Account::open()?;
    print!("{PLAY_LOGIN_STEPS}");
    let email = prompt_line("\nGoogle account email: ")?;
    let oauth_token = eclipse::apk::play::Secret::new(prompt_line("oauth_token value: ")?);
    println!("# Signing in and registering the device profile with Google Play…");
    let credentials = eclipse::apk::play::sign_in(&email, &oauth_token)?;
    account.save(&credentials)?;
    println!(
        "signed in to Google Play as {}; credentials saved to {} (readable only by you)",
        credentials.email,
        account.credentials_path().display()
    );
    println!("run `eclipse update --play` to download Roblox from Google Play");
    Ok(())
}

fn parse_libroblox_init_lib_dir(arguments: &[OsString]) -> Result<&std::path::Path, String> {
    match arguments {
        [lib_dir] => Ok(std::path::Path::new(lib_dir)),
        _ => Err("usage: eclipse __run-libroblox-init <LIB_DIR>".to_string()),
    }
}

fn prompt_line(prompt: &str) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Write as _;

    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Err("standard input closed before an answer was entered".into());
    }
    Ok(line.trim().to_owned())
}

#[derive(Debug, PartialEq, Eq)]
enum UpdateSource {
    ApkCombo,
    GooglePlay,
}

fn parse_update_source(arguments: &[OsString]) -> Result<UpdateSource, String> {
    match arguments {
        [] => Ok(UpdateSource::ApkCombo),
        [flag] if flag == "--play" => Ok(UpdateSource::GooglePlay),
        _ => Err("usage: eclipse update [--play]".to_string()),
    }
}

fn update_command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    eclipse::runtime::android_cpu_baseline()?;
    let source = parse_update_source(arguments)?;
    let _client = lock_out_clients()?;
    let store = Store::open()?;
    let current = store.usable_current()?;
    let status = StatusSink::terminal();
    match source {
        UpdateSource::ApkCombo => update_from_apkcombo(&store, current.as_ref(), None, &status),
        UpdateSource::GooglePlay => update_from_play(&store, current.as_ref(), &status),
    }
    .map(drop)
}

fn update_from_apkcombo(
    store: &Store,
    current: Option<&ApkSet>,
    rejected: Option<Release>,
    status: &StatusSink,
) -> Result<Option<ApkSet>, Box<dyn std::error::Error>> {
    status.step("Checking APKCombo for the newest Roblox client…");
    let outcome = eclipse::apk::apkcombo::update(store, current, rejected, status)?;
    finish_update(store, outcome, status)
}

fn update_from_play(
    store: &Store,
    current: Option<&ApkSet>,
    status: &StatusSink,
) -> Result<Option<ApkSet>, Box<dyn std::error::Error>> {
    let credentials = eclipse::apk::play::Account::open()?
        .credentials()?
        .ok_or("not signed in to Google Play; run `eclipse play-login` first")?;
    status.step("Checking Google Play for the newest Roblox client…");
    let outcome = eclipse::apk::play::update(&credentials, store, current, status)?;
    finish_update(store, outcome, status)
}

fn finish_update(
    store: &Store,
    outcome: UpdateOutcome,
    status: &StatusSink,
) -> Result<Option<ApkSet>, Box<dyn std::error::Error>> {
    store.record_check(&UpdateCheck {
        at: std::time::SystemTime::now(),
        rejected: None,
        outcome: CheckOutcome::Completed,
    })?;
    let (previous, committed) = match outcome {
        UpdateOutcome::UpToDate { installed } => {
            status.outcome(format!("Roblox {installed} is up to date"));
            return Ok(None);
        }
        UpdateOutcome::Updated {
            previous,
            committed,
        } => (previous, committed),
    };
    let Committed { set, leftover } = *committed;
    let installed = InstalledVersion::from(&set);
    match previous {
        Some(previous) => status.outcome(format!("updated Roblox from {previous} to {installed}")),
        None => status.outcome(format!("installed Roblox {installed}")),
    }
    if let Some(leftover) = leftover {
        status.warning(leftover.to_string());
    }
    Ok(Some(set))
}

fn update_if_due(
    store: &Store,
    installed: Option<VersionCode>,
    update: impl FnOnce(Option<Release>) -> Result<Option<ApkSet>, Box<dyn std::error::Error>>,
) -> Result<Option<ApkSet>, Box<dyn std::error::Error>> {
    let last_check = store.last_check()?;
    let now = std::time::SystemTime::now();
    if !eclipse::apk::store::update_due(installed, last_check.as_ref(), now) {
        return Ok(None);
    }
    let rejected = last_check.and_then(|check| check.rejected);
    let error = match update(rejected) {
        Ok(updated) => return Ok(updated),
        Err(error) => error,
    };
    if store.last_check()? == last_check {
        let failed = UpdateCheck {
            at: now,
            rejected,
            outcome: CheckOutcome::Failed,
        };
        if let Err(record) = store.record_check(&failed) {
            return Err(format!(
                "{error} (Eclipse could not record the failed check, so the next launch checks \
                 again: {record})"
            )
            .into());
        }
    }
    Err(error)
}

fn installed_apk_set(status: &StatusSink) -> Result<ApkSet, Box<dyn std::error::Error>> {
    let store = Store::open()?;
    status.step(VERIFYING_SIGNATURE);
    let set = installed_or_updated_set(&store, status, |current| {
        update_if_due(&store, current.map(ApkSet::version_code), |rejected| {
            update_from_apkcombo(&store, current, rejected, status)
        })
    })?;
    if let Some(cache) = eclipse::runtime::dalvik_cache_dir() {
        remove_other_version_oats(&cache, store.root(), set.base_path(), set.version_code())?;
    }
    status.step(format!(
        "Launching the installed Roblox {}",
        InstalledVersion::from(&set)
    ));
    Ok(set)
}

fn remove_other_version_oats(
    cache: &std::path::Path,
    store_root: &std::path::Path,
    apk: &std::path::Path,
    keep: eclipse::apk::VersionCode,
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;

    let (Some(mut prefix), Some(mut kept), Some(apk_name)) = (
        eclipse::runtime::dalvik_cache_stem(store_root),
        eclipse::runtime::dalvik_cache_stem(apk),
        apk.file_name(),
    ) else {
        return Err(format!(
            "the Roblox store {} and its APK {} must be absolute paths",
            store_root.display(),
            apk.display()
        ));
    };
    prefix.push("@");
    kept.push("@classes.dex");
    let mut artefact = apk_name.to_os_string();
    artefact.push("@classes.");
    let version_of = |name: &[u8]| -> Option<u32> {
        let rest = name.strip_prefix(prefix.as_bytes())?;
        let split = rest.iter().position(|&byte| byte == b'@')?;
        let (code, file) = (&rest[..split], &rest[split + 1..]);
        if !file.starts_with(artefact.as_bytes()) {
            return None;
        }
        std::str::from_utf8(code).ok()?.parse().ok()
    };
    if version_of(kept.as_bytes()) != Some(keep.0) {
        return Err(format!(
            "the installed Roblox APK {} is not in the version {} directory of the store {}",
            apk.display(),
            keep.0,
            store_root.display()
        ));
    }
    let list_error =
        |error: std::io::Error| format!("cannot list the ART cache {}: {error}", cache.display());
    let entries = match std::fs::read_dir(cache) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(list_error(error)),
    };
    for entry in entries {
        let entry = entry.map_err(list_error)?;
        if version_of(entry.file_name().as_bytes()).is_some_and(|code| code != keep.0) {
            let path = entry.path();
            std::fs::remove_file(&path).map_err(|error| {
                format!(
                    "cannot remove the old Roblox ART code {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn installed_or_updated_set(
    store: &Store,
    status: &StatusSink,
    update: impl FnOnce(Option<&ApkSet>) -> Result<Option<ApkSet>, Box<dyn std::error::Error>>,
) -> Result<ApkSet, Box<dyn std::error::Error>> {
    let current = match store.verified_current() {
        Err(error) if !error.is_unusable_install() => return Err(error.into()),
        current => current,
    };
    let verified = current.as_ref().ok().and_then(Option::as_ref);
    let verified_version = verified.map(ApkSet::version_code);
    let error = match update(verified) {
        Ok(Some(updated)) => return Ok(updated),
        Ok(None) => return Ok(current?.ok_or(NOT_INSTALLED)?),
        Err(error) => error,
    };
    let recorded = store.current()?.map(|installed| installed.version_code);
    let fallback = if recorded == verified_version {
        current
    } else {
        store.verified_current()
    };
    match fallback {
        Ok(Some(installed)) => {
            status.warning(format!("could not update Roblox: {error}"));
            Ok(installed)
        }
        Ok(None) => Err(format!("could not download Roblox: {error}").into()),
        Err(install) if install.is_unusable_install() => {
            Err(format!("{install}, and downloading Roblox failed: {error}").into())
        }
        Err(install) => Err(install.into()),
    }
}

fn install_url_handler_command(arguments: &[OsString]) -> Result<(), Box<dyn std::error::Error>> {
    if !arguments.is_empty() {
        return Err("usage: eclipse install-url-handler".into());
    }
    let outcome = desktop_integration::install_url_handler()?;
    println!("{}", url_handler_message(&outcome));
    let note = match Store::open() {
        Ok(store) => installed_client_note(&store),
        Err(error) => Some(error.to_string()),
    };
    if let Some(note) = note {
        println!("note: {note}");
    }
    Ok(())
}

fn installed_client_note(store: &Store) -> Option<String> {
    match store.current() {
        Ok(Some(_)) => None,
        Ok(None) => Some(NOT_INSTALLED.to_owned()),
        Err(error) => Some(error.to_string()),
    }
}

fn url_handler_message(outcome: &desktop_integration::UrlHandlerInstall) -> String {
    use desktop_integration::UrlHandlerInstall;

    match outcome {
        UrlHandlerInstall::Registered { desktop_path } => format!(
            "Roblox browser Play handler installed: {}",
            desktop_path.display()
        ),
        UrlHandlerInstall::FlatpakExport {
            app_id,
            desktop_path,
        } => desktop_integration::flatpak_handler_notice(app_id, desktop_path),
    }
}

fn native_lib_dir(root: NativeLibRoot, version: VersionCode) -> Result<PathBuf, String> {
    let root = match root {
        NativeLibRoot::Cache(root) => {
            remove_other_native_lib_versions(&root, version)?;
            root
        }
        NativeLibRoot::Override(root) => root,
    };
    Ok(root.join(version.to_string()))
}

fn remove_other_native_lib_versions(
    root: &std::path::Path,
    keep: eclipse::apk::VersionCode,
) -> Result<(), String> {
    let list_error = |error: std::io::Error| {
        format!(
            "cannot list the native-lib cache {}: {error}",
            root.display()
        )
    };
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(list_error(error)),
    };
    for entry in entries {
        let entry = entry.map_err(list_error)?;
        let other_version = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_some_and(|code| code != keep.0);
        if !other_version {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if file_type.is_dir() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                format!(
                    "cannot remove the old native libs in {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

enum Launch {
    Installed,
    File,
    Link(LaunchTarget),
}

impl Launch {
    fn context(&self) -> &'static str {
        match self {
            Self::Installed | Self::File => "eclipse run",
            Self::Link(_) => OPEN_CONTEXT,
        }
    }

    fn already_running(&self, lock: &Path) -> String {
        let advice = match self {
            Self::Installed | Self::File => {
                "switch to its window, or close it or wait for the install to finish before \
                 starting Roblox again"
            }
            Self::Link(_) => {
                "this link did not start a second copy; close Roblox in Eclipse or wait for the \
                 install to finish, then open the link again"
            }
        };
        format!(
            "Roblox is already running in Eclipse, or Eclipse is installing it; {advice} \
             (another Eclipse holds {})",
            lock.display()
        )
    }

    fn handed_off(&self) -> String {
        match self {
            Self::Installed | Self::File => {
                "Roblox is already running in Eclipse; its window was asked to come to the front"
                    .to_owned()
            }
            Self::Link(target) => format!("Roblox in Eclipse is starting and will open {target}"),
        }
    }

    fn target(&self) -> Option<&LaunchTarget> {
        match self {
            Self::Link(target) => Some(target),
            Self::Installed | Self::File => None,
        }
    }

    fn control_request(&self) -> Option<Request> {
        match self {
            Self::Installed => Some(Request::Show {}),
            Self::File => None,
            Self::Link(target) => Some(Request::open(target)),
        }
    }
}

fn window_title() -> String {
    eclipse::window_title(eclipse::apk::ROBLOX_PACKAGE)
}

fn show_error_window(message: &str, log: Option<&Path>) {
    if let Some(mut window) = open_error_window() {
        window.show_error(message, log);
    }
}

fn open_error_window() -> Option<LaunchWindow> {
    LaunchWindow::open(&window_title())
        .inspect_err(|error| eprintln!("eclipse: cannot open a window to show this error: {error}"))
        .ok()
}

fn report_failure(launch: &Launch, error: &str) {
    eprintln!("{}: {error}", launch.context());
    eclipse::diagnostics::record_status(tracing::Level::ERROR, error);
}

struct LockedRun {
    lock: std::fs::File,
    log: eclipse::diagnostics::RunLog,
}

enum RunStart {
    Locked(LockedRun),
    HandedOff,
}

fn lock_run(app_data_dir: &Path, launch: &Launch) -> Result<RunStart, String> {
    let control = match launch.control_request() {
        Some(_) => Some(instance_control::control_socket(app_data_dir)?),
        None => None,
    };
    lock_run_in(app_data_dir, launch, control.as_deref())
}

fn lock_run_in(
    app_data_dir: &Path,
    launch: &Launch,
    control: Option<&Path>,
) -> Result<RunStart, String> {
    let runtime_dir = app_data_dir.join(RUNTIME_DIR);
    let lock = match instance_control::lock_client(&runtime_dir)? {
        ClientLock::Acquired(lock) => lock,
        ClientLock::Held(lock) => {
            let (Some(socket), Some(request)) = (control, launch.control_request()) else {
                return Err(launch.already_running(&lock));
            };
            if hand_off(launch, &request, socket, &runtime_dir)? == HandOff::Delivered {
                return Ok(RunStart::HandedOff);
            }
            match instance_control::lock_client(&runtime_dir)? {
                ClientLock::Acquired(lock) => lock,
                ClientLock::Held(lock) => return Err(launch.already_running(&lock)),
            }
        }
    };
    let log = eclipse::diagnostics::RunLog::start(app_data_dir).map_err(|error| {
        format!(
            "cannot write Eclipse's log under {}: {error}",
            app_data_dir.display()
        )
    })?;
    Ok(RunStart::Locked(LockedRun { lock, log }))
}

struct SetupFailure {
    error: String,
    listener: Option<std::os::unix::net::UnixListener>,
}

impl From<String> for SetupFailure {
    fn from(error: String) -> Self {
        Self {
            error,
            listener: None,
        }
    }
}

fn start_client_run(
    launch: &Launch,
    fflags: &BTreeMap<String, serde_json::Value>,
) -> Result<Option<std::os::unix::net::UnixListener>, SetupFailure> {
    let control = match launch.control_request() {
        Some(_) => {
            let app_data_dir =
                eclipse::framework::app_data_dir().ok_or_else(|| NO_APP_DATA_DIR.to_owned())?;
            Some(instance_control::control_socket(&app_data_dir)?)
        }
        None => None,
    };
    start_client_run_in(control.as_deref(), client_settings_path(), fflags)
}

fn start_client_run_in(
    control: Option<&Path>,
    client_settings: Result<PathBuf, String>,
    fflags: &BTreeMap<String, serde_json::Value>,
) -> Result<Option<std::os::unix::net::UnixListener>, SetupFailure> {
    let listener = control.map(instance_control::listen).transpose()?;
    let prepared = client_settings
        .and_then(|path| stage_client_settings(&path, fflags))
        .and_then(|()| {
            eclipse::runtime::prepare_art_boot_environment().map_err(|error| error.to_string())
        });
    match prepared {
        Ok(()) => Ok(listener),
        Err(error) => Err(SetupFailure { error, listener }),
    }
}

fn stage_client_settings(
    path: &Path,
    fflags: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    write_client_settings(path, fflags)?;
    verify_client_settings_redirect()?;
    println!(
        "# Roblox Fast Flags staged at {} (Android {ANDROID_CLIENT_SETTINGS_PATH})",
        path.display()
    );
    Ok(())
}

fn lock_out_clients() -> Result<std::fs::File, String> {
    let app_data_dir = eclipse::framework::app_data_dir().ok_or(NO_APP_DATA_DIR)?;
    match instance_control::lock_client(&app_data_dir.join(RUNTIME_DIR))? {
        ClientLock::Acquired(lock) => Ok(lock),
        ClientLock::Held(lock) => Err(format!(
            "Roblox is running in Eclipse, or another Eclipse is installing it; close Roblox or \
             wait for that install to finish, then try again (another Eclipse holds {})",
            lock.display()
        )),
    }
}

fn run_file(path: &Path, loaded: &eclipse_config::Loaded) -> ClientEnd {
    let staged = client_settings_path()
        .and_then(|settings| stage_client_settings(&settings, &loaded.config.fflags));
    if let Err(error) = staged {
        report_failure(&Launch::File, &error);
        return ClientEnd::FailureShown;
    }
    match play_file(path, loaded) {
        Ok(()) => ClientEnd::Played,
        Err(error) => {
            report_failure(&Launch::File, &error.to_string());
            ClientEnd::FailureShown
        }
    }
}

fn play_file(
    path: &Path,
    loaded: &eclipse_config::Loaded,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = StatusSink::terminal();
    report_config(loaded, &status);
    let paths = ApkSetPaths::locate(path)?;
    eclipse::runtime::prepare_art_boot_environment()?;
    status.step(VERIFYING_SIGNATURE);
    let prepared = prepare_client(ApkSet::open(paths)?, &status)?;
    boot_and_play(
        prepared,
        None,
        &mut Host {
            status: &status,
            window: None,
        },
        &loaded.config,
    )
}

type Preparation = std::thread::JoinHandle<Result<PreparedClient, String>>;

fn launch_in_window(launch: &Launch, loaded: &eclipse_config::Loaded, log: &Path) -> ClientEnd {
    let (sender, updates) = std::sync::mpsc::channel();
    let status = StatusSink::with_window(sender.clone());
    report_config(loaded, &status);
    let listener = match start_client_run(launch, &loaded.config.fflags) {
        Ok(listener) => listener,
        Err(failure) => return show_setup_failure(launch, failure, log),
    };
    let preparation = prepare_in_background(sender);
    let mut window = match LaunchWindow::open(&window_title()) {
        Ok(window) => window,
        Err(error) => {
            report_failure(launch, &format!("cannot open the Eclipse window: {error}"));
            return ClientEnd::FailureShown;
        }
    };
    let slot = std::sync::Arc::new(LaunchSlot::new(launch.target().cloned()));
    if let Some(listener) = listener {
        let serving = instance_control::serve_in_background(
            listener,
            std::sync::Arc::clone(&slot),
            window.control(),
        );
        if let Err(error) = serving {
            let error = format!("cannot take launches from other Eclipse processes: {error}");
            report_failure(launch, &error);
            window.show_error(&error, Some(log));
            return ClientEnd::FailureShown;
        }
    }
    let played = preparation.map_err(Into::into).and_then(|worker| {
        play_in_window(
            &mut window,
            &status,
            &updates,
            worker,
            &slot,
            &loaded.config,
        )
    });
    let Err(error) = played else {
        slot.end();
        return ClientEnd::Played;
    };
    if slot.closing() {
        tracing::info!("Roblox closed because another Eclipse launch asked it to");
        return ClientEnd::ClosedForAnotherLaunch;
    }
    slot.end();
    let text = error.to_string();
    report_failure(launch, &text);
    window.show_error(&text, Some(&eclipse::diagnostics::newest_run_part(log)));
    if slot.closing() {
        ClientEnd::ClosedForAnotherLaunch
    } else if error.is::<WindowClosed>() {
        ClientEnd::WindowClosed
    } else {
        ClientEnd::FailureShown
    }
}

fn show_setup_failure(launch: &Launch, failure: SetupFailure, log: &Path) -> ClientEnd {
    report_failure(launch, &failure.error);
    let Some(mut window) = open_error_window() else {
        return ClientEnd::FailureShown;
    };
    let slot = std::sync::Arc::new(LaunchSlot::new(None));
    slot.end();
    if let Some(listener) = failure.listener {
        let serving = instance_control::serve_in_background(
            listener,
            std::sync::Arc::clone(&slot),
            window.control(),
        );
        if let Err(error) = serving {
            tracing::warn!(
                %error,
                "later Eclipse launches cannot close this error and will report Roblox as running"
            );
        }
    }
    window.show_error(&failure.error, Some(log));
    if slot.closing() {
        ClientEnd::ClosedForAnotherLaunch
    } else {
        ClientEnd::FailureShown
    }
}

fn prepare_in_background(
    updates: std::sync::mpsc::Sender<StatusUpdate>,
) -> std::io::Result<Preparation> {
    std::thread::Builder::new()
        .name("eclipse-install".to_owned())
        .spawn(move || {
            let status = StatusSink::with_window(updates);
            installed_apk_set(&status)
                .and_then(|apks| prepare_client(apks, &status))
                .map_err(|error| error.to_string())
        })
}

fn play_in_window(
    window: &mut LaunchWindow,
    status: &StatusSink,
    updates: &std::sync::mpsc::Receiver<StatusUpdate>,
    worker: Preparation,
    slot: &LaunchSlot,
    config: &eclipse_config::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    window.wait_for(updates, &worker)?;
    let prepared = worker
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
    let target = slot.begin_play();
    boot_and_play(
        prepared,
        target.as_ref(),
        &mut Host {
            status,
            window: Some((window, updates)),
        },
        config,
    )
}

struct Host<'a> {
    status: &'a StatusSink,
    window: Option<(
        &'a mut LaunchWindow,
        &'a std::sync::mpsc::Receiver<StatusUpdate>,
    )>,
}

impl Host<'_> {
    fn step(&mut self, text: String) -> Result<(), WindowClosed> {
        self.status.step(text);
        self.refresh()
    }

    fn refresh(&mut self) -> Result<(), WindowClosed> {
        match &mut self.window {
            Some((window, updates)) => window.refresh(updates),
            None => Ok(()),
        }
    }

    fn run_game(
        &mut self,
        title: &str,
        vm: &eclipse::runtime::Vm,
        touch_mode: eclipse_config::TouchMode,
    ) -> Result<(), eclipse::graphics::GraphicsError> {
        match &mut self.window {
            Some((window, _)) => {
                let (event_loop, activation_token, commands) = window.event_loop();
                eclipse::graphics::run_windowed(
                    event_loop,
                    activation_token,
                    title,
                    Some(vm),
                    touch_mode,
                    Some(commands),
                )
            }
            None => eclipse::graphics::run_windowed(
                eclipse::graphics::host_event_loop()?,
                None,
                title,
                Some(vm),
                touch_mode,
                None,
            ),
        }
    }
}

struct PreparedClient {
    apks: ApkSet,
    app_lib_dir: PathBuf,
    client_cache: ClientCacheDir,
}

fn prepare_client(
    mut apks: ApkSet,
    status: &StatusSink,
) -> Result<PreparedClient, Box<dyn std::error::Error>> {
    let client_cache = prepare_client_cache(status)?;
    let app_lib_dir = native_lib_dir(eclipse::runtime::native_lib_root()?, apks.version_code())?;
    status.step(format!(
        "Extracting native libs (lib/x86_64/) to {}…",
        app_lib_dir.display()
    ));
    let lib_count = apks
        .native_libs_mut()
        .extract_native_libs(eclipse::apk::TARGET_ABI, &app_lib_dir)?;
    println!("extracted {lib_count} native lib(s) ✓");

    let assets_dir = eclipse::framework::app_data_dir()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot resolve the app data directory (no $HOME/XDG base and ECLIPSE_APP_DATA_DIR \
                 unset); set ECLIPSE_APP_DATA_DIR to the engine content root",
            )
        })?
        .join("files")
        .join("assets");
    status.step(format!(
        "Extracting Roblox bundled assets (assets/ → files/assets/) to {}…",
        assets_dir.display()
    ));
    let asset_count = apks.base_mut().extract_assets(&assets_dir)?;
    println!("extracted {asset_count} asset file(s) ✓");
    Ok(PreparedClient {
        apks,
        app_lib_dir,
        client_cache,
    })
}

fn prepare_client_cache(status: &StatusSink) -> Result<ClientCacheDir, Box<dyn std::error::Error>> {
    let client_cache = eclipse::runtime::client_cache_dir()?;
    eclipse::storage::create_client_cache(client_cache.path())?;
    match eclipse::storage::trim_client_cache(
        client_cache.path(),
        eclipse::storage::CLIENT_CACHE_CAP,
        std::time::SystemTime::now(),
    ) {
        Ok(Trim::Done { freed_bytes }) if freed_bytes > 0 => tracing::info!(
            freed_bytes,
            cache = %client_cache.path().display(),
            "trimmed Roblox's cache to three quarters of its cap"
        ),
        Ok(_) => {}
        Err(error) => status.warning(format!("could not trim Roblox's cache: {error}")),
    }
    Ok(client_cache)
}

fn boot_and_play(
    prepared: PreparedClient,
    target: Option<&LaunchTarget>,
    host: &mut Host<'_>,
    config: &eclipse_config::Config,
) -> Result<(), Box<dyn std::error::Error>> {
    let PreparedClient {
        apks,
        app_lib_dir,
        client_cache,
    } = prepared;
    let base_path = apks.base_path().to_path_buf();
    let apk_path = base_path
        .to_str()
        .ok_or("the Roblox APK path is not valid UTF-8")?
        .to_owned();

    eclipse::loader::ndk_registry::set_apk_path(base_path.clone());
    let manifest = apks.manifest().clone();
    eclipse::webview::client::use_helper_path(config.webview_helper_path.clone())?;
    eclipse::performance::configure_engine_cpu_affinity(config.graphics_optimization_mode);
    let plan = eclipse::runtime::BootPlan::new(&manifest, config, client_cache);
    let link = target.map(|target| (target, target.android_uri()));
    let start = match &link {
        None => ActivityStart::Launcher(&plan.launcher_activity),
        Some((target, uri)) => {
            let activity = manifest.resolve_view_activity(uri).ok_or_else(|| {
                let version = apks.version_name().map_or_else(
                    || format!("versionCode {}", apks.version_code()),
                    str::to_owned,
                );
                format!(
                    "Roblox {version} does not accept {} links; update Roblox or report this.",
                    target.kind()
                )
            })?;
            ActivityStart::View { activity, uri }
        }
    };

    println!("# ART boot plan for {apk_path}");
    println!("package:            {}", manifest.package);
    println!(
        "version:            {} (versionCode {})",
        apks.version_name().unwrap_or("unnamed"),
        apks.version_code()
    );
    println!("launcher_activity:  {}", plan.launcher_activity);
    if let Some((target, _)) = &link {
        println!("link:               {target} via {}", start.activity());
    }
    println!("sdk_int:            {}", plan.sdk_int);
    println!(
        "heap:               {} MiB (DisableHSpaceCompactForOOM={})",
        plan.heap_mib, plan.disable_hspace_compact
    );
    println!("instruction_set:    {}", plan.instruction_set_features);

    println!("\n# VM options (-> JNI_CreateJavaVM):");
    for opt in plan.vm_options() {
        println!("    {opt}");
    }

    host.step(format!(
        "Starting Roblox {}…",
        apks.version_name().unwrap_or("")
    ))?;
    println!("\n# Booting the ART VM with Roblox on the classpath…");

    let vm = eclipse::runtime::boot(&plan, Some(&base_path), Some(&app_lib_dir))?;
    println!("ART VM booted with Roblox's Java on the classpath ✓");
    host.refresh()?;

    println!("# Provisioning bionic sonames (libm.so → Eclipse apkenv-loadable shim) …");
    eclipse::runtime::provision_bionic_sonames(&app_lib_dir)?;
    println!("bionic sonames provisioned (Eclipse libm shim) ✓");

    let fw = eclipse::runtime::find_framework()?;
    println!("# Whitelisting the app-lib dir in the bionic linker search path…");
    eclipse::runtime::whitelist_bionic_library_path(&fw, Some(&app_lib_dir))?;
    println!("bionic linker search path whitelisted (dl_parse_library_path) ✓");

    println!("# Registering engine-JNI_OnLoad-reachable framework natives (Log + Process + SystemClock)…");
    eclipse::framework::register_engine_preload_natives(&vm)?;
    println!("engine-preload framework natives registered ✓");

    let _preloaded_libs = preload_app_native_libs(apks.native_libs(), &app_lib_dir)?;
    host.refresh()?;

    if let Some((target, _)) = &link {
        host.step(format!("Joining {target}…"))?;
    }
    println!("# Driving the framework lifecycle (JNI; steps 1–7 to Activity.onResume / RESUMED)…");
    let progress = eclipse::framework::drive_application_lifecycle(
        &vm,
        &apk_path,
        &app_lib_dir,
        apks.signing_certificate_history(),
        start,
    )?;
    println!(
        "framework lifecycle driven: {progress:?} (non-GTK Context/Window/View natives bound; \
         started Activity = {}) ✓",
        start.activity()
    );
    host.refresh()?;

    if std::env::var("ECLIPSE_WEB_LOGIN").is_ok_and(|value| value == "1") {
        println!("# Opening Roblox's official web login in Eclipse…");
        let handle = eclipse::framework::drive_roblox_web_login(&vm)?;
        println!("official Roblox web login opened (WebView handle {handle}) ✓");
    }

    println!("# Opening the host window (winit; close it to exit)…");
    host.run_game(
        &eclipse::window_title(&manifest.package),
        &vm,
        config.touch_mode,
    )?;
    Ok(())
}

fn preload_app_native_libs(
    apk: &eclipse::apk::Apk,
    app_lib_dir: &std::path::Path,
) -> Result<Vec<eclipse::loader::engine::PreloadedLib>, Box<dyn std::error::Error>> {
    use eclipse::apk::{ENGINE_LIB, TARGET_ABI};

    let mut log = std::io::stdout();
    let mut loaded: Vec<eclipse::loader::engine::PreloadedLib> = Vec::new();

    println!("# Pre-loading the native engine via Eclipse's Rust loader (NOT the apkenv linker)…");
    let engine = eclipse::loader::engine::load_app_native_lib(app_lib_dir, ENGINE_LIB, &mut log)?
        .ok_or("libroblox.so unexpectedly deduped on first load")?;
    report_preloaded(&engine);
    loaded.push(engine);

    let filenames = apk.native_lib_filenames(TARGET_ABI);
    println!(
        "# Pre-loading {} other x86_64 JNI lib(s) via the Rust loader (tolerant of per-lib failure)…",
        filenames.iter().filter(|f| *f != ENGINE_LIB).count()
    );
    for filename in &filenames {
        if filename == ENGINE_LIB {
            continue;
        }
        match eclipse::loader::engine::load_app_native_lib(app_lib_dir, filename, &mut log) {
            Ok(Some(lib)) => {
                report_preloaded(&lib);
                loaded.push(lib);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("# WARNING: pre-load of {filename} failed (continuing): {e}");
            }
        }
    }

    println!(
        "engine pre-load complete: {} x86_64 JNI lib(s) loaded via the Rust loader ✓",
        loaded.len()
    );
    Ok(loaded)
}

struct WebViewTestReport {
    load_upcalls: u32,
    started_ms: u128,
    finished_ms: u128,
}

impl std::fmt::Display for WebViewTestReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WebView engine pipeline OK: internalLoadChanged upcalls {}/3 (state 0 @ {}ms, \
             state 3 @ {}ms), page URL OK, bridge round-trip OK, evaluateJavascript OK, honest \
             UA OK, page cookies OK, cookie set/get OK, cookie callback OK, cookie flush OK, \
             ViewClosed, idle stop OK, helper restart OK, helper exit 0",
            self.load_upcalls, self.started_ms, self.finished_ms
        )
    }
}

const WEBVIEW_TEST_PAGE: &str = "<!doctype html><meta charset=utf-8><title>eclipse</title>\
<body style=\"background:#2244aa;color:#fff;font-size:40px\">Eclipse WebView M4\
<script>window.__eclipseUA=navigator.userAgent;\
function eclipseBridge(){\
if(window.EclipseTest&&window.EclipseTest.echo){\
window.EclipseTest.echo('PING').then(function(r){window.__eclipseBridgeResult=r;},\
function(e){window.__eclipseBridgeResult='ERR:'+e;});}\
else{setTimeout(eclipseBridge,50);}}\
eclipseBridge();</script></body>";

const WEBVIEW_TEST_PAGE_COOKIE: &str = "ECLIPSE_PAGE=1; Max-Age=3600; Path=/";

const REQUEST_HEAD_LIMIT: usize = 16 * 1024;

fn read_request_head(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut head = Vec::new();
    let mut chunk = [0u8; 2048];
    while !head.windows(4).any(|end| end == b"\r\n\r\n") && head.len() < REQUEST_HEAD_LIMIT {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&chunk[..n]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn request_cookie_names(head: &str) -> String {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("cookie"))
        .flat_map(|(_, cookies)| cookies.split(';'))
        .filter_map(|pair| pair.split_once('=').map(|(name, _)| name.trim()))
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn start_loopback_page() -> std::io::Result<u16> {
    use std::io::Write;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let head = read_request_head(&mut stream);
            let path = head.split_whitespace().nth(1).unwrap_or("/");
            let resp = if path == "/" || path.starts_with("/?") {
                let body = WEBVIEW_TEST_PAGE.replacen(
                    "<script>",
                    &format!(
                        "<script>window.__eclipseServerCookies=\"{}\";",
                        request_cookie_names(&head)
                    ),
                    1,
                );
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Set-Cookie: {WEBVIEW_TEST_PAGE_COOKIE}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            };
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Ok(port)
}

fn has_cookie(cookies: &str, pair: &str) -> bool {
    cookies.split("; ").any(|cookie| cookie == pair)
}

fn pump_tick(vm: &eclipse::runtime::Vm, ms: u64) {
    if let Err(e) = eclipse::framework::pump_main_looper(vm) {
        eprintln!("# main Looper pump failed: {e}");
    }
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

fn run_platform_test(
) -> Result<eclipse::framework::platform_probe::PlatformProbeReport, Box<dyn std::error::Error>> {
    let paths = eclipse::apk::ApkSetPaths::from_env()?.ok_or_else(|| {
        format!(
            "no Roblox APK (set {} to an APK file or to a directory holding {} and {}) — \
             __platform-test boots ART with the installed framework on the classpath",
            eclipse::apk::DEV_APK_ENV,
            eclipse::apk::BASE_APK,
            eclipse::apk::NATIVE_SPLIT_APK
        )
    })?;
    let apks = eclipse::apk::ApkSet::open(paths)?;
    let loaded = eclipse_config::load();
    report_config(&loaded, &StatusSink::terminal());
    let plan = eclipse::runtime::BootPlan::new(
        apks.manifest(),
        &loaded.config,
        eclipse::runtime::client_cache_dir()?,
    );
    let vm = eclipse::runtime::boot(&plan, Some(apks.base_path()), None)?;
    eclipse::framework::register_engine_preload_natives(&vm)?;
    Ok(eclipse::framework::platform_probe::run(&vm)?)
}

fn run_webview_test() -> Result<WebViewTestReport, Box<dyn std::error::Error>> {
    use eclipse::framework;
    use eclipse::webview::client;
    use std::time::{Duration, Instant};

    const START_DEADLINE: Duration = Duration::from_secs(30);
    const FINISH_DEADLINE: Duration = Duration::from_secs(90);
    const UPCALL_DEADLINE: Duration = Duration::from_secs(10);
    const LEG_DEADLINE: Duration = Duration::from_secs(15);
    const CLOSE_DEADLINE: Duration = Duration::from_secs(15);

    let wayland_set = std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty());
    let display_set = std::env::var("DISPLAY").is_ok_and(|v| !v.is_empty());
    match (wayland_set, display_set) {
        (true, _) => println!("# display: wayland (WAYLAND_DISPLAY set)"),
        (false, true) => println!("# display: x11 (DISPLAY set, WAYLAND_DISPLAY unset)"),
        (false, false) => {
            return Err(
                "no display detected: neither WAYLAND_DISPLAY nor DISPLAY is set — the \
                 WebKitGTK helper needs a Wayland or X11 session"
                    .into(),
            )
        }
    }

    let port = start_loopback_page()?;
    let target_url = format!("http://127.0.0.1:{port}/");
    println!("# __webview-test: loopback page serving at {target_url}");

    let paths = eclipse::apk::ApkSetPaths::from_env()?.ok_or_else(|| {
        format!(
            "no Roblox APK (set {} to an APK file or to a directory holding {} and {}) — \
             __webview-test boots ART with the installed framework on the classpath",
            eclipse::apk::DEV_APK_ENV,
            eclipse::apk::BASE_APK,
            eclipse::apk::NATIVE_SPLIT_APK
        )
    })?;
    let apks = eclipse::apk::ApkSet::open(paths)?;
    println!(
        "# __webview-test: booting ART from {} (framework classpath; no libroblox preload, \
         no lifecycle, no window)…",
        apks.base_path().display()
    );
    let loaded = eclipse_config::load();
    report_config(&loaded, &StatusSink::terminal());
    client::use_helper_path(loaded.config.webview_helper_path.clone())?;
    let plan = eclipse::runtime::BootPlan::new(
        apks.manifest(),
        &loaded.config,
        eclipse::runtime::client_cache_dir()?,
    );
    let vm = eclipse::runtime::boot(&plan, Some(apks.base_path()), None)?;

    eclipse::framework::register_engine_preload_natives(&vm)?;

    eclipse::framework::prepare_main_looper(&vm)?;
    framework::register_cookie_manager(&vm)?;
    if std::env::var("ECLIPSE_WEBVIEW_EXPECT_PERSISTED_TEST_COOKIE").as_deref() == Ok("1") {
        let restored = framework::cookie_manager_get_cookie(&vm, &target_url);
        for pair in ["ECLIPSE_TEST=1", "ECLIPSE_HTTPONLY=1", "ECLIPSE_PAGE=1"] {
            if !has_cookie(&restored, pair) {
                return Err(format!(
                    "getCookie before the first view did not restore {pair} from the jar"
                )
                .into());
            }
        }
        if client::helper_running() {
            return Err("getCookie before the first view started the web engine helper".into());
        }
        println!(
            "# persisted cookies OK (served from the jar before any view, values not printed)"
        );
    }
    framework::cookie_manager_set_cookie_cb(&vm, &target_url, "ECLIPSE_PRE=1; Path=/")
        .map_err(|e| format!("CookieManager.setCookie(3-arg) before the first view failed: {e}"))?;
    if framework::read_probe_last_value(&vm).is_some() {
        return Err(
            "the jar ran the setCookie ValueCallback inside the call instead of posting it \
             to the main Looper"
                .into(),
        );
    }
    framework::cookie_manager_set_cookie(&vm, &target_url, "ECLIPSE_HTTPONLY=1; HttpOnly; Path=/")
        .map_err(|e| format!("CookieManager.setCookie before the first view failed: {e}"))?;
    pump_tick(&vm, 0);
    if !framework::read_probe_last_value(&vm).is_some_and(|v| v.contains("true")) {
        return Err(
            "the jar's setCookie ValueCallback did not get Boolean.TRUE at the next Looper pump"
                .into(),
        );
    }
    if client::helper_running() {
        return Err("setCookie before the first view started the web engine helper".into());
    }
    println!("# jar answer posted OK (the ValueCallback ran at the next Looper pump)");
    println!("# ART booted ✓ — driving the WebView smoke (register → alloc → setWebViewClient → addJavascriptInterface → loadUrl)…");
    let start = Instant::now();
    let handle = eclipse::framework::drive_webview_smoke(&vm, &target_url)?;

    let fail_reason =
        || client::failed_reason().map(|r| format!("web engine helper unavailable: {r}"));
    let mut started_ms: Option<u128> = None;
    let finished_ms = loop {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        let obs = client::load_observed(handle);
        if let Some(obs) = obs {
            if obs.started && started_ms.is_none() {
                started_ms = Some(start.elapsed().as_millis());
                println!(
                    "# load-state 0 observed @ {} ms",
                    start.elapsed().as_millis()
                );
            }
            if obs.finished {
                let finished_ms = start.elapsed().as_millis();
                println!("# load-state 3 observed, first view @ {finished_ms} ms");
                break finished_ms;
            }
        }
        if started_ms.is_none() && start.elapsed() > START_DEADLINE {
            return Err("load-started (internalLoadChanged 0) not observed within 30 s".into());
        }
        if start.elapsed() > FINISH_DEADLINE {
            return Err("load-finished (internalLoadChanged 3) not observed within 90 s".into());
        }
        pump_tick(&vm, 50);
    };
    let started_ms = started_ms.ok_or("load-finished arrived without load-started")?;

    let upcall_deadline = Instant::now() + UPCALL_DEADLINE;
    let load_upcalls = loop {
        let delivered = client::load_observed(handle)
            .map(|o| o.load_upcalls)
            .unwrap_or(0);
        let page_url = client::url(handle);
        if delivered >= 3 && page_url.as_deref() == Some(target_url.as_str()) {
            break delivered;
        }
        if Instant::now() > upcall_deadline {
            return Err(format!(
                "within 10 s of load-finish only {delivered}/3 internalLoadChanged upcalls \
                 completed and WebView.getUrl reported the loaded page: {}",
                page_url.as_deref() == Some(target_url.as_str())
            )
            .into());
        }
        pump_tick(&vm, 50);
    };
    println!("# page URL OK (WebView.getUrl follows the engine)");

    let eval_and_wait = |view: i64, script: &str| -> Option<String> {
        if framework::webview_evaluate(&vm, view, script).is_err() {
            return None;
        }
        let end = Instant::now() + LEG_DEADLINE;
        loop {
            if let Some(v) = framework::read_probe_last_value(&vm) {
                return Some(v);
            }
            if Instant::now() > end {
                return None;
            }
            pump_tick(&vm, 50);
        }
    };

    let ua = eval_and_wait(handle, "navigator.userAgent")
        .ok_or("evaluateJavascript(navigator.userAgent) produced no result within 15 s")?;
    if !(ua.contains("Eclipse-WebView") && ua.contains("Chrome/152"))
        || ua.contains("GDPR VIOLATION")
    {
        return Err(
            "navigator.userAgent is not the honest Eclipse UA (evaluateJavascript/UA leg failed)"
                .into(),
        );
    }
    println!("# evaluateJavascript OK; honest UA OK (UA value not printed)");

    let bridge_deadline = Instant::now() + LEG_DEADLINE;
    loop {
        if let Some(r) = eval_and_wait(handle, "window.__eclipseBridgeResult||''") {
            if r.contains("echo:PING") {
                break;
            }
        }
        if Instant::now() > bridge_deadline {
            return Err("bridge round-trip did not complete (window.__eclipseBridgeResult != echo:PING within 15 s)".into());
        }
        pump_tick(&vm, 100);
    }

    match framework::read_probe_last(&vm).as_deref() {
        Some("PING") => {
            println!("# bridge round-trip OK (page JS → JNI reflect-invoke → async result)")
        }
        other => {
            return Err(format!(
                "EclipseBridgeProbe.last != PING (JNI reflect-invoke leg failed: {other:?})"
            )
            .into())
        }
    }

    let server_saw = |view: i64| -> Result<Vec<String>, String> {
        let names = eval_and_wait(view, "window.__eclipseServerCookies||''")
            .ok_or("the page's server cookie list produced no result within 15 s")?;
        Ok(names
            .trim_matches('"')
            .split(',')
            .map(str::to_string)
            .collect())
    };
    let server_cookies = server_saw(handle)?;
    if !["ECLIPSE_PRE", "ECLIPSE_HTTPONLY"]
        .iter()
        .all(|name| server_cookies.iter().any(|seen| seen == name))
    {
        return Err("the page request did not carry the cookies set before the first view".into());
    }
    let document_cookies = eval_and_wait(handle, "document.cookie")
        .ok_or("document.cookie produced no result within 15 s")?;
    let document_cookies = document_cookies.trim_matches('"');
    if !has_cookie(document_cookies, "ECLIPSE_PRE=1")
        || document_cookies.contains("ECLIPSE_HTTPONLY")
    {
        return Err("document.cookie must show ECLIPSE_PRE and hide the HttpOnly cookie".into());
    }
    let page_cookie_deadline = Instant::now() + LEG_DEADLINE;
    while !has_cookie(
        &framework::cookie_manager_get_cookie(&vm, &target_url),
        "ECLIPSE_PAGE=1",
    ) {
        if Instant::now() > page_cookie_deadline {
            return Err("getCookie did not return the page's ECLIPSE_PAGE within 15 s".into());
        }
        pump_tick(&vm, 100);
    }
    println!(
        "# page cookies OK (the jar reached the request, HttpOnly stayed hidden, the page's cookie \
         reached getCookie)"
    );
    framework::cookie_manager_set_cookie(&vm, &target_url, "ECLIPSE_TEST=1; Path=/")
        .map_err(|e| format!("CookieManager.setCookie(2-arg) failed: {e}"))?;
    let cookie_deadline = Instant::now() + LEG_DEADLINE;
    loop {
        let got = framework::cookie_manager_get_cookie(&vm, &target_url);
        if got.contains("ECLIPSE_TEST=1") {
            break;
        }
        if Instant::now() > cookie_deadline {
            return Err("CookieManager.getCookie did not return ECLIPSE_TEST=1 within 15 s".into());
        }
        pump_tick(&vm, 100);
    }
    println!("# cookie set/get OK (values not printed)");

    framework::cookie_manager_set_cookie_cb(&vm, &target_url, "ECLIPSE_CB=1; Path=/")
        .map_err(|e| format!("CookieManager.setCookie(3-arg) failed: {e}"))?;
    let cb_deadline = Instant::now() + LEG_DEADLINE;
    let cb_ok = loop {
        if let Some(v) = framework::read_probe_last_value(&vm) {
            if v.contains("true") {
                break true;
            }
        }
        if Instant::now() > cb_deadline {
            break false;
        }
        pump_tick(&vm, 50);
    };
    if !cb_ok {
        return Err(
            "3-arg setCookie ValueCallback did not fire with Boolean.TRUE within 15 s".into(),
        );
    }
    println!("# cookie callback OK (real Boolean.TRUE, not fabricated)");
    framework::cookie_manager_flush(&vm).map_err(|e| format!("CookieManager.flush failed: {e}"))?;
    println!("# cookie flush OK (the engine saved its session cookies)");

    let close = |view: i64| -> Result<(), String> {
        client::close_view(view).map_err(|e| format!("CloseView send failed: {e}"))?;
        let close_deadline = Instant::now() + CLOSE_DEADLINE;
        while client::view_close_pending(view) {
            if let Some(reason) = fail_reason() {
                return Err(reason);
            }
            if Instant::now() > close_deadline {
                return Err("ViewClosed not observed within 15 s".into());
            }
            pump_tick(&vm, 50);
        }
        Ok(())
    };
    close(handle)?;
    println!("# view-closed ✓ — waiting for the idle helper to stop…");

    let closed = Instant::now();
    let idle_wait = client::HELPER_IDLE_GRACE + Duration::from_secs(5);
    let mut stopped_after = None;
    while closed.elapsed() < idle_wait {
        if stopped_after.is_none() && !client::helper_running() {
            stopped_after = Some(start.elapsed());
        }
        pump_tick(&vm, 100);
    }
    let stopped_after = stopped_after.ok_or_else(|| {
        format!(
            "the helper still ran {} s after its page closed",
            idle_wait.as_secs()
        )
    })?;
    if stopped_after < client::HELPER_IDLE_GRACE {
        return Err(format!(
            "the helper stopped {} ms after the page loaded, before its idle grace",
            stopped_after.as_millis()
        )
        .into());
    }
    if !has_cookie(
        &framework::cookie_manager_get_cookie(&vm, &target_url),
        "ECLIPSE_PAGE=1",
    ) || client::helper_running()
    {
        return Err(
            "getCookie after the idle stop did not answer ECLIPSE_PAGE from the jar".into(),
        );
    }
    println!("# idle stop OK (no page shown for the grace; the jar answers the page's cookie)");

    let again = eclipse::framework::drive_webview_smoke(&vm, &target_url)?;
    let again_deadline = Instant::now() + FINISH_DEADLINE;
    while !client::load_observed(again).is_some_and(|obs| obs.finished) {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        if Instant::now() > again_deadline {
            return Err("the page loaded after the idle stop did not finish within 90 s".into());
        }
        pump_tick(&vm, 50);
    }
    if !client::helper_running() {
        return Err("a page loaded after the idle stop did not start the helper again".into());
    }
    if !server_saw(again)?.iter().any(|seen| seen == "ECLIPSE_PAGE") {
        return Err("the restarted helper did not send the jar's ECLIPSE_PAGE".into());
    }
    close(again)?;
    println!("# helper restart OK (the next page started the helper with the jar's cookies)");
    println!("# shutting the helper down…");
    let report = client::shutdown(&vm, Duration::from_secs(15));
    if report.helper_exit != Some(0) {
        return Err(format!(
            "helper exit status {:?} (expected 0; reader_joined={})",
            report.helper_exit, report.reader_joined
        )
        .into());
    }
    Ok(WebViewTestReport {
        load_upcalls,
        started_ms,
        finished_ms,
    })
}

fn report_preloaded(lib: &eclipse::loader::engine::PreloadedLib) {
    let ctors = if lib.constructors_run > 0 {
        format!("{} ctor(s)", lib.constructors_run)
    } else {
        "no ctors".to_string()
    };
    let onload = match lib.jni_onload {
        eclipse::loader::engine::JniOnLoad::DeferredToLoadLibrary => {
            "JNI_OnLoad runs at System.loadLibrary"
        }
        eclipse::loader::engine::JniOnLoad::Absent => "no JNI_OnLoad",
    };
    println!("  {} ✓ ({ctors}; {onload})", lib.soname);
}

#[cfg(test)]
mod tests {
    use super::{
        finish_android_process, installed_client_note, installed_or_updated_set, lock_run_in,
        native_lib_dir, parse_libroblox_init_lib_dir, parse_update_source,
        remove_other_native_lib_versions, remove_other_version_oats, update_if_due,
        url_handler_message, ClientEnd, ClientLock, ExitRecord, Launch, LaunchCommand,
        LaunchCommandError, RunStart, UpdateSource, LAUNCH_LINK_COMMAND, LAUNCH_LINK_ENV,
        NOT_INSTALLED, OPEN_USAGE, RUN_USAGE,
    };
    use crate::desktop_integration::BROWSER_HANDLER_COMMAND;
    use eclipse::apk::store::{CheckOutcome, Release, Store, UpdateCheck};
    use eclipse::apk::VersionCode;
    use eclipse::links::LaunchTarget;
    use eclipse::runtime::NativeLibRoot;
    use eclipse::status::StatusSink;
    use std::collections::BTreeMap;
    use std::ffi::OsString;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "eclipse-main-{tag}-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        root
    }

    const RAW_EXIT_CHILD: &str = "ECLIPSE_TEST_RAW_ANDROID_EXIT_CHILD";

    extern "C" fn abort_if_atexit_runs() {
        std::process::abort();
    }

    fn launch_command(
        arguments: &[&str],
        launch_link: Option<&str>,
    ) -> Result<Option<LaunchCommand>, LaunchCommandError> {
        let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
        LaunchCommand::parse(&arguments, launch_link.map(OsString::from))
    }

    fn link(link: &str) -> LaunchCommand {
        LaunchCommand::Link(eclipse::links::parse(link).unwrap())
    }

    #[test]
    fn run_accepts_at_most_one_path_argument() {
        assert_eq!(launch_command(&["run"], None), Ok(Some(LaunchCommand::Run)));
        assert_eq!(
            launch_command(&["run", "roblox.apk"], None),
            Ok(Some(LaunchCommand::RunFile("roblox.apk".into())))
        );
        assert_eq!(
            launch_command(&["run", "roblox.apk", "roblox://placeId=1"], None),
            Err(LaunchCommandError::Usage(RUN_USAGE))
        );
    }

    #[test]
    fn open_takes_exactly_one_link_or_place_id() {
        assert_eq!(
            launch_command(&["open", "1818"], None),
            Ok(Some(link("roblox://placeId=1818")))
        );
        assert_eq!(
            launch_command(
                &[
                    "open",
                    "https://www.roblox.com/games/1818/Classic-Crossroads"
                ],
                None
            ),
            Ok(Some(link("roblox://placeId=1818")))
        );
        let private = "roblox://placeId=1818&linkCode=0042";
        assert_eq!(
            launch_command(&["open", private], None),
            Ok(Some(link(private)))
        );
        for arguments in [&["open"][..], &["open", "1818", "1819"]] {
            assert_eq!(
                launch_command(arguments, None),
                Err(LaunchCommandError::Usage(OPEN_USAGE))
            );
        }
        assert!(OPEN_USAGE.starts_with("usage: eclipse open "));
        assert_eq!(
            launch_command(&["open", "https://www.roblox.com/catalog/1"], None),
            Err(LaunchCommandError::Link {
                context: "eclipse open",
                message: "This is a Roblox shop link; Eclipse opens experience links. Open it \
                          in a web browser."
                    .to_owned(),
            })
        );
    }

    #[test]
    fn the_internal_link_launch_takes_its_link_only_from_the_environment() {
        let place = "roblox://placeId=1818";
        assert_eq!(
            launch_command(&[LAUNCH_LINK_COMMAND], Some(place)),
            Ok(Some(link(place)))
        );
        for (arguments, launch_link) in [
            (&[LAUNCH_LINK_COMMAND][..], None),
            (&[LAUNCH_LINK_COMMAND, place], None),
            (&[LAUNCH_LINK_COMMAND, place], Some(place)),
        ] {
            let error = launch_command(arguments, launch_link).unwrap_err();
            let LaunchCommandError::Link { message, .. } = &error else {
                panic!("a bad internal launch is reported in a window: {error:?}");
            };
            assert!(message.contains(LAUNCH_LINK_ENV), "{message}");
        }
        assert_eq!(
            launch_command(&["run"], Some(place)),
            Ok(Some(LaunchCommand::Run))
        );
    }

    #[test]
    fn update_uses_apkcombo_unless_google_play_is_asked_for() {
        assert_eq!(parse_update_source(&[]).unwrap(), UpdateSource::ApkCombo);
        assert_eq!(
            parse_update_source(&["--play".into()]).unwrap(),
            UpdateSource::GooglePlay
        );
        for arguments in [vec!["play".into()], vec!["--play".into(), "--play".into()]] {
            assert_eq!(
                parse_update_source(&arguments).unwrap_err(),
                "usage: eclipse update [--play]"
            );
        }
    }

    #[test]
    fn native_libs_of_other_roblox_versions_are_removed() {
        let root = std::env::temp_dir().join(format!(
            "eclipse-native-lib-versions-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        for dir in ["3055", "3056", "3057", "custom"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("libroblox.so"), b"lib").unwrap();
        }
        std::fs::write(root.join("3054"), b"not a directory").unwrap();

        remove_other_native_lib_versions(&root, eclipse::apk::VersionCode(3056)).unwrap();

        let mut left: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["3054", "3056", "custom"]);
        std::fs::remove_dir_all(&root).ok();

        remove_other_native_lib_versions(&root, eclipse::apk::VersionCode(3056))
            .expect("a missing cache directory is not an error");

        std::fs::write(&root, b"not a directory").unwrap();
        let error = remove_other_native_lib_versions(&root, eclipse::apk::VersionCode(3056))
            .expect_err("a cache path that is a file cannot be listed");
        assert!(
            error.contains("cannot list") && error.contains(&root.display().to_string()),
            "{error}"
        );
        std::fs::remove_file(&root).ok();
    }

    #[test]
    fn art_code_of_other_roblox_versions_is_removed() {
        let cache = std::env::temp_dir().join(format!(
            "eclipse-art-cache-versions-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&cache).ok();
        std::fs::create_dir_all(&cache).unwrap();
        let names = [
            "x@data@eclipse@roblox@3056@base.apk@classes.dex",
            "x@data@eclipse@roblox@3056@base.apk@classes.vdex",
            "x@data@eclipse@roblox@3170@base.apk@classes.dex",
            "x@data@eclipse@roblox@3170@base.apk@classes.vdex",
            "x@data@eclipse@roblox@3056@other.apk@classes.dex",
            "x@data@eclipse@roblox@custom@base.apk@classes.dex",
            "x@data@eclipse@roblox-old@3056@base.apk@classes.dex",
            "app@lib@eclipse@framework@api-impl.jar@classes.dex",
            "home@u@Projects@verified@base.apk@classes.dex",
        ];
        for name in names {
            std::fs::write(cache.join(name), b"oat").unwrap();
        }
        let store = std::path::Path::new("/x/data/eclipse/roblox");
        let apk = std::path::Path::new("/x/data/eclipse/roblox/3170/base.apk");
        let keep = eclipse::apk::VersionCode(3170);

        remove_other_version_oats(&cache, store, apk, keep).unwrap();

        let mut left: Vec<String> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut expected: Vec<String> = names[2..].iter().map(|name| name.to_string()).collect();
        expected.sort();
        assert_eq!(left, expected);
        std::fs::remove_dir_all(&cache).ok();

        remove_other_version_oats(&cache, store, apk, keep)
            .expect("a missing ART cache is not an error");

        std::fs::write(&cache, b"not a directory").unwrap();
        let error = remove_other_version_oats(&cache, store, apk, keep)
            .expect_err("an ART cache path that is a file cannot be listed");
        assert!(
            error.contains("cannot list") && error.contains(&cache.display().to_string()),
            "{error}"
        );
        std::fs::remove_file(&cache).ok();
    }

    #[test]
    fn art_code_pruning_fails_when_the_apk_is_outside_its_store_version_directory() {
        let cache = std::path::Path::new("/nonexistent/eclipse-art-cache");
        let store = std::path::Path::new("/x/data/eclipse/roblox");
        let keep = eclipse::apk::VersionCode(3170);
        for apk in [
            "/x/data/eclipse/roblox/versions/3170/base.apk",
            "/x/data/eclipse/roblox/3056/base.apk",
            "/x/data/eclipse/other/3170/base.apk",
        ] {
            let error = remove_other_version_oats(cache, store, std::path::Path::new(apk), keep)
                .expect_err("an APK outside <store>/<version>/ breaks the cache pattern");
            assert!(
                error.contains(apk) && error.contains("version 3170 directory"),
                "{error}"
            );
        }
    }

    #[test]
    fn native_libs_in_a_chosen_directory_are_never_removed() {
        let root = temp_root("native-lib-override");
        for (dir, file) in [("2024", "notes.txt"), ("3055", "libroblox.so")] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join(file), b"kept").unwrap();
        }

        let dir = native_lib_dir(NativeLibRoot::Override(root.clone()), VersionCode(3056)).unwrap();
        assert_eq!(dir, root.join("3056"));
        assert!(root.join("2024/notes.txt").exists());
        assert!(root.join("3055/libroblox.so").exists());

        let dir = native_lib_dir(NativeLibRoot::Cache(root.clone()), VersionCode(3056)).unwrap();
        assert_eq!(dir, root.join("3056"));
        assert!(!root.join("2024").exists() && !root.join("3055").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    fn answer_one_request(
        socket: &std::path::Path,
        reply: &'static str,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        use std::io::{Read as _, Write as _};

        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
            request
        })
    }

    #[test]
    fn a_launch_that_cannot_hand_off_is_refused_while_the_first_one_runs() {
        let root = temp_root("client-lock");
        let Ok(RunStart::Locked(mut first)) = lock_run_in(&root, &Launch::File, None) else {
            panic!("the first client starts");
        };
        let first_log = first.log.head_path();
        assert_eq!(
            std::fs::canonicalize(root.join("logs").join("eclipse.log")).unwrap(),
            std::fs::canonicalize(&first_log).unwrap()
        );
        first
            .log
            .record(
                tracing::Level::INFO,
                "eclipse::status",
                "the first client runs",
            )
            .unwrap();
        first.log.flush().unwrap();
        let file = lock_run_in(&root, &Launch::File, None).err().unwrap();
        assert!(file.contains("already running"), "{file}");
        let place = Launch::Link(eclipse::links::parse("1818").unwrap());
        let link = lock_run_in(&root, &place, None).err().unwrap();
        assert!(link.contains("open the link again"), "{link}");
        let log = std::fs::read_to_string(&first_log).unwrap();
        assert!(
            log.contains("the first client runs"),
            "a refused launch leaves the running client's log alone: {log}"
        );
        assert_eq!(
            std::fs::canonicalize(root.join("logs").join("eclipse.log")).unwrap(),
            std::fs::canonicalize(&first_log).unwrap(),
            "a refused launch starts no run log"
        );
        drop(first);
        let concurrent_spawns_released_it = (0..100).find_map(|_| {
            match super::instance_control::lock_client(&root.join(super::RUNTIME_DIR)).unwrap() {
                ClientLock::Acquired(lock) => Some(lock),
                ClientLock::Held(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
            }
        });
        assert!(
            concurrent_spawns_released_it.is_some(),
            "a client starts once the first one exits"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_launch_that_loses_the_race_for_the_lock_hands_its_link_to_the_winner() {
        let root = super::instance_control::socket_test_root("hand-off");
        let runtime = root.join(super::RUNTIME_DIR);
        let ClientLock::Acquired(_running) =
            super::instance_control::lock_client(&runtime).unwrap()
        else {
            panic!("the running client holds the lock");
        };
        let socket = root.join("c.sock");
        let server = answer_one_request(&socket, r#""accepted""#);
        let access_code = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
        let link = format!("roblox://placeId=1818&accessCode={access_code}");
        let target = eclipse::links::parse(&link).unwrap();

        let started = lock_run_in(&root, &Launch::Link(target), Some(&socket));
        assert!(matches!(started, Ok(RunStart::HandedOff)));
        let request = server.join().unwrap();
        assert_eq!(
            request,
            [
                b"\x01{\"open\":{\"link\":\"".as_slice(),
                link.as_bytes(),
                b"\"}}"
            ]
            .concat()
        );
        assert!(
            !root.join("logs").exists(),
            "a handed-off launch writes no run log"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_launch_that_fails_its_setup_keeps_answering_later_launches() {
        let root = super::instance_control::socket_test_root("setup-failure");
        let socket = root.join("c.sock");
        let failure = super::start_client_run_in(
            Some(&socket),
            Err("the client-settings bridge did not load".to_owned()),
            &BTreeMap::new(),
        )
        .expect_err("setup fails without the client-settings bridge");
        assert_eq!(failure.error, "the client-settings bridge did not load");
        let listener = failure
            .listener
            .expect("the control socket is bound before any setup step can fail");
        let _later_launch = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        listener.accept().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn every_launch_but_a_file_run_shows_setup_failures_in_a_window() {
        let place = "roblox://placeId=1818";
        for (arguments, launch_link) in [
            (&["run"][..], None),
            (&["open", place], None),
            (&[BROWSER_HANDLER_COMMAND, place], None),
            (&[LAUNCH_LINK_COMMAND], Some(place)),
        ] {
            let launch = launch_command(arguments, launch_link).unwrap().unwrap();
            assert!(launch.launches_in_window(), "{arguments:?}");
        }
        let file_run = launch_command(&["run", "/home/u/roblox"], None);
        assert!(!file_run.unwrap().unwrap().launches_in_window());
        for arguments in [&["__webview-test"][..], &["install", "x"], &[]] {
            assert_eq!(launch_command(arguments, None), Ok(None), "{arguments:?}");
        }
    }

    #[test]
    fn a_failed_check_is_not_retried_for_thirty_minutes() {
        let root = temp_root("failed-check");
        let store = Store::at(root.clone());
        let rejected = Release {
            version_code: VersionCode(3171),
            base_sha1: [0x41; 20],
        };
        store
            .record_check(&UpdateCheck {
                at: std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 60 * 60),
                rejected: Some(rejected),
                outcome: CheckOutcome::Completed,
            })
            .unwrap();

        let mut attempts = 0;
        let failed = update_if_due(&store, Some(VersionCode(3170)), |previous| {
            attempts += 1;
            assert_eq!(previous, Some(rejected));
            Err("APKCombo is unreachable".into())
        })
        .err()
        .expect("the check fails");
        assert_eq!(failed.to_string(), "APKCombo is unreachable");
        let next_launch = update_if_due(&store, Some(VersionCode(3170)), |_| {
            attempts += 1;
            Ok(None)
        });
        assert!(matches!(next_launch, Ok(None)));
        assert_eq!(
            attempts, 1,
            "the failed check is not repeated at the next launch"
        );
        let recorded = store.last_check().unwrap().unwrap();
        assert_eq!(recorded.outcome, CheckOutcome::Failed);
        assert_eq!(
            recorded.rejected,
            Some(rejected),
            "a failed check keeps the release rejected before it"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_rejection_recorded_by_a_failed_check_is_kept() {
        let root = temp_root("rejected-check");
        let store = Store::at(root.clone());
        let rejected = Release {
            version_code: VersionCode(3171),
            base_sha1: [0x41; 20],
        };
        let recorded = UpdateCheck {
            at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
            rejected: Some(rejected),
            outcome: CheckOutcome::Completed,
        };
        let error = update_if_due(&store, Some(VersionCode(3170)), |_| {
            store.record_check(&recorded)?;
            Err("the download failed verification".into())
        })
        .err()
        .expect("the update failed");
        assert_eq!(error.to_string(), "the download failed verification");
        assert_eq!(store.last_check().unwrap(), Some(recorded));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_url_handler_notes_a_missing_or_unreadable_install() {
        let root = temp_root("handler-note");
        let store = Store::at(root.clone());
        assert_eq!(
            installed_client_note(&store).as_deref(),
            Some(NOT_INSTALLED)
        );
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("current.json"), b"not json").unwrap();
        let note = installed_client_note(&store).expect("a corrupt record is noted");
        assert!(
            note.contains("not a valid Eclipse install record"),
            "{note}"
        );
        std::fs::write(root.join("current.json"), b"{\"version_code\": 3170}").unwrap();
        assert_eq!(installed_client_note(&store), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn libroblox_init_takes_exactly_one_explicit_lib_dir() {
        let lib_dir = OsString::from("harness-libs");
        assert!(parse_libroblox_init_lib_dir(&[]).is_err());
        assert_eq!(
            parse_libroblox_init_lib_dir(std::slice::from_ref(&lib_dir)).unwrap(),
            std::path::Path::new("harness-libs")
        );
        assert!(parse_libroblox_init_lib_dir(&[lib_dir.clone(), lib_dir]).is_err());
    }

    #[test]
    fn url_handler_message_claims_an_install_only_when_one_was_written() {
        use super::desktop_integration::UrlHandlerInstall;

        let registered = url_handler_message(&UrlHandlerInstall::Registered {
            desktop_path: "/home/u/.local/share/applications/dev.eclipse.RobloxPlayer.desktop"
                .into(),
        });
        assert_eq!(
            registered,
            "Roblox browser Play handler installed: \
             /home/u/.local/share/applications/dev.eclipse.RobloxPlayer.desktop"
        );

        let flatpak = url_handler_message(&UrlHandlerInstall::FlatpakExport {
            app_id: "io.github.kuenec.Eclipse".to_owned(),
            desktop_path: "/app/share/applications/io.github.kuenec.Eclipse.UrlHandler.desktop"
                .into(),
        });
        assert!(flatpak.contains("nothing was written"), "{flatpak}");
        assert!(!flatpak.contains("handler installed"), "{flatpak}");
    }

    #[test]
    fn an_unreadable_install_record_is_reported_without_updating() {
        let root = std::env::temp_dir().join(format!(
            "eclipse-unreadable-install-record-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("current.json")).unwrap();
        let store = eclipse::apk::store::Store::at(root.clone());

        let mut updated = false;
        let error = installed_or_updated_set(&store, &StatusSink::terminal(), |_| {
            updated = true;
            Ok(None)
        })
        .err()
        .expect("an unreadable install record cannot be launched");
        assert!(!updated, "an unreadable install record is not replaced");
        assert!(
            matches!(
                error.downcast_ref(),
                Some(eclipse::apk::store::StoreError::Io { .. })
            ),
            "{error}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_first_download_is_reported_as_a_failed_download() {
        let root = std::env::temp_dir().join(format!(
            "eclipse-failed-first-download-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        let store = eclipse::apk::store::Store::at(root.clone());

        let error = installed_or_updated_set(&store, &StatusSink::terminal(), |current| {
            assert!(current.is_none(), "nothing is installed yet");
            Err("cannot load APKCombo's Roblox download page: timed out".into())
        })
        .err()
        .expect("nothing can be launched");
        assert_eq!(
            error.to_string(),
            "could not download Roblox: cannot load APKCombo's Roblox download page: timed out"
        );

        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("current.json"), b"{\"version_code\": 3056}").unwrap();
        let error = installed_or_updated_set(&store, &StatusSink::terminal(), |current| {
            assert!(current.is_none(), "the recorded install is missing");
            Err("APKCombo is unreachable".into())
        })
        .err()
        .expect("a missing install cannot be launched");
        let text = error.to_string();
        assert!(
            text.contains("is missing")
                && text.ends_with("downloading Roblox failed: APKCombo is unreachable"),
            "{text}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_update_launches_the_client_the_store_records() {
        let Some(paths) =
            eclipse::apk::ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "eclipse-failed-update-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        let store = eclipse::apk::store::Store::at(root.clone());
        let sources: Vec<std::path::PathBuf> = std::iter::once(paths.base)
            .chain(paths.native_split)
            .collect();

        let committed = installed_or_updated_set(&store, &StatusSink::terminal(), |current| {
            assert!(current.is_none(), "nothing is installed yet");
            store.install(&sources, &StatusSink::terminal())?;
            Err("recording the update check failed".into())
        })
        .expect("the update committed before failing is launched");
        let installed = store.current().unwrap().expect("the update was committed");
        assert_eq!(committed.version_code(), installed.version_code);
        drop(committed);

        let kept = installed_or_updated_set(&store, &StatusSink::terminal(), |current| {
            assert_eq!(
                current.map(eclipse::apk::ApkSet::version_code),
                Some(installed.version_code)
            );
            Err("Google Play is unreachable".into())
        })
        .expect("the verified install is launched");
        assert_eq!(kept.version_code(), installed.version_code);
        drop(kept);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn browser_ticket_is_replaced_before_android_startup() {
        let ticket = "SUPER_SECRET_TICKET_4f9d8c";
        let access_code = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
        let link_code = "98765";
        let click = format!(
            "roblox-player:1+launchmode:play+gameinfo:{ticket}+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestPrivateGame%26placeId%3D1818%26accessCode%3D{access_code}%26linkCode%3D{link_code}"
        );
        let handler = launch_command(&[BROWSER_HANDLER_COMMAND, &click], None)
            .unwrap()
            .unwrap();

        let mut restart = std::process::Command::new("eclipse");
        handler.restart(&mut restart);
        assert_eq!(
            restart.get_args().collect::<Vec<_>>(),
            [LAUNCH_LINK_COMMAND]
        );
        let environment: Vec<_> = restart.get_envs().collect();
        let [(name, Some(launch_link))] = environment[..] else {
            panic!("the restart passes exactly one link variable");
        };
        assert_eq!(name, LAUNCH_LINK_ENV);
        assert!(!launch_link.to_string_lossy().contains(ticket));
        let restarted = launch_command(&[LAUNCH_LINK_COMMAND], launch_link.to_str()).unwrap();
        assert_eq!(restarted.as_ref(), Some(&handler));

        let LaunchCommand::Link(target) = &handler else {
            panic!("a private-server click is a link launch");
        };
        assert!(matches!(target, LaunchTarget::PrivateServer { .. }));
        assert_eq!(target.to_string(), "place 1818 on a private server");
        for logged in [
            format!("{target:?}"),
            Launch::Link(target.clone()).already_running(std::path::Path::new("client.lock")),
        ] {
            for secret in [ticket, access_code, link_code] {
                assert!(!logged.contains(secret), "{logged}");
            }
        }
    }

    #[test]
    fn handler_starts_every_link_kind_and_never_echoes_codes_in_its_errors() {
        let ticket = "SUPER_SECRET_TICKET_4f9d8c";
        for accepted in [
            "roblox://placeId=1818&launchData=abc",
            "roblox://userId=261",
            "robloxmobile://placeId=1818",
            "roblox://placeId=1818&gameInstanceId=3a5e0cf4-3e23-46a0-9dc7-887dad37e760",
        ] {
            assert_eq!(
                launch_command(&[BROWSER_HANDLER_COMMAND, accepted], None),
                Ok(Some(link(accepted))),
                "{accepted}"
            );
        }
        let studio = format!("roblox-player:1+launchmode:edit+gameinfo:{ticket}");
        let error = launch_command(&[BROWSER_HANDLER_COMMAND, &studio], None).unwrap_err();
        assert_eq!(
            error,
            LaunchCommandError::Link {
                context: "eclipse browser launch",
                message: "This link is for Roblox Studio, which Eclipse does not run.".to_owned(),
            }
        );
        assert!(!format!("{error:?}").contains(ticket));
        assert!(matches!(
            launch_command(&[BROWSER_HANDLER_COMMAND], None),
            Err(LaunchCommandError::Link { .. })
        ));
    }

    #[test]
    fn arguments_that_are_not_utf8_are_parsed_without_panicking() {
        use std::os::unix::ffi::OsStringExt as _;

        let latin1 = OsString::from_vec(b"R\xf6blox.xapk".to_vec());
        assert_eq!(
            LaunchCommand::parse(&["run".into(), latin1.clone()], None),
            Ok(Some(LaunchCommand::RunFile(latin1.clone().into())))
        );
        for command in ["open", BROWSER_HANDLER_COMMAND] {
            let error = LaunchCommand::parse(&[command.into(), latin1.clone()], None)
                .expect_err("a Roblox URL is text");
            let LaunchCommandError::Link { message, .. } = &error else {
                panic!("a non-UTF-8 link is reported in a window: {error:?}");
            };
            assert!(message.contains("UTF-8"), "{message}");
        }
        assert_eq!(LaunchCommand::parse(&[latin1], None), Ok(None));
    }

    #[test]
    fn settings_shim_redirects_through_the_next_interposer() {
        const CHILD: &str = "ECLIPSE_TEST_SETTINGS_SHIM_CHILD";
        const FIXTURE_PATH: &std::ffi::CStr = c"/eclipse-fixture/next-interposer";
        const SETTINGS: &[u8] = b"{\"FFlagEclipseTest\":true}";

        if std::env::var_os(CHILD).is_some() {
            let android_path = std::ffi::CString::new(super::ANDROID_CLIENT_SETTINGS_PATH).unwrap();
            let mut status: libc::stat64 = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::stat64(android_path.as_ptr(), &mut status) },
                0,
                "stat64 of the Android settings path is redirected"
            );
            assert_eq!(status.st_uid, 4242, "stat64 reached the next interposer");
            assert_eq!(
                unsafe { libc::access(FIXTURE_PATH.as_ptr(), libc::F_OK) },
                0,
                "access reached the next interposer"
            );
            let fd = unsafe { libc::open(FIXTURE_PATH.as_ptr(), libc::O_RDONLY) };
            assert!(fd >= 0, "open reached the next interposer");
            unsafe { libc::close(fd) };
            assert_eq!(
                std::fs::read(super::ANDROID_CLIENT_SETTINGS_PATH).unwrap(),
                SETTINGS
            );
            super::verify_client_settings_redirect()
                .expect("the restarted process sees the redirected settings");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "eclipse settings shim-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(super::CLIENT_SETTINGS_PATH_SHIM_NAME),
            super::CLIENT_SETTINGS_PATH_SHIM,
        )
        .unwrap();
        let settings = dir.join("ClientAppSettings.json");
        std::fs::write(&settings, SETTINGS).unwrap();

        let output = std::process::Command::new(
            std::env::current_exe().expect("the test harness executable must have a path"),
        )
        .args([
            "--exact",
            "tests::settings_shim_redirects_through_the_next_interposer",
        ])
        .env(CHILD, "1")
        .env(super::CLIENT_SETTINGS_PATH_ENV, &settings)
        .env(
            "LD_LIBRARY_PATH",
            super::prepend_search_list_entry(dir.as_os_str(), std::env::var_os("LD_LIBRARY_PATH")),
        )
        .env(
            "LD_PRELOAD",
            super::prepend_search_list_entry(
                std::ffi::OsStr::new(super::CLIENT_SETTINGS_PATH_SHIM_NAME),
                Some(env!("ECLIPSE_NEXT_INTERPOSER_FIXTURE_SO").into()),
            ),
        )
        .output()
        .expect("the preloaded child must start");
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            output.status.success(),
            "the preloaded child failed: status={:?}, stdout={}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn concurrent_launches_stage_complete_client_settings() {
        const ROUNDS: usize = 1000;
        let root = temp_root("settings-staging");
        std::fs::create_dir_all(&root).unwrap();
        let settings = root.join(super::CLIENT_SETTINGS_FILE);
        let fflags = ["FFlagEclipseFirst", "FFlagEclipseSecond"]
            .map(|flag| BTreeMap::from([(flag.to_owned(), serde_json::Value::Bool(true))]));
        let contents = fflags
            .clone()
            .map(|fflags| super::client_app_settings_json(&fflags));
        std::thread::scope(|scope| {
            let launches = fflags.map(|fflags| {
                let (settings, contents) = (&settings, &contents);
                scope.spawn(move || {
                    for _ in 0..ROUNDS {
                        super::write_client_settings(settings, &fflags)?;
                        let staged = std::fs::read(settings).map_err(|error| error.to_string())?;
                        if !contents.contains(&staged) {
                            return Err(format!(
                                "a launch read incomplete settings {:?}",
                                String::from_utf8_lossy(&staged)
                            ));
                        }
                    }
                    Ok(())
                })
            });
            for launch in launches {
                launch
                    .join()
                    .unwrap()
                    .expect("every launch stages its settings");
            }
        });
        std::fs::remove_dir_all(&root).ok();
    }

    fn client_app_settings(fflags: &BTreeMap<String, serde_json::Value>) -> serde_json::Value {
        serde_json::from_slice(&super::client_app_settings_json(fflags)).unwrap()
    }

    #[test]
    fn client_app_settings_enable_only_the_maximum_frame_rate_row_by_default() {
        assert_eq!(
            client_app_settings(&BTreeMap::new()),
            serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "True"})
        );
    }

    #[test]
    fn client_app_settings_add_the_user_fflags_to_the_default() {
        let fflags = BTreeMap::from([(
            "DFIntExample".to_owned(),
            serde_json::Value::Number(42.into()),
        )]);
        assert_eq!(
            client_app_settings(&fflags),
            serde_json::json!({
                "DFIntExample": 42,
                "FFlagGameBasicSettingsFramerateCap5": "True",
            })
        );
    }

    #[test]
    fn user_fflags_override_the_default_flag() {
        let fflags = BTreeMap::from([(
            "FFlagGameBasicSettingsFramerateCap5".to_owned(),
            serde_json::Value::from("False"),
        )]);
        assert_eq!(
            client_app_settings(&fflags),
            serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "False"})
        );
    }

    #[test]
    fn android_process_exit_skips_unsafe_foreign_atexit_handlers() {
        if std::env::var_os(RAW_EXIT_CHILD).is_some() {
            let registered = unsafe { libc::atexit(abort_if_atexit_runs) };
            assert_eq!(registered, 0, "the child must register its atexit sentinel");
            let discarded = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap();
            finish_android_process(ClientEnd::Played, ExitRecord(discarded));
        }

        let output = std::process::Command::new(
            std::env::current_exe().expect("the test harness executable must have a path"),
        )
        .args([
            "--exact",
            "tests::android_process_exit_skips_unsafe_foreign_atexit_handlers",
        ])
        .env(RAW_EXIT_CHILD, "1")
        .output()
        .expect("the raw-exit child must start");

        assert!(
            output.status.success(),
            "the raw-exit child ran an atexit handler: status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
