use std::process::ExitCode;

mod browser_launch;
mod desktop_integration;

const CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV: &str = "ECLIPSE_CLIENT_SETTINGS_REDIRECT_ACTIVE";
const CLIENT_SETTINGS_PATH_ENV: &str = "ECLIPSE_CLIENT_APP_SETTINGS_PATH";
const CLIENT_SETTINGS_PATH_SHIM: &[u8] =
    include_bytes!(env!("ECLIPSE_CLIENT_SETTINGS_PATH_SHIM_SO"));

const HELP: &str = "\
eclipse — run the Android Roblox build on Linux (open-source, Rust)

USAGE:
    eclipse <COMMAND>

COMMANDS:
    run [APK]  Parse the APK, boot the ART VM (Roblox on the classpath), open the window.
               With no APK and `auto_fetch_missing`+`apk_url` set (or ECLIPSE_APK_URL),
               auto-downloads from your configured source first.
    install-url-handler [APK]
               Register Eclipse for browser Play clicks and remember the Roblox APK.
    fetch      Report the latest upstream Roblox version + download the APK from your
               configured source (config `apk_url` / ECLIPSE_APK_URL) into the cache.
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

NOTE: Eclipse never hosts or hard-codes a Roblox APK source. You supply your own APK (path
    or a download URL you configure); auto-fetch is opt-in. Eclipse does not redistribute Roblox.

STATUS:
    `run` parses the manifest, prints the ART boot plan, boots the vendored ART VM with
    Roblox's Java on the classpath, then opens the host game window (winit, no GTK). The
    framework that drives the launcher Activity to onCreate and renders the engine into the
    window is the next phase (component-map F). See docs/.
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
    if is_android_run_command(args.first().map(String::as_str))
        && std::env::var_os(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV).is_none()
    {
        if let Err(error) = install_client_settings_and_reexec(&args) {
            eprintln!("eclipse Android settings setup: {error}");
            return ExitCode::FAILURE;
        }
        unreachable!("a successful settings-path handoff replaces this process");
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
            let status = match parse_run_apk_path(&args[1..])
                .and_then(|apk_path| run_apk(apk_path, None).map_err(|error| error.to_string()))
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
        Some("config") => match show_config() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse config: {e}");
                ExitCode::FAILURE
            }
        },
        Some("fetch") => match fetch_apk_command() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse fetch: {e}");
                ExitCode::FAILURE
            }
        },

        Some("__run-libroblox-init") => match eclipse::loader::init_run::run_libroblox_init() {
            Ok(completed) => {
                println!("__run-libroblox-init: {completed} constructor(s) completed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__run-libroblox-init: {e}");
                ExitCode::FAILURE
            }
        },

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
        return Err("the roblox-player handler requires exactly one URL".to_string());
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

