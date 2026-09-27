use std::process::ExitCode;

mod browser_launch;
mod desktop_integration;

const CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV: &str = "ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE";
const CLIENT_SETTINGS_PATH_ENV: &str = "ECLIPSE_CLIENT_APP_SETTINGS_PATH";
const ANDROID_CLIENT_SETTINGS_PATH: &str = "/data/local/tmp/ClientAppSettings.json";
const CLIENT_SETTINGS_PATH_SHIM_NAME: &str = "libeclipse_client_settings_path.so";
const CLIENT_SETTINGS_PATH_SHIM: &[u8] =
    include_bytes!(env!("ECLIPSE_CLIENT_SETTINGS_PATH_SHIM_SO"));

const HELP: &str = "\
eclipse — run the Android Roblox build on Linux (open-source, Rust)

USAGE:
    eclipse <COMMAND>

COMMANDS:
    run [PATH]  Verify the Roblox client, boot the ART VM (Roblox on the classpath) and open
                the window. With no PATH, runs the installed client: the first run downloads
                Roblox, and later runs check for a Roblox update at most every 6 hours. PATH
                may be an APK file or a directory holding base.apk and split_config.x86_64.apk.
    install <PATH>...
                Verify and install the Roblox client: base.apk plus split_config.x86_64.apk,
                a directory holding them, or an .apks/.xapk/.apkm bundle.
    update [--play]
                Download and install the newest Roblox client from APKCombo, without any
                account. With --play, download it from Google Play with the account saved by
                play-login instead.
    play-login  Sign in to Google Play with your own Google account (once, for update --play).
    install-url-handler
                Register Eclipse for browser Play clicks (they run the installed client).
    config      Show effective configuration and its path
    help        Show this help
    --version   Show version

NOTE: Eclipse runs only the official, unmodified Roblox client signed by Roblox Corporation.
    It never hosts or modifies it. `eclipse update` downloads Roblox's own release files from
    APKCombo (or Google Play with --play) and installs them only when Roblox's signature
    verifies; anything else is discarded.
";

fn main() -> ExitCode {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let args = match normalize_browser_launch(raw_args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("eclipse browser launch: {error}");
            return ExitCode::FAILURE;
        }
    };
    if is_android_run_command(args.first().map(String::as_str)) {
        let settings = match std::env::var_os(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV) {
            None => install_client_settings_and_reexec(&args),
            Some(_) => verify_client_settings_redirect().map_err(Into::into),
        };
        if let Err(error) = settings {
            eprintln!("eclipse Android settings setup: {error}");
            return ExitCode::FAILURE;
        }
    }
    if is_android_run_command(args.first().map(String::as_str))
        || matches!(args.first().map(String::as_str), Some("__webview-test"))
    {
        if let Err(error) = eclipse::runtime::prepare_art_boot_environment() {
            eprintln!("eclipse ART startup: {error}");
            return ExitCode::FAILURE;
        }
    }

    eclipse::diagnostics::init();

    tracing::debug!(
        version = eclipse::VERSION,
        command = args.first(),
        "eclipse starting"
    );
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("eclipse {}", eclipse::VERSION);
            ExitCode::SUCCESS
        }
        Some("run") => {
            let status = match parse_run_path(&args[1..])
                .and_then(|path| run_apk(path, None).map_err(|error| error.to_string()))
            {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("eclipse run: {e}");
                    1
                }
            };
            finish_android_process(status)
        }
        Some("__run-browser-place") => {
            let status = match parse_internal_place_id(&args[1..]).and_then(|place_id| {
                run_apk(None, Some(place_id)).map_err(|error| error.to_string())
            }) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("eclipse browser launch: {error}");
                    1
                }
            };
            finish_android_process(status)
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
        Some("config") => match show_config() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse config: {e}");
                ExitCode::FAILURE
            }
        },

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

fn is_android_run_command(command: Option<&str>) -> bool {
    matches!(command, Some("run") | Some("__run-browser-place"))
}

fn normalize_browser_launch(mut arguments: Vec<String>) -> Result<Vec<String>, String> {
    if !matches!(
        arguments.first().map(String::as_str),
        Some(desktop_integration::BROWSER_HANDLER_COMMAND)
    ) {
        return Ok(arguments);
    }
    if arguments.len() != 2 {
        return Err("the Roblox URL handler requires exactly one URL".to_string());
    }

    let place_id = browser_launch::place_id(&arguments[1]).map_err(|error| error.to_string())?;
    arguments.clear();
    arguments.push("__run-browser-place".to_string());
    arguments.push(place_id.to_string());
    Ok(arguments)
}