fn install_client_settings_and_reexec(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt as _;

    let config = eclipse::config::Config::load()?;
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
    let mut json = serde_json::to_vec_pretty(&config.roblox_client_app_settings())?;
    json.push(b'\n');
    std::fs::write(&temporary_path, json)?;
    std::fs::rename(&temporary_path, &settings_path)?;

    let shim_path = runtime_dir.join("libeclipse_client_settings_path.so");
    let shim_is_current =
        std::fs::read(&shim_path).is_ok_and(|bytes| bytes.as_slice() == CLIENT_SETTINGS_PATH_SHIM);
    if !shim_is_current {
        let temporary_shim = runtime_dir.join(format!(
            ".libeclipse_client_settings_path.so.{}.tmp",
            std::process::id()
        ));
        std::fs::write(&temporary_shim, CLIENT_SETTINGS_PATH_SHIM)?;
        std::fs::rename(temporary_shim, &shim_path)?;
    }

    let settings_path = settings_path.canonicalize()?;
    let shim_path = shim_path.canonicalize()?;

    println!(
        "# Roblox Fast Flags staged at {} (Android /data/local/tmp/ClientAppSettings.json)",
        settings_path.display()
    );

    let preload = match std::env::var_os("LD_PRELOAD") {
        Some(existing) if !existing.is_empty() => {
            let mut value = shim_path.as_os_str().to_os_string();
            value.push(":");
            value.push(existing);
            value
        }
        _ => shim_path.as_os_str().to_os_string(),
    };
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let current_exe = std::env::current_exe()?;
    let error = std::process::Command::new(current_exe)
        .args(args)
        .env(CLIENT_SETTINGS_REDIRECT_ACTIVE_ENV, "1")
        .env(CLIENT_SETTINGS_PATH_ENV, &settings_path)
        .env("LD_PRELOAD", preload)
        .exec();
    Err(
        format!("could not restart Eclipse with the Android client-settings path bridge: {error}")
            .into(),
    )
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

fn configured_apk_url(config: &eclipse::config::Config) -> Option<String> {
    std::env::var("ECLIPSE_APK_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.apk_url.clone())
}

fn fetch_apk_command() -> Result<(), Box<dyn std::error::Error>> {
    match eclipse::apk::fetch::latest_roblox_version() {
        Ok(v) => {
            let android_major = v.split('.').nth(1).unwrap_or("?");
            println!(
                "# Latest upstream Roblox version (oracle): {v}  (≈ Android 2.{android_major}.x)"
            );
        }
        Err(e) => eprintln!("# version oracle unavailable (non-fatal): {e}"),
    }
    let config = eclipse::config::Config::load()?;
    let url = configured_apk_url(&config).ok_or(
        "no APK source configured — set config `apk_url` or ECLIPSE_APK_URL (Eclipse never hard-codes one)",
    )?;
    println!("# Fetching APK from your configured source: {url}");
    let path = eclipse::apk::fetch::fetch_apk(&url, config.apk_sha256.as_deref())?;
    println!("fetched APK: {} ✓", path.display());
    Ok(())
}

fn parse_run_apk_path(arguments: &[String]) -> Result<Option<&str>, String> {
    match arguments {
        [] => Ok(None),
        [apk_path] => Ok(Some(apk_path)),
        _ => Err("usage: eclipse run [APK]".to_string()),
    }
}

fn last_apk_path_file() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    Ok(eclipse::config::Config::config_path()?.with_file_name("last-apk.json"))
}

fn remember_apk_path(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let path = path.canonicalize()?;
    if !path.is_file() {
        return Err(format!("Roblox APK is not a file: {}", path.display()).into());
    }
    let setting = last_apk_path_file()?;
    let parent = setting
        .parent()
        .ok_or("the remembered APK setting has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".last-apk.{}.tmp", std::process::id()));
    let text = path
        .to_str()
        .ok_or("the Roblox APK path is not valid UTF-8")?;
    std::fs::write(&temporary, serde_json::to_vec(text)?)?;
    std::fs::rename(&temporary, &setting)?;
    Ok(path)
}

fn remembered_apk_path() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let setting = last_apk_path_file()?;
    let bytes = std::fs::read(&setting).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            std::io::Error::new(
                error.kind(),
                "no Roblox APK is remembered; run `eclipse install-url-handler /path/to/roblox.apk`",
            )
        } else {
            error
        }
    })?;
    let text: String = serde_json::from_slice(&bytes)?;
    let path = std::path::PathBuf::from(text);
    if !path.is_file() {
        return Err(format!(
            "the remembered Roblox APK no longer exists: {}",
            path.display()
        )
        .into());
    }
    Ok(path)
}

fn install_url_handler_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let apk_path = match arguments {
        [] => remembered_apk_path()?,
        [apk_path] => {
            let mut apk = eclipse::apk::Apk::open(std::path::Path::new(apk_path))?;
            apk.manifest()?;
            remember_apk_path(std::path::Path::new(apk_path))?
        }
        _ => return Err("usage: eclipse install-url-handler [APK]".into()),
    };
    let desktop_path = desktop_integration::install_url_handler()?;
    println!(
        "Roblox browser Play handler installed: {} (APK: {}) ✓",
        desktop_path.display(),
        apk_path.display()
    );
    Ok(())
}