fn parse_internal_place_id(arguments: &[String]) -> Result<u64, String> {
    let [place_id] = arguments else {
        return Err("invalid internal browser launch request".to_string());
    };
    let place_id = place_id
        .parse::<u64>()
        .map_err(|_| "invalid internal browser launch request".to_string())?;
    if place_id == 0 {
        return Err("invalid internal browser launch request".to_string());
    }
    Ok(place_id)
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

fn install_client_settings_and_reexec(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::CommandExt as _;

    let json = eclipse::config::Config::load()?.client_app_settings_json()?;
    let app_data_dir = eclipse::framework::app_data_dir().ok_or(
        "cannot resolve Eclipse's app-data directory; set HOME, XDG_DATA_HOME, or ECLIPSE_APP_DATA_DIR",
    )?;
    let runtime_dir = app_data_dir.join("runtime");
    std::fs::create_dir_all(&runtime_dir)?;

    let settings_path = runtime_dir.join("ClientAppSettings.json");
    let temporary_path = runtime_dir.join(format!(
        ".ClientAppSettings.json.{}.tmp",
        std::process::id()
    ));
    std::fs::write(&temporary_path, json)?;
    std::fs::rename(&temporary_path, &settings_path)?;

    let shim_path = runtime_dir.join(CLIENT_SETTINGS_PATH_SHIM_NAME);
    let shim_is_current =
        std::fs::read(&shim_path).is_ok_and(|bytes| bytes.as_slice() == CLIENT_SETTINGS_PATH_SHIM);
    if !shim_is_current {
        let temporary_shim = runtime_dir.join(format!(
            ".{CLIENT_SETTINGS_PATH_SHIM_NAME}.{}.tmp",
            std::process::id()
        ));
        std::fs::write(&temporary_shim, CLIENT_SETTINGS_PATH_SHIM)?;
        std::fs::rename(temporary_shim, &shim_path)?;
    }

    let settings_path = settings_path.canonicalize()?;
    let runtime_dir = runtime_dir.canonicalize()?;
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
        )
        .into());
    }

    println!(
        "# Roblox Fast Flags staged at {} (Android {ANDROID_CLIENT_SETTINGS_PATH})",
        settings_path.display()
    );

    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let current_exe = std::env::current_exe()?;
    let error = std::process::Command::new(current_exe)
        .args(args)
        .env(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV, "1")
        .env(CLIENT_SETTINGS_PATH_ENV, &settings_path)
        .env(
            "LD_LIBRARY_PATH",
            prepend_search_list_entry(runtime_dir.as_os_str(), std::env::var_os("LD_LIBRARY_PATH")),
        )
        .env(
            "LD_PRELOAD",
            prepend_search_list_entry(
                std::ffi::OsStr::new(CLIENT_SETTINGS_PATH_SHIM_NAME),
                std::env::var_os("LD_PRELOAD"),
            ),
        )
        .exec();
    Err(
        format!("could not restart Eclipse with the Android client-settings path bridge: {error}")
            .into(),
    )
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

fn finish_android_process(status: libc::c_int) -> ! {
    use std::io::Write as _;

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    unsafe { libc::_exit(status) }
}

fn show_config() -> Result<(), eclipse::config::ConfigError> {
    let path = eclipse::config::Config::config_path()?;
    let config = eclipse::config::Config::load()?;
    println!("# {}", path.display());
    println!("{}", config.to_json_pretty()?);
    Ok(())
}

const NOT_INSTALLED: &str = "Roblox is not installed; run `eclipse update` to download it, or \
     install the APKs with `eclipse install <PATH>`";

const VERIFYING_SIGNATURE: &str = "# Verifying the Roblox client's signature…";

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

fn parse_run_path(arguments: &[String]) -> Result<Option<&std::path::Path>, String> {
    match arguments {
        [] => Ok(None),
        [path] => Ok(Some(std::path::Path::new(path))),
        _ => Err("usage: eclipse run [APK | DIRECTORY]".to_string()),
    }
}

fn install_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if arguments.is_empty() {
        return Err("usage: eclipse install <APK | DIRECTORY | BUNDLE>...".into());
    }
    let sources: Vec<std::path::PathBuf> = arguments.iter().map(std::path::PathBuf::from).collect();
    println!("# Verifying and installing the Roblox client…");
    let installed = eclipse::apk::store::Store::open()?.install(&sources)?;
    println!("installed Roblox {installed}");
    Ok(())
}

fn play_login_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
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

fn parse_libroblox_init_lib_dir(arguments: &[String]) -> Result<&std::path::Path, String> {
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

fn parse_update_source(arguments: &[String]) -> Result<UpdateSource, String> {
    match arguments {
        [] => Ok(UpdateSource::ApkCombo),
        [flag] if flag == "--play" => Ok(UpdateSource::GooglePlay),
        _ => Err("usage: eclipse update [--play]".to_string()),
    }
}

fn update_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let source = parse_update_source(arguments)?;
    let store = eclipse::apk::store::Store::open()?;
    let current = store.usable_current()?;
    match source {
        UpdateSource::ApkCombo => update_from_apkcombo(&store, current.as_ref(), None),
        UpdateSource::GooglePlay => update_from_play(&store, current.as_ref()),
    }
    .map(drop)
}

fn update_from_apkcombo(
    store: &eclipse::apk::store::Store,
    current: Option<&eclipse::apk::ApkSet>,
    rejected: Option<eclipse::apk::store::Release>,
) -> Result<Option<eclipse::apk::ApkSet>, Box<dyn std::error::Error>> {
    println!("# Checking APKCombo for the newest Roblox client…");
    let offer = eclipse::apk::apkcombo::newest_offer()?;
    println!("# APKCombo offers {offer}");
    let outcome = eclipse::apk::apkcombo::update(&offer, store, current, rejected)?;
    finish_update(store, outcome)
}

fn update_from_play(
    store: &eclipse::apk::store::Store,
    current: Option<&eclipse::apk::ApkSet>,
) -> Result<Option<eclipse::apk::ApkSet>, Box<dyn std::error::Error>> {
    let credentials = eclipse::apk::play::Account::open()?
        .credentials()?
        .ok_or("not signed in to Google Play; run `eclipse play-login` first")?;
    println!("# Checking Google Play for the newest Roblox client…");
    let outcome = eclipse::apk::play::update(&credentials, store, current)?;
    finish_update(store, outcome)
}

fn finish_update(
    store: &eclipse::apk::store::Store,
    outcome: eclipse::apk::store::UpdateOutcome,
) -> Result<Option<eclipse::apk::ApkSet>, Box<dyn std::error::Error>> {
    use eclipse::apk::store::{InstalledVersion, UpdateCheck, UpdateOutcome};

    store.record_check(&UpdateCheck {
        at: std::time::SystemTime::now(),
        rejected: None,
    })?;
    match outcome {
        UpdateOutcome::UpToDate { installed } => {
            println!("Roblox {installed} is up to date");
            Ok(None)
        }
        UpdateOutcome::Updated {
            previous: Some(previous),
            set,
        } => {
            println!(
                "updated Roblox from {previous} to {}",
                InstalledVersion::from(&*set)
            );
            Ok(Some(*set))
        }
        UpdateOutcome::Updated {
            previous: None,
            set,
        } => {
            println!("installed Roblox {}", InstalledVersion::from(&*set));
            Ok(Some(*set))
        }
    }
}

fn update_if_due(
    store: &eclipse::apk::store::Store,
    current: Option<&eclipse::apk::ApkSet>,
) -> Result<Option<eclipse::apk::ApkSet>, Box<dyn std::error::Error>> {
    let last_check = store.last_check()?;
    let due = eclipse::apk::store::update_due(
        current.map(eclipse::apk::ApkSet::version_code),
        last_check.map(|check| check.at),
        std::time::SystemTime::now(),
    );
    if !due {
        return Ok(None);
    }
    update_from_apkcombo(store, current, last_check.and_then(|check| check.rejected))
}