fn run_apk(
    apk_path: Option<&str>,
    browser_place_id: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let resolved: String = match apk_path {
        Some(p) => p.to_string(),
        None if browser_place_id.is_some() => remembered_apk_path()?.to_string_lossy().into_owned(),
        None => {
            let config = eclipse::config::Config::load()?;
            let env_url = std::env::var_os("ECLIPSE_APK_URL").is_some();
            match configured_apk_url(&config) {
                Some(url) if config.auto_fetch_missing || env_url => {
                    println!("# No APK supplied — auto-fetching from your configured source: {url}");
                    let path = eclipse::apk::fetch::fetch_apk(&url, config.apk_sha256.as_deref())?;
                    println!("fetched APK: {} ✓", path.display());
                    path.to_string_lossy().into_owned()
                }
                _ => {
                    return Err("missing APK path (usage: eclipse run <APK>); or set config `apk_url` + \
                                `auto_fetch_missing` (or ECLIPSE_APK_URL) to auto-download — `eclipse fetch`"
                        .into())
                }
            }
        }
    };
    let apk_path = resolved.as_str();

    let mut apk = eclipse::apk::Apk::open(std::path::Path::new(apk_path))?;

    eclipse::loader::ndk_registry::set_apk_path(std::path::PathBuf::from(apk_path));
    let manifest = apk.manifest()?;
    remember_apk_path(std::path::Path::new(apk_path))?;
    let config = eclipse::config::Config::load()?;
    let has_native_engine = apk
        .native_abis()
        .iter()
        .any(|abi| abi.name == TARGET_ABI && abi.has_engine);
    if has_native_engine {
        eclipse::performance::configure_engine_cpu_affinity(config.graphics_optimization_mode);
    }
    let plan = eclipse::runtime::BootPlan::new(&manifest, &config);

    println!("# ART boot plan (dry run) for {apk_path}");
    println!("package:            {}", manifest.package);
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
    println!("# dex2oat options (-> dex2oat AOT compiler):");
    for opt in plan.dex2oat_options() {
        println!("    {opt}");
    }

    let app_lib_dir = eclipse::runtime::native_lib_cache_dir()?;
    println!(
        "\n# Extracting native libs (lib/x86_64/) to {}…",
        app_lib_dir.display()
    );
    let extracted = apk.extract_native_libs("x86_64", &app_lib_dir)?;
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
    let asset_count = apk.extract_assets(&assets_dir)?;
    println!("extracted {asset_count} asset file(s) ✓");

    let cursor_asset_count = eclipse::system_cursor::install(&assets_dir, config.touch_mode)?;
    if config.touch_mode == eclipse::config::TouchMode::Off {
        println!(
            "desktop system cursor active (suppressed {cursor_asset_count} extracted Roblox cursor texture(s)) ✓"
        );
    }

    println!("\n# Booting the ART VM with Roblox on the classpath…");

    let vm = eclipse::runtime::boot(
        &plan,
        Some(std::path::Path::new(apk_path)),
        Some(&app_lib_dir),
    )?;
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

    let _preloaded_libs =
        preload_app_native_libs(&mut apk, std::path::Path::new(apk_path), &app_lib_dir, &vm)?;

    println!("# Driving the framework lifecycle (JNI; steps 1–7 to Activity.onResume / RESUMED)…");
    let android_deep_link = browser_place_id.map(|place_id| format!("roblox://placeId={place_id}"));
    let progress = eclipse::framework::drive_application_lifecycle(
        &vm,
        apk_path,
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

const TARGET_ABI: &str = "x86_64";
const ENGINE_FILENAME: &str = "libroblox.so";

fn preload_app_native_libs(
    apk: &mut eclipse::apk::Apk,
    apk_path: &std::path::Path,
    app_lib_dir: &std::path::Path,
    vm: &eclipse::runtime::Vm,
) -> Result<Vec<eclipse::loader::engine::PreloadedLib>, Box<dyn std::error::Error>> {
    let has_engine = apk
        .native_abis()
        .iter()
        .any(|abi| abi.name == TARGET_ABI && abi.has_engine);
    if !has_engine {
        println!("# No lib/x86_64/libroblox.so in APK — skipping the Rust engine loader (framework-only path).");
        return Ok(Vec::new());
    }

    let mut log = std::io::stdout();
    let vm_raw = vm.as_raw();
    let mut loaded: Vec<eclipse::loader::engine::PreloadedLib> = Vec::new();

    println!("# Pre-loading the native engine via Eclipse's Rust loader (NOT the apkenv linker)…");
    let engine = eclipse::loader::engine::load_app_native_lib(
        apk_path,
        ENGINE_FILENAME,
        vm_raw,
        app_lib_dir,
        &mut log,
    )?
    .ok_or("libroblox.so unexpectedly deduped on first load")?;
    report_preloaded(&engine);
    loaded.push(engine);

    let filenames = apk.native_lib_filenames(TARGET_ABI);
    println!(
        "# Pre-loading {} other x86_64 JNI lib(s) via the Rust loader (tolerant of per-lib failure)…",
        filenames.iter().filter(|f| *f != ENGINE_FILENAME).count()
    );
    for filename in &filenames {
        if filename == ENGINE_FILENAME {
            continue;
        }
        match eclipse::loader::engine::load_app_native_lib(
            apk_path,
            filename,
            vm_raw,
            app_lib_dir,
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

    let apk_path = eclipse::loader::init_run::find_roblox_apk().ok_or(
        "no Roblox APK (set ECLIPSE_ROBLOX_APK or place it at the default dev-host path) — \
         __webview-test boots ART with the installed framework on the classpath",
    )?;
    println!(
        "# __webview-test: booting ART from {} (framework classpath; no libroblox preload, \
         no lifecycle, no window)…",
        apk_path.display()
    );
    let mut apk = eclipse::apk::Apk::open(&apk_path)?;
    let manifest = apk.manifest()?;
    let config = eclipse::config::Config::load()?;
    let plan = eclipse::runtime::BootPlan::new(&manifest, &config);
    let vm = eclipse::runtime::boot(&plan, Some(apk_path.as_path()), None)?;

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
    if !(ua.contains("Eclipse-WebView") && ua.contains("Chrome/149"))
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
    use super::{finish_android_process, normalize_browser_launch, parse_run_apk_path};

    const RAW_EXIT_CHILD: &str = "ECLIPSE_TEST_RAW_ANDROID_EXIT_CHILD";

    extern "C" fn abort_if_atexit_runs() {
        std::process::abort();
    }

    #[test]
    fn run_accepts_at_most_one_apk_argument() {
        let apk = "roblox.apk".to_string();
        assert_eq!(parse_run_apk_path(&[]).unwrap(), None);
        assert_eq!(
            parse_run_apk_path(std::slice::from_ref(&apk)).unwrap(),
            Some("roblox.apk")
        );
        assert!(parse_run_apk_path(&[apk, "roblox://placeId=1".into()]).is_err());
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