fn installed_apk_set(
    check_for_update: bool,
) -> Result<eclipse::apk::ApkSet, Box<dyn std::error::Error>> {
    let store = eclipse::apk::store::Store::open()?;
    println!("{VERIFYING_SIGNATURE}");
    let set = if check_for_update {
        installed_or_updated_set(&store, |current| update_if_due(&store, current))?
    } else {
        store.verified_current()?.ok_or(NOT_INSTALLED)?
    };
    if let Some(cache) = eclipse::runtime::dalvik_cache_dir() {
        remove_other_version_oats(&cache, store.root(), set.base_path(), set.version_code())?;
    }
    println!(
        "# Launching the installed Roblox {}",
        eclipse::apk::store::InstalledVersion::from(&set)
    );
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
    store: &eclipse::apk::store::Store,
    update: impl FnOnce(
        Option<&eclipse::apk::ApkSet>,
    ) -> Result<Option<eclipse::apk::ApkSet>, Box<dyn std::error::Error>>,
) -> Result<eclipse::apk::ApkSet, Box<dyn std::error::Error>> {
    let current = match store.verified_current() {
        Err(error) if !error.is_unusable_install() => return Err(error.into()),
        current => current,
    };
    let verified = current.as_ref().ok().and_then(Option::as_ref);
    let verified_version = verified.map(eclipse::apk::ApkSet::version_code);
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
            eprintln!("# WARNING: could not update Roblox: {error}");
            Ok(installed)
        }
        Ok(None) => Err(format!("could not download Roblox: {error}").into()),
        Err(install) if install.is_unusable_install() => {
            Err(format!("{install}, and downloading Roblox failed: {error}").into())
        }
        Err(install) => Err(install.into()),
    }
}

fn install_url_handler_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if !arguments.is_empty() {
        return Err("usage: eclipse install-url-handler".into());
    }
    let outcome = desktop_integration::install_url_handler()?;
    println!("{}", url_handler_message(&outcome));
    if eclipse::apk::store::Store::open()?.current()?.is_none() {
        println!("note: {NOT_INSTALLED}");
    }
    Ok(())
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

fn native_lib_dir(
    version: eclipse::apk::VersionCode,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let root = eclipse::runtime::native_lib_cache_dir()?;
    remove_other_native_lib_versions(&root, version)?;
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

fn run_apk(
    path: Option<&std::path::Path>,
    browser_place_id: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut apks = match path {
        Some(path) => {
            let paths = eclipse::apk::ApkSetPaths::locate(path)?;
            println!("{VERIFYING_SIGNATURE}");
            eclipse::apk::ApkSet::open(paths)?
        }
        None => installed_apk_set(browser_place_id.is_none())?,
    };
    let base_path = apks.base_path().to_path_buf();
    let apk_path = base_path
        .to_str()
        .ok_or("the Roblox APK path is not valid UTF-8")?
        .to_owned();

    eclipse::loader::ndk_registry::set_apk_path(base_path.clone());
    let manifest = apks.manifest().clone();
    let config = eclipse::config::Config::load()?;
    eclipse::performance::configure_engine_cpu_affinity(config.graphics_optimization_mode);
    let plan = eclipse::runtime::BootPlan::new(&manifest, &config);

    println!("# ART boot plan (dry run) for {apk_path}");
    println!("package:            {}", manifest.package);
    println!(
        "version:            {} (versionCode {})",
        apks.version_name().unwrap_or("unnamed"),
        apks.version_code()
    );
    println!("launcher_activity:  {}", plan.launcher_activity);
    println!("sdk_int:            {}", plan.sdk_int);
    println!(
        "heap:               {} MiB (DisableHSpaceCompactForOOM={})",
        plan.heap_mib, plan.disable_hspace_compact
    );
    println!("graphics_backend:   {}", plan.graphics_backend.as_str());
    println!("instruction_set:    {}", plan.instruction_set_features);

    println!("\n# VM options (-> JNI_CreateJavaVM):");
    for opt in plan.vm_options() {
        println!("    {opt}");
    }

    let app_lib_dir = native_lib_dir(apks.version_code())?;
    println!(
        "\n# Extracting native libs (lib/x86_64/) to {}…",
        app_lib_dir.display()
    );
    let extracted = apks
        .native_libs_mut()
        .extract_native_libs(eclipse::apk::TARGET_ABI, &app_lib_dir)?;
    println!("extracted {} native lib(s) ✓", extracted.len());

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
    println!(
        "\n# Extracting Roblox bundled assets (assets/ → files/assets/) to {}…",
        assets_dir.display()
    );
    let asset_count = apks.base_mut().extract_assets(&assets_dir)?;
    println!("extracted {asset_count} asset file(s) ✓");

    println!("\n# Booting the ART VM with Roblox on the classpath…");

    let vm = eclipse::runtime::boot(&plan, Some(&base_path), Some(&app_lib_dir))?;
    println!("ART VM booted with Roblox's Java on the classpath ✓");

    println!("# Provisioning bionic sonames (libm.so → Eclipse apkenv-loadable shim) …");
    eclipse::runtime::provision_bionic_sonames(&app_lib_dir)?;
    println!("bionic sonames provisioned (Eclipse libm shim) ✓");

    let fw = eclipse::runtime::find_framework()?;
    println!("# Whitelisting the app-lib dir in the bionic linker search path…");
    eclipse::runtime::whitelist_bionic_library_path(&fw, Some(&app_lib_dir))?;
    println!("bionic linker search path whitelisted (dl_parse_library_path) ✓");

    println!("# Registering engine-JNI_OnLoad-reachable framework natives (Log + Process)…");
    eclipse::framework::register_engine_preload_natives(&vm)?;
    println!("engine-preload framework natives registered ✓");

    let _preloaded_libs = preload_app_native_libs(apks.native_libs(), &app_lib_dir, &vm)?;

    println!("# Driving the framework lifecycle (JNI; steps 1–7 to Activity.onResume / RESUMED)…");
    let android_deep_link = browser_place_id.map(|place_id| format!("roblox://placeId={place_id}"));
    let progress = eclipse::framework::drive_application_lifecycle(
        &vm,
        &apk_path,
        apks.signing_certificate_history(),
        &plan.launcher_activity,
        android_deep_link.as_deref(),
    )?;
    let activity_target = if browser_place_id.is_some() {
        "resolved ACTION_VIEW activity"
    } else {
        plan.launcher_activity.as_str()
    };
    println!("framework lifecycle driven: {progress:?} (non-GTK Context/Window/View natives bound; launcher Activity = {activity_target}) ✓");

    if std::env::var("ECLIPSE_WEB_LOGIN").is_ok_and(|value| value == "1") {
        println!("# Opening Roblox's official web login in Eclipse…");
        let handle = eclipse::framework::drive_roblox_web_login(&vm)?;
        println!("official Roblox web login opened (WebView handle {handle}) ✓");
    }

    println!("# Opening the host window (winit; close it to exit)…");
    eclipse::graphics::run_windowed(
        &format!("Eclipse — {}", manifest.package),
        Some(&vm),
        config.touch_mode,
    )?;
    Ok(())
}

fn preload_app_native_libs(
    apk: &eclipse::apk::Apk,
    app_lib_dir: &std::path::Path,
    vm: &eclipse::runtime::Vm,
) -> Result<Vec<eclipse::loader::engine::PreloadedLib>, Box<dyn std::error::Error>> {
    use eclipse::apk::{ENGINE_LIB, TARGET_ABI};

    let mut log = std::io::stdout();
    let java_vm = unsafe { jni::vm::JavaVM::from_raw(vm.as_raw()) };
    let mut loaded: Vec<eclipse::loader::engine::PreloadedLib> = Vec::new();

    println!("# Pre-loading the native engine via Eclipse's Rust loader (NOT the apkenv linker)…");
    let engine =
        eclipse::loader::engine::load_app_native_lib(app_lib_dir, ENGINE_LIB, &java_vm, &mut log)?
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
        match eclipse::loader::engine::load_app_native_lib(
            app_lib_dir,
            filename,
            &java_vm,
            &mut log,
        ) {
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
    upcalls_ok: u32,
    started_ms: u128,
    finished_ms: u128,
    http: i32,
    frame_w: u32,
    frame_h: u32,
    distinct: usize,
}

impl std::fmt::Display for WebViewTestReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WebView engine pipeline OK: internalLoadChanged upcalls {}/2 (state 0 @ {}ms, \
             state 3 @ {}ms, http {}), frame {}x{} {} distinct pixels, bridge round-trip OK, \
             evaluateJavascript OK, honest UA OK, cookie set/get OK, cookie callback OK, \
             cookie flush OK, \
             ViewClosed, helper exit 0, bound=5",
            self.upcalls_ok,
            self.started_ms,
            self.finished_ms,
            self.http,
            self.frame_w,
            self.frame_h,
            self.distinct
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

fn start_loopback_page() -> std::io::Result<u16> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, body): (&str, &str) = if path == "/" || path.starts_with("/?") {
                ("200 OK", WEBVIEW_TEST_PAGE)
            } else {
                ("404 Not Found", "")
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Ok(port)
}

fn pump_tick(vm: &eclipse::runtime::Vm, ms: u64) {
    if let Err(e) = eclipse::framework::pump_main_looper(vm) {
        eprintln!("# main Looper pump failed: {e}");
    }
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

fn run_webview_test() -> Result<WebViewTestReport, Box<dyn std::error::Error>> {
    use eclipse::framework;
    use eclipse::webview::client;
    use std::time::{Duration, Instant};

    const START_DEADLINE: Duration = Duration::from_secs(30);
    const FINISH_DEADLINE: Duration = Duration::from_secs(90);
    const UPCALL_DEADLINE: Duration = Duration::from_secs(10);
    const INK_DEADLINE: Duration = Duration::from_secs(20);
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
                        CEF helper needs a Wayland or X11 session (its own select_ozone would \
                        refuse with the same error)"
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
    let config = eclipse::config::Config::load()?;
    let plan = eclipse::runtime::BootPlan::new(apks.manifest(), &config);
    let vm = eclipse::runtime::boot(&plan, Some(apks.base_path()), None)?;

    eclipse::framework::register_engine_preload_natives(&vm)?;

    eclipse::framework::prepare_main_looper(&vm)?;
    println!("# ART booted ✓ — driving the WebView smoke (register → alloc → setWebViewClient → addJavascriptInterface → loadUrl)…");
    let handle = eclipse::framework::drive_webview_smoke(&vm, &target_url)?;

    let fail_reason =
        || client::failed_reason().map(|r| format!("web engine helper unavailable: {r}"));
    let start = Instant::now();
    let mut started_ms: Option<u128> = None;
    let (finished_ms, http) = loop {
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
            if let Some(http) = obs.finished_http {
                println!(
                    "# load-state 3 observed @ {} ms http={http}",
                    start.elapsed().as_millis()
                );
                break (start.elapsed().as_millis(), http);
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
    let upcalls_ok = loop {
        let ok = client::load_observed(handle)
            .map(|o| o.upcalls_ok)
            .unwrap_or(0);
        if ok >= 2 {
            break ok;
        }
        if Instant::now() > upcall_deadline {
            return Err(format!(
                "only {ok}/2 internalLoadChanged upcalls completed within 10 s of load-finish"
            )
            .into());
        }
        pump_tick(&vm, 50);
    };

    let ink_deadline = Instant::now() + INK_DEADLINE;
    let (frame_w, frame_h, distinct) = loop {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        let census = client::with_latest_frame(handle, |stage| {
            let mut distinct = std::collections::HashSet::new();
            for px in stage.bytes.as_chunks::<4>().0 {
                distinct.insert(u32::from_ne_bytes([px[0], px[1], px[2], px[3]]));
            }
            (stage.width, stage.height, distinct.len())
        });
        if let Some((w, h, count)) = census {
            if count > 1 {
                println!("# staged frame {w}x{h} distinct_pixels={count}");
                break (w, h, count);
            }
        }
        if Instant::now() > ink_deadline {
            return Err("no staged frame with nonzero ink within 20 s of load-finish".into());
        }
        pump_tick(&vm, 50);
    };

    let eval_and_wait = |script: &str| -> Option<String> {
        if framework::webview_evaluate(&vm, handle, script).is_err() {
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

    let ua = eval_and_wait("navigator.userAgent")
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
        if let Some(r) = eval_and_wait("window.__eclipseBridgeResult||''") {
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

    if std::env::var("ECLIPSE_WEBVIEW_EXPECT_PERSISTED_TEST_COOKIE").as_deref() == Ok("1") {
        let restored = framework::cookie_manager_get_cookie(&vm, &target_url);
        if !restored.contains("ECLIPSE_TEST=1") {
            return Err("persistent-cookie probe did not restore ECLIPSE_TEST before this process's setCookie".into());
        }
        println!("# persisted cookie restored OK (value not printed)");
    }
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
    println!("# cookie flush OK (CEF persistent-store completion boundary returned)");

    client::close_view(handle).map_err(|e| format!("CloseView send failed: {e}"))?;
    let close_deadline = Instant::now() + CLOSE_DEADLINE;
    while client::view_is_tracked(handle) {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        if Instant::now() > close_deadline {
            return Err("ViewClosed not observed within 15 s".into());
        }
        pump_tick(&vm, 50);
    }
    println!("# view-closed ✓ — shutting the helper down…");
    let report = client::shutdown(&vm, Duration::from_secs(15));
    if report.helper_exit != Some(0) {
        return Err(format!(
            "helper exit status {:?} (expected 0; reader_joined={})",
            report.helper_exit, report.reader_joined
        )
        .into());
    }
    Ok(WebViewTestReport {
        upcalls_ok,
        started_ms,
        finished_ms,
        http,
        frame_w,
        frame_h,
        distinct,
    })
}

fn report_preloaded(lib: &eclipse::loader::engine::PreloadedLib) {
    let ctors = if lib.constructors_run > 0 {
        format!("{} ctor(s)", lib.constructors_run)
    } else {
        "no ctors".to_string()
    };
    let onload = match lib.jni_onload_version {
        Some(v) if v < 0 => format!("JNI_OnLoad error {v:#x}"),
        Some(v) => format!("JNI_OnLoad → {v:#x}"),
        None => "lazy natives (no JNI_OnLoad)".to_string(),
    };
    println!("  {} ✓ ({ctors}; {onload})", lib.soname);
}

#[cfg(test)]
mod tests {
    use super::{
        finish_android_process, installed_or_updated_set, normalize_browser_launch,
        parse_libroblox_init_lib_dir, parse_run_path, parse_update_source,
        remove_other_native_lib_versions, remove_other_version_oats, url_handler_message,
        UpdateSource,
    };

    const RAW_EXIT_CHILD: &str = "ECLIPSE_TEST_RAW_ANDROID_EXIT_CHILD";

    extern "C" fn abort_if_atexit_runs() {
        std::process::abort();
    }

    #[test]
    fn run_accepts_at_most_one_path_argument() {
        let apk = "roblox.apk".to_string();
        assert_eq!(parse_run_path(&[]).unwrap(), None);
        assert_eq!(
            parse_run_path(std::slice::from_ref(&apk)).unwrap(),
            Some(std::path::Path::new("roblox.apk"))
        );
        assert!(parse_run_path(&[apk, "roblox://placeId=1".into()]).is_err());
    }

    #[test]
    fn update_uses_apkcombo_unless_google_play_is_asked_for() {
        assert_eq!(parse_update_source(&[]).unwrap(), UpdateSource::ApkCombo);
        assert_eq!(
            parse_update_source(&["--play".to_string()]).unwrap(),
            UpdateSource::GooglePlay
        );
        for arguments in [
            vec!["play".to_string()],
            vec!["--play".to_string(), "--play".to_string()],
        ] {
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
    fn libroblox_init_takes_exactly_one_explicit_lib_dir() {
        let lib_dir = "harness-libs".to_string();
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
        let error = installed_or_updated_set(&store, |_| {
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

        let error = installed_or_updated_set(&store, |current| {
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
        let error = installed_or_updated_set(&store, |current| {
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

        let committed = installed_or_updated_set(&store, |current| {
            assert!(current.is_none(), "nothing is installed yet");
            store.install(&sources)?;
            Err("recording the update check failed".into())
        })
        .expect("the update committed before failing is launched");
        let installed = store.current().unwrap().expect("the update was committed");
        assert_eq!(committed.version_code(), installed.version_code);
        drop(committed);

        let kept = installed_or_updated_set(&store, |current| {
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
        let secret = "SUPER_SECRET_TICKET_4f9d8c";
        let protocol = format!(
            "roblox-player:1+launchmode:play+gameinfo:{secret}+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestGame%26placeId%3D90441122676618"
        );
        let normalized =
            normalize_browser_launch(vec!["__handle-roblox-player-url".to_string(), protocol])
                .unwrap();
        assert_eq!(normalized, ["__run-browser-place", "90441122676618"]);
        assert!(!normalized.iter().any(|argument| argument.contains(secret)));
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
    fn android_process_exit_skips_unsafe_foreign_atexit_handlers() {
        if std::env::var_os(RAW_EXIT_CHILD).is_some() {
            let registered = unsafe { libc::atexit(abort_if_atexit_runs) };
            assert_eq!(registered, 0, "the child must register its atexit sentinel");
            finish_android_process(0);
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
