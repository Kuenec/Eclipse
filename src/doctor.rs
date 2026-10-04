use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use eclipse::apk::store::{
    CheckOutcome, InstalledVersion, LeftBecause, Store, StoreError, StoredVersion, UpdateCheck,
};
use eclipse::apk::VersionCode;
use eclipse::gamepad::DeviceAccess;
use eclipse::runtime::RuntimeError;
use eclipse::storage::{Category, Starts};
use eclipse_config::edit::EditError;
use eclipse_config::Config;
use rustix::fs::StatVfsMountFlags;

use crate::desktop_integration::{self, HostHandler, Packaging};
use crate::instance_control::RUNTIME_DIR;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const LOW_SPACE: u64 = GIB;
const SHORT_COMMIT: usize = 12;
const OS_RELEASES: [&str; 2] = ["/run/host/os-release", "/etc/os-release"];
const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
const CPU_INFO: &str = "/proc/cpuinfo";
const MEM_INFO: &str = "/proc/meminfo";
const SEARCH_PATH_SEPARATORS: &[u8] = b":;";
const FFLAGS_KEY: &str = "fflags";
const SHOWN_FLAG_NAMES: usize = 40;
const NO_APP_DATA_DIR: &str =
    "cannot resolve Eclipse's app-data directory; set HOME, XDG_DATA_HOME, or ECLIPSE_APP_DATA_DIR";

type Fact<T> = Result<T, String>;

pub(crate) struct Doctor {
    version: &'static str,
    install: Fact<Install>,
    system: System,
    cpu: Cpu,
    memory: Fact<Memory>,
    graphics: Graphics,
    storage: Storage,
    roblox: Roblox,
    config: ConfigFile,
    url_handler: Fact<UrlHandler>,
    logs: Fact<Logs>,
}

enum Install {
    Flatpak(FlatpakInstall),
    Host { executable: PathBuf },
}

struct FlatpakInstall {
    app_id: String,
    branch: Option<String>,
    arch: Option<String>,
    commit: Option<String>,
    runtime: Option<String>,
    runtime_commit: Option<String>,
    flatpak_version: Option<String>,
}

impl FlatpakInstall {
    fn parse(info: &str) -> io::Result<Self> {
        let value = |group, key| eclipse::flatpak::info_value(info, group, key).map(str::to_owned);
        let commit =
            |key| value("Instance", key).map(|commit| commit.chars().take(SHORT_COMMIT).collect());
        Ok(Self {
            app_id: desktop_integration::flatpak_app_id(info)?.to_owned(),
            branch: value("Instance", "branch"),
            arch: value("Instance", "arch"),
            commit: commit("app-commit"),
            runtime: value("Application", "runtime"),
            runtime_commit: commit("runtime-commit"),
            flatpak_version: value("Instance", "flatpak-version"),
        })
    }
}

struct System {
    os: Fact<String>,
    kernel: Fact<String>,
    desktop: Option<String>,
    session: Option<String>,
    windows: WindowSystem,
    controllers: DeviceAccess,
    sdl: Fact<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowSystem {
    Wayland,
    X11,
    XWayland,
    Missing,
}

impl WindowSystem {
    fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name| var(name).is_some_and(|value| !value.is_empty());
        if set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET") {
            Self::Wayland
        } else if !set("DISPLAY") {
            Self::Missing
        } else if var("XDG_SESSION_TYPE").as_deref() == Some("wayland") {
            Self::XWayland
        } else {
            Self::X11
        }
    }
}

impl fmt::Display for WindowSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wayland => "Wayland",
            Self::X11 => "X11",
            Self::XWayland => "X11 through XWayland",
            Self::Missing => "none, because neither WAYLAND_DISPLAY nor DISPLAY is set",
        })
    }
}

struct Cpu {
    info: Fact<CpuInfo>,
    features: String,
    baseline: Result<(), RuntimeError>,
}

#[derive(Debug, PartialEq, Eq)]
struct CpuInfo {
    model: Option<String>,
    logical: usize,
}

fn parse_cpu_info(text: &str) -> CpuInfo {
    let mut model = None;
    let mut logical = 0;
    for (key, value) in text.lines().filter_map(|line| line.split_once(':')) {
        match key.trim() {
            "processor" => logical += 1,
            "model name" if model.is_none() => model = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    CpuInfo { model, logical }
}

#[derive(Debug, PartialEq, Eq)]
struct Memory {
    total: u64,
    available: Option<u64>,
}

fn parse_mem_info(text: &str) -> Option<Memory> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let kib = line.strip_prefix(name)?.strip_prefix(':')?;
            Some(kib.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()? * KIB)
        })
    };
    Some(Memory {
        total: field("MemTotal")?,
        available: field("MemAvailable"),
    })
}

fn parse_os_release(text: &str) -> Option<String> {
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))?
        .trim();
    let value = ['"', '\'']
        .into_iter()
        .find_map(|quote| value.strip_prefix(quote)?.strip_suffix(quote))
        .unwrap_or(value);
    Some(value.replace("\\\"", "\"")).filter(|value| !value.is_empty())
}

struct Graphics {
    section: String,
    problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeDirProblem {
    SearchPathSeparator(PathBuf),
    NoExec(PathBuf),
}

impl fmt::Display for RuntimeDirProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SearchPathSeparator(dir) => write!(
                f,
                "the Android client-settings bridge directory {} contains a colon or semicolon, \
                 which LD_LIBRARY_PATH cannot carry; set ECLIPSE_APP_DATA_DIR to a directory \
                 without colons or semicolons",
                dir.display()
            ),
            Self::NoExec(dir) => write!(
                f,
                "the Android client-settings bridge directory {} is on a noexec mount, so the \
                 bridge cannot load from it; keep Eclipse's app-data directory off noexec \
                 mounts or set ECLIPSE_APP_DATA_DIR to one that allows executable files",
                dir.display()
            ),
        }
    }
}

pub(crate) fn runtime_dir_problem(dir: &Path) -> io::Result<Option<RuntimeDirProblem>> {
    let mount = filesystem_of(dir)?;
    Ok(runtime_dir_problem_on(dir, mount.f_flag))
}

fn runtime_dir_problem_on(dir: &Path, flags: StatVfsMountFlags) -> Option<RuntimeDirProblem> {
    if dir
        .as_os_str()
        .as_bytes()
        .iter()
        .any(|byte| SEARCH_PATH_SEPARATORS.contains(byte))
    {
        return Some(RuntimeDirProblem::SearchPathSeparator(dir.to_owned()));
    }
    flags
        .contains(StatVfsMountFlags::NOEXEC)
        .then(|| RuntimeDirProblem::NoExec(dir.to_owned()))
}

fn filesystem_of(path: &Path) -> io::Result<rustix::fs::StatVfs> {
    let existing = path
        .ancestors()
        .map(|ancestor| {
            if ancestor.as_os_str().is_empty() {
                Path::new(".")
            } else {
                ancestor
            }
        })
        .find(|ancestor| ancestor.exists())
        .unwrap_or(Path::new("/"));
    rustix::fs::statvfs(existing).map_err(|error| {
        io::Error::new(
            io::Error::from(error).kind(),
            format!(
                "cannot inspect the filesystem of {}: {error}",
                existing.display()
            ),
        )
    })
}

struct Storage {
    runtime_dir: Fact<Option<RuntimeDirProblem>>,
    places: Vec<Place>,
}

struct Place {
    kind: PlaceKind,
    path: Fact<PathBuf>,
    free: Fact<u64>,
}

impl Place {
    fn at(kind: PlaceKind, path: Fact<PathBuf>) -> Self {
        let free = match &path {
            Ok(path) => filesystem_of(path)
                .map(|mount| mount.f_bavail.saturating_mul(mount.f_frsize))
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        };
        Self { kind, path, free }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaceKind {
    AppData,
    RobloxStore,
    Cache,
}

impl fmt::Display for PlaceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AppData => "App data",
            Self::RobloxStore => "Roblox store",
            Self::Cache => "Cache",
        })
    }
}

struct Roblox {
    installed: Fact<Option<InstalledVersion>>,
    stored: Fact<Vec<StoredVersion>>,
    last_check: Fact<Option<UpdateCheck>>,
    skipped: Fact<Vec<(VersionCode, LeftBecause)>>,
    play_sign_in: Fact<PlaySignIn>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaySignIn {
    Saved,
    NotSaved,
}

struct ConfigFile {
    path: Option<PathBuf>,
    problems: Vec<String>,
    repair: ConfigRepair,
    settings: Config,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigRepair {
    InSettings,
    ByHand,
    WhereManaged,
}

impl ConfigRepair {
    fn of(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::ByHand;
        };
        match eclipse_config::edit::check(path) {
            Ok(()) => Self::InSettings,
            Err(EditError::Link { .. } | EditError::ReadOnly { .. }) => Self::WhereManaged,
            Err(
                EditError::Invalid(_) | EditError::ChangedWhileSaving { .. } | EditError::Io { .. },
            ) => Self::ByHand,
        }
    }
}

enum UrlHandler {
    Exported(PathBuf),
    NotExported(PathBuf),
    Host(HostHandler),
}

struct Logs {
    dir: PathBuf,
    runs: usize,
    bytes: u64,
    newest: Option<NewestRun>,
}

struct NewestRun {
    head: PathBuf,
    outcome: Fact<Option<String>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Problem {
    CpuBelowBaseline(&'static str),
    Graphics(String),
    RuntimeDir(RuntimeDirProblem),
    LowSpace {
        path: PathBuf,
        free: u64,
    },
    InvalidConfig {
        repair: ConfigRepair,
        packaging: Packaging,
    },
    NotInstalled {
        packaging: Packaging,
    },
    LastCheckFailed {
        packaging: Packaging,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CpuBelowBaseline(feature) => write!(
                f,
                "This CPU lacks {feature}. Roblox's Android x86-64 client needs SSSE3, SSE4.1, \
                 SSE4.2 and POPCNT, so it cannot run on this computer."
            ),
            Self::Graphics(problem) => f.write_str(problem),
            Self::RuntimeDir(problem) => {
                let text = problem.to_string();
                let mut characters = text.chars();
                if let Some(first) = characters.next() {
                    write!(f, "{}{}.", first.to_uppercase(), characters.as_str())?;
                }
                Ok(())
            }
            Self::LowSpace { path, free } => write!(
                f,
                "Only {} is free where Roblox is stored ({}). A Roblox update downloads about \
                 234 MiB and unpacks more, so free at least 1 GiB.",
                size_text(*free),
                path.display()
            ),
            Self::InvalidConfig { repair, packaging } => {
                let config_set = packaging.command("config set");
                f.write_str("The settings file has problems, listed under Config. ")?;
                match repair {
                    ConfigRepair::InSettings => write!(
                        f,
                        "Eclipse uses the defaults for those settings until they are fixed in \
                         Settings or with `{config_set}`."
                    ),
                    ConfigRepair::ByHand => write!(
                        f,
                        "Settings and `{config_set}` cannot change the file until it is fixed \
                         by hand where each problem points; until then Eclipse uses the \
                         defaults they name."
                    ),
                    ConfigRepair::WhereManaged => f.write_str(
                        "The file is managed outside Eclipse, so fix them where it is managed; \
                         until then Eclipse uses the defaults they name.",
                    ),
                }
            }
            Self::NotInstalled { packaging } => write!(
                f,
                "Roblox is not installed. Start Eclipse to download it, or run `{}`.",
                packaging.command("update")
            ),
            Self::LastCheckFailed { packaging } => write!(
                f,
                "The last check for a Roblox update failed. The log of that launch says why; \
                 `{}` tries again.",
                packaging.command("update")
            ),
        }
    }
}

fn flag_names(fflags: &BTreeMap<String, serde_json::Value>) -> String {
    let count = fflags.len();
    let names: Vec<&String> = fflags.keys().take(SHOWN_FLAG_NAMES).collect();
    let names = serde_json::to_string(&names).expect("flag names have a JSON form");
    match count {
        0 => "none".to_owned(),
        1 => format!("1 flag, its name only: {names}"),
        count if count <= SHOWN_FLAG_NAMES => format!("{count} flags, names only: {names}"),
        count => format!("{count} flags, the first {SHOWN_FLAG_NAMES} names only: {names}"),
    }
}

fn size_text(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn read_fact(path: &str) -> Fact<String> {
    std::fs::read_to_string(path).map_err(|error| format!("cannot read {path}: {error}"))
}

impl Doctor {
    pub(crate) fn examine() -> Self {
        let install = examine_install();
        let app_data = eclipse::framework::app_data_dir().ok_or_else(|| NO_APP_DATA_DIR.to_owned());
        let store = Store::open().map_err(|error| error.to_string());
        let loaded = eclipse_config::load();
        let graphics = eclipse::gpu::diagnosis::diagnose(&loaded.config);
        let config_repair = ConfigRepair::of(loaded.path.as_deref());
        let url_handler = match &install {
            Ok(Install::Flatpak(flatpak)) => {
                let path = desktop_integration::flatpak_url_handler_path(&flatpak.app_id);
                Ok(if path.is_file() {
                    UrlHandler::Exported(path)
                } else {
                    UrlHandler::NotExported(path)
                })
            }
            Ok(Install::Host { .. }) => desktop_integration::host_url_handler()
                .map(UrlHandler::Host)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        };
        Self {
            version: eclipse::VERSION,
            install,
            system: System {
                os: OS_RELEASES
                    .into_iter()
                    .find_map(|path| std::fs::read_to_string(path).ok())
                    .and_then(|text| parse_os_release(&text))
                    .ok_or_else(|| {
                        "neither /run/host/os-release nor /etc/os-release names it".to_owned()
                    }),
                kernel: read_fact(KERNEL_RELEASE).map(|release| release.trim().to_owned()),
                desktop: non_empty_var("XDG_CURRENT_DESKTOP"),
                session: non_empty_var("XDG_SESSION_TYPE"),
                windows: WindowSystem::from_env(|name| std::env::var(name).ok()),
                controllers: eclipse::gamepad::device_access(),
                sdl: eclipse::gamepad::sdl_version().map(|version| version.to_string()),
            },
            cpu: Cpu {
                info: read_fact(CPU_INFO).map(|text| parse_cpu_info(&text)),
                features: eclipse::runtime::instruction_set_features(),
                baseline: eclipse::runtime::android_cpu_baseline(),
            },
            memory: read_fact(MEM_INFO).and_then(|text| {
                parse_mem_info(&text).ok_or_else(|| format!("{MEM_INFO} names no MemTotal"))
            }),
            graphics: Graphics {
                section: graphics.to_string(),
                problems: graphics.problems(),
            },
            storage: examine_storage(app_data.clone(), &store),
            roblox: examine_roblox(&store),
            config: ConfigFile {
                repair: config_repair,
                problems: loaded.problems.iter().map(ToString::to_string).collect(),
                path: loaded.path,
                settings: loaded.config,
            },
            url_handler,
            logs: app_data
                .and_then(|app_data| examine_logs(&eclipse::diagnostics::log_dir(&app_data))),
        }
    }

    pub(crate) fn log_dir(&self) -> Result<&Path, &str> {
        match &self.logs {
            Ok(logs) => Ok(&logs.dir),
            Err(error) => Err(error),
        }
    }

    fn packaging(&self) -> Packaging {
        match &self.install {
            Ok(Install::Flatpak(flatpak)) => Packaging::Flatpak {
                app_id: flatpak.app_id.clone(),
            },
            Ok(Install::Host { .. }) | Err(_) => Packaging::Host,
        }
    }

    pub(crate) fn fixes(&self) -> Vec<String> {
        self.problems().iter().map(ToString::to_string).collect()
    }

    fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        if let Err(RuntimeError::CpuLacksFeature(feature)) = self.cpu.baseline {
            problems.push(Problem::CpuBelowBaseline(feature));
        }
        problems.extend(
            self.graphics
                .problems
                .iter()
                .cloned()
                .map(Problem::Graphics),
        );
        if let Ok(Some(problem)) = &self.storage.runtime_dir {
            problems.push(Problem::RuntimeDir(problem.clone()));
        }
        problems.extend(self.storage.places.iter().find_map(|place| match place {
            Place {
                kind: PlaceKind::RobloxStore,
                path: Ok(path),
                free: Ok(free),
            } if *free < LOW_SPACE => Some(Problem::LowSpace {
                path: path.clone(),
                free: *free,
            }),
            _ => None,
        }));
        if !self.config.problems.is_empty() {
            problems.push(Problem::InvalidConfig {
                repair: self.config.repair,
                packaging: self.packaging(),
            });
        }
        if let Ok(None) = self.roblox.installed {
            problems.push(Problem::NotInstalled {
                packaging: self.packaging(),
            });
        }
        if let Ok(Some(UpdateCheck {
            outcome: CheckOutcome::Failed,
            ..
        })) = self.roblox.last_check
        {
            problems.push(Problem::LastCheckFailed {
                packaging: self.packaging(),
            });
        }
        problems
    }
}

fn examine_install() -> Fact<Install> {
    match desktop_integration::flatpak_info().map_err(|error| error.to_string())? {
        Some(info) => FlatpakInstall::parse(&info)
            .map(Install::Flatpak)
            .map_err(|error| error.to_string()),
        None => std::env::current_exe()
            .map(|executable| Install::Host { executable })
            .map_err(|error| format!("cannot locate the Eclipse executable: {error}")),
    }
}

fn examine_storage(app_data: Fact<PathBuf>, store: &Fact<Store>) -> Storage {
    let runtime_dir = app_data.clone().and_then(|app_data| {
        let dir = app_data.join(RUNTIME_DIR);
        let dir = dir.canonicalize().unwrap_or(dir);
        runtime_dir_problem(&dir).map_err(|error| error.to_string())
    });
    let cache = eclipse::runtime::client_cache_dir()
        .map_err(|error| error.to_string())
        .map(|dir| dir.path().parent().unwrap_or(dir.path()).to_owned());
    let store = match store {
        Ok(store) => Ok(store.root().to_owned()),
        Err(error) => Err(error.clone()),
    };
    Storage {
        runtime_dir,
        places: vec![
            Place::at(PlaceKind::AppData, app_data),
            Place::at(PlaceKind::RobloxStore, store),
            Place::at(PlaceKind::Cache, cache),
        ],
    }
}

fn store_fact<T>(
    store: &Fact<Store>,
    read: impl FnOnce(&Store) -> Result<T, StoreError>,
) -> Fact<T> {
    match store {
        Ok(store) => read(store).map_err(|error| error.to_string()),
        Err(error) => Err(error.clone()),
    }
}

fn examine_roblox(store: &Fact<Store>) -> Roblox {
    Roblox {
        installed: store_fact(store, Store::current),
        stored: store_fact(store, Store::versions),
        last_check: store_fact(store, Store::last_check),
        skipped: store_fact(store, |store| {
            Ok(store
                .rejections(eclipse::VERSION)?
                .skipped_versions()
                .collect())
        }),
        play_sign_in: eclipse::apk::play::Account::open()
            .map_err(|error| error.to_string())
            .and_then(|account| {
                let path = account.credentials_path();
                let saved = path
                    .try_exists()
                    .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
                Ok(if saved {
                    PlaySignIn::Saved
                } else {
                    PlaySignIn::NotSaved
                })
            }),
    }
}

fn examine_logs(dir: &Path) -> Fact<Logs> {
    let runs = eclipse::diagnostics::kept_runs(dir).map_err(|error| error.to_string())?;
    let newest = runs.first().map(|run| NewestRun {
        head: run.head.clone(),
        outcome: eclipse::diagnostics::run_outcome(&run.head).map_err(|error| error.to_string()),
    });
    Ok(Logs {
        dir: dir.to_owned(),
        runs: runs.len(),
        bytes: runs.iter().map(|run| run.bytes).sum(),
        newest,
    })
}

fn or_unknown(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("unknown")
}

fn fact_line<T>(
    f: &mut fmt::Formatter<'_>,
    label: &str,
    fact: &Fact<T>,
    show: impl FnOnce(&T) -> String,
) -> fmt::Result {
    match fact {
        Ok(value) => writeln!(f, "  {label}: {}", show(value)),
        Err(error) => writeln!(f, "  {label}: {error}"),
    }
}

impl fmt::Display for Doctor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_eclipse(f)?;
        self.write_system(f)?;
        self.write_cpu(f)?;
        writeln!(f, "Graphics")?;
        f.write_str(&self.graphics.section)?;
        self.write_storage(f)?;
        self.write_roblox(f)?;
        self.write_config(f)?;
        self.write_url_handler(f)?;
        self.write_logs(f)?;
        writeln!(f, "Problems")?;
        let problems = self.problems();
        if problems.is_empty() {
            writeln!(f, "  None found.")?;
        }
        for problem in problems {
            writeln!(f, "  - {problem}")?;
        }
        Ok(())
    }
}

impl Doctor {
    fn write_eclipse(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Eclipse")?;
        writeln!(f, "  Version: {}", self.version)?;
        match &self.install {
            Ok(Install::Flatpak(flatpak)) => {
                writeln!(
                    f,
                    "  Flatpak: {}, branch {}, {}, commit {}",
                    flatpak.app_id,
                    or_unknown(&flatpak.branch),
                    or_unknown(&flatpak.arch),
                    or_unknown(&flatpak.commit)
                )?;
                writeln!(
                    f,
                    "  Runtime: {}, commit {}",
                    or_unknown(&flatpak.runtime),
                    or_unknown(&flatpak.runtime_commit)
                )?;
                writeln!(
                    f,
                    "  Flatpak version: {}",
                    or_unknown(&flatpak.flatpak_version)
                )
            }
            Ok(Install::Host { executable }) => {
                writeln!(f, "  Executable: {}", executable.display())
            }
            Err(error) => writeln!(f, "  Installation: {error}"),
        }
    }

    fn write_system(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let system = &self.system;
        writeln!(f, "System")?;
        fact_line(f, "OS", &system.os, Clone::clone)?;
        fact_line(f, "Kernel", &system.kernel, Clone::clone)?;
        writeln!(f, "  Desktop: {}", or_unknown(&system.desktop))?;
        writeln!(f, "  Session: {}", or_unknown(&system.session))?;
        writeln!(f, "  Eclipse's windows: {}", system.windows)?;
        writeln!(f, "  Controllers: {}", system.controllers)?;
        fact_line(f, "SDL", &system.sdl, Clone::clone)?;
        fact_line(f, "Memory", &self.memory, |memory| match memory.available {
            Some(available) => format!(
                "{} in all, {} available",
                size_text(memory.total),
                size_text(available)
            ),
            None => format!("{} in all", size_text(memory.total)),
        })
    }

    fn write_cpu(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "CPU")?;
        fact_line(f, "Model", &self.cpu.info, |info| {
            format!("{}, {} logical CPUs", or_unknown(&info.model), info.logical)
        })?;
        writeln!(f, "  Features for ART: {}", self.cpu.features)?;
        match &self.cpu.baseline {
            Ok(()) => writeln!(f, "  Android x86-64 baseline: met"),
            Err(error) => writeln!(f, "  Android x86-64 baseline: {error}"),
        }
    }

    fn write_storage(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Storage")?;
        for place in &self.storage.places {
            match (&place.path, &place.free) {
                (Ok(path), Ok(free)) => writeln!(
                    f,
                    "  {}: {} ({} free)",
                    place.kind,
                    path.display(),
                    size_text(*free)
                )?,
                (Ok(path), Err(error)) => {
                    writeln!(f, "  {}: {} ({error})", place.kind, path.display())?
                }
                (Err(error), _) => writeln!(f, "  {}: {error}", place.kind)?,
            }
        }
        fact_line(
            f,
            "Runtime directory",
            &self.storage.runtime_dir,
            |problem| match problem {
                Some(problem) => problem.to_string(),
                None => "usable".to_owned(),
            },
        )
    }

    fn write_roblox(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let roblox = &self.roblox;
        writeln!(f, "Roblox")?;
        fact_line(
            f,
            "Installed",
            &roblox.installed,
            |installed| match installed {
                Some(installed) => installed.to_string(),
                None => "no".to_owned(),
            },
        )?;
        match &roblox.stored {
            Ok(stored) => {
                for stored in stored {
                    let category = Category::RobloxVersion {
                        version: stored.version.clone(),
                        role: stored.role,
                        starts: Starts(stored.state),
                    };
                    writeln!(f, "  Stored: {category}")?;
                }
            }
            Err(error) => writeln!(f, "  Stored: {error}")?,
        }
        fact_line(
            f,
            "Last update check",
            &roblox.last_check,
            |check| match check {
                Some(check) => {
                    let outcome = match check.outcome {
                        CheckOutcome::Completed => "completed",
                        CheckOutcome::Failed => "failed",
                    };
                    format!("{}, {outcome}", eclipse::diagnostics::utc_text(check.at))
                }
                None => "never".to_owned(),
            },
        )?;
        match &roblox.skipped {
            Ok(skipped) => {
                for (version, because) in skipped {
                    let because = match because {
                        LeftBecause::FailedToStart => "it failed to start twice".to_owned(),
                        LeftBecause::RolledBack => format!(
                            "you went back from it with `{}`",
                            self.packaging().command("rollback")
                        ),
                    };
                    writeln!(
                        f,
                        "  Skipped by automatic updates: versionCode {version}, because {because}"
                    )?;
                }
            }
            Err(error) => writeln!(f, "  Skipped by automatic updates: {error}")?,
        }
        fact_line(f, "Google Play sign-in", &roblox.play_sign_in, |sign_in| {
            match sign_in {
                PlaySignIn::Saved => "saved",
                PlaySignIn::NotSaved => "none",
            }
            .to_owned()
        })
    }

    fn write_config(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Config")?;
        match &self.config.path {
            Some(path) => writeln!(f, "  File: {}", path.display())?,
            None => writeln!(f, "  File: none")?,
        }
        if self.config.problems.is_empty() {
            writeln!(f, "  Loads: yes")?;
        }
        for problem in &self.config.problems {
            writeln!(f, "  Problem: {problem}")?;
        }
        writeln!(f, "  In effect:")?;
        let settings =
            serde_json::to_value(&self.config.settings).expect("the settings have a JSON form");
        let settings = settings
            .as_object()
            .expect("the settings are a JSON object");
        for (key, value) in settings {
            if key == FFLAGS_KEY {
                writeln!(f, "    {key}: {}", flag_names(&self.config.settings.fflags))?;
            } else {
                writeln!(f, "    {key}: {value}")?;
            }
        }
        Ok(())
    }

    fn write_url_handler(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Roblox links")?;
        fact_line(f, "Handler", &self.url_handler, |handler| match handler {
            UrlHandler::Exported(path) => format!(
                "Eclipse cannot see which app opens them from inside Flatpak; to make Eclipse \
                 open them, run on the host: `{}`",
                desktop_integration::make_default_handler_command(path)
            ),
            UrlHandler::NotExported(path) => {
                format!("missing at {}; reinstall the Flatpak", path.display())
            }
            UrlHandler::Host(HostHandler::Eclipse) => "Eclipse".to_owned(),
            UrlHandler::Host(HostHandler::Other(desktop_id)) => {
                format!("{desktop_id}; run `eclipse install-url-handler` to open them in Eclipse")
            }
            UrlHandler::Host(HostHandler::Nothing) => {
                "none; run `eclipse install-url-handler` to open them in Eclipse".to_owned()
            }
        })
    }

    fn write_logs(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Logs")?;
        let logs = match &self.logs {
            Ok(logs) => logs,
            Err(error) => return writeln!(f, "  Directory: {error}"),
        };
        writeln!(f, "  Directory: {}", logs.dir.display())?;
        let Some(newest) = &logs.newest else {
            return writeln!(f, "  Kept runs: none");
        };
        writeln!(f, "  Kept runs: {}, {}", logs.runs, size_text(logs.bytes))?;
        let name = newest.head.file_name().unwrap_or(newest.head.as_os_str());
        writeln!(f, "  Newest run: {}", Path::new(name).display())?;
        fact_line(
            f,
            "How it ended",
            &newest.outcome,
            |outcome| match outcome {
                Some(outcome) => outcome.replace('\n', "\n    "),
                None => "not recorded; it may still be running".to_owned(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FEATURES: &str = "ssse3,sse4.1,sse4.2,avx,avx2,popcnt";

    const RX_6800: &str = "  Device 0: AMD Radeon RX 6800 (RADV NAVI21) [1002:73bf], discrete, \
                           Vulkan 1.4.312, driver radv Mesa 25.2.4";

    const DRAWS_WITH_VULKAN: &str = "  Roblox draws with: Vulkan\n";

    const ON_LLVMPIPE: &str = "Roblox is rendering on the CPU (llvmpipe), which is slow.";

    fn flatpak() -> Packaging {
        Packaging::Flatpak {
            app_id: "io.github.kuenec.Eclipse".to_owned(),
        }
    }

    fn healthy() -> Doctor {
        Doctor {
            version: "0.1.9",
            install: Ok(Install::Flatpak(FlatpakInstall {
                app_id: "io.github.kuenec.Eclipse".to_owned(),
                branch: Some("stable".to_owned()),
                arch: Some("x86_64".to_owned()),
                commit: Some("0123456789ab".to_owned()),
                runtime: Some("runtime/org.gnome.Platform/x86_64/51".to_owned()),
                runtime_commit: None,
                flatpak_version: Some("1.16.1".to_owned()),
            })),
            system: System {
                os: Ok("Arch Linux".to_owned()),
                kernel: Ok("6.17.1-arch1-1".to_owned()),
                desktop: Some("KDE".to_owned()),
                session: Some("wayland".to_owned()),
                windows: WindowSystem::Wayland,
                controllers: DeviceAccess::Visible,
                sdl: Ok("3.2.24".to_owned()),
            },
            cpu: Cpu {
                info: Ok(CpuInfo {
                    model: Some("AMD Ryzen 7 5800X 8-Core Processor".to_owned()),
                    logical: 16,
                }),
                features: ALL_FEATURES.to_owned(),
                baseline: Ok(()),
            },
            memory: Ok(Memory {
                total: 32 * GIB,
                available: Some(20 * GIB + 512 * MIB),
            }),
            graphics: Graphics {
                section: format!("{RX_6800}\n{DRAWS_WITH_VULKAN}"),
                problems: Vec::new(),
            },
            storage: Storage {
                runtime_dir: Ok(None),
                places: vec![
                    Place {
                        kind: PlaceKind::AppData,
                        path: Ok(PathBuf::from("/data/eclipse/app-data")),
                        free: Ok(100 * GIB),
                    },
                    Place {
                        kind: PlaceKind::RobloxStore,
                        path: Ok(PathBuf::from("/data/eclipse/roblox")),
                        free: Ok(100 * GIB),
                    },
                    Place {
                        kind: PlaceKind::Cache,
                        path: Err("cannot resolve the cache directory".to_owned()),
                        free: Err("cannot resolve the cache directory".to_owned()),
                    },
                ],
            },
            roblox: Roblox {
                installed: Ok(Some(InstalledVersion {
                    version_code: VersionCode(3212),
                    version_name: Some("2.692.843".to_owned()),
                })),
                stored: Ok(Vec::new()),
                last_check: Ok(Some(UpdateCheck {
                    at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
                    outcome: CheckOutcome::Completed,
                })),
                skipped: Ok(Vec::new()),
                play_sign_in: Ok(PlaySignIn::NotSaved),
            },
            config: ConfigFile {
                path: Some(PathBuf::from("/config/eclipse/config.json")),
                problems: Vec::new(),
                repair: ConfigRepair::InSettings,
                settings: Config {
                    fflags: BTreeMap::from([
                        (
                            "FFlagDebugGraphicsPreferVulkan".to_owned(),
                            serde_json::Value::from("True"),
                        ),
                        (
                            "DFIntTaskSchedulerTargetFps".to_owned(),
                            serde_json::Value::from(144),
                        ),
                    ]),
                    ..Config::default()
                },
            },
            url_handler: Ok(UrlHandler::Exported(PathBuf::from(
                "/app/share/applications/io.github.kuenec.Eclipse.UrlHandler.desktop",
            ))),
            logs: Ok(Logs {
                dir: PathBuf::from("/data/eclipse/app-data/logs"),
                runs: 2,
                bytes: 3 * MIB,
                newest: Some(NewestRun {
                    head: PathBuf::from(
                        "/data/eclipse/app-data/logs/eclipse-20270115T080000.692Z.log",
                    ),
                    outcome: Ok(Some(
                        "Roblox crashed (signal 11, SIGSEGV: invalid memory access)\nLast error: \
                         the engine stopped"
                            .to_owned(),
                    )),
                }),
            }),
        }
    }

    #[test]
    fn a_healthy_setup_has_no_problems() {
        assert_eq!(healthy().problems(), []);
    }

    #[test]
    fn a_cpu_below_the_baseline_is_named() {
        let mut doctor = healthy();
        doctor.cpu.baseline = Err(RuntimeError::CpuLacksFeature("SSE4.1"));
        assert_eq!(doctor.problems(), [Problem::CpuBelowBaseline("SSE4.1")]);
    }

    #[test]
    fn graphics_problems_follow_the_cpu_and_precede_the_storage() {
        let mut doctor = healthy();
        doctor.cpu.baseline = Err(RuntimeError::CpuLacksFeature("SSE4.1"));
        doctor.graphics.problems = vec![ON_LLVMPIPE.to_owned()];
        let noexec = RuntimeDirProblem::NoExec(PathBuf::from("/media/games/eclipse/runtime"));
        doctor.storage.runtime_dir = Ok(Some(noexec.clone()));
        assert_eq!(
            doctor.problems(),
            [
                Problem::CpuBelowBaseline("SSE4.1"),
                Problem::Graphics(ON_LLVMPIPE.to_owned()),
                Problem::RuntimeDir(noexec),
            ]
        );
        assert!(
            doctor
                .to_string()
                .contains(&format!("\n  - {ON_LLVMPIPE}\n")),
            "{doctor}"
        );
    }

    #[test]
    fn runtime_directory_problems_are_named() {
        for problem in [
            RuntimeDirProblem::NoExec(PathBuf::from("/media/games/eclipse/runtime")),
            RuntimeDirProblem::SearchPathSeparator(PathBuf::from("/games:x/runtime")),
        ] {
            let mut doctor = healthy();
            doctor.storage.runtime_dir = Ok(Some(problem.clone()));
            assert_eq!(doctor.problems(), [Problem::RuntimeDir(problem)]);
        }
    }

    #[test]
    fn runtime_directories_need_exec_and_no_search_path_separator() {
        let dir = Path::new("/home/player/.local/share/eclipse/app-data/runtime");
        assert_eq!(
            runtime_dir_problem_on(dir, StatVfsMountFlags::empty()),
            None
        );
        assert_eq!(
            runtime_dir_problem_on(dir, StatVfsMountFlags::NOEXEC | StatVfsMountFlags::NOSUID),
            Some(RuntimeDirProblem::NoExec(dir.to_owned()))
        );
        for separated in ["/games:eclipse/runtime", "/games;eclipse/runtime"] {
            let separated = Path::new(separated);
            assert_eq!(
                runtime_dir_problem_on(separated, StatVfsMountFlags::NOEXEC),
                Some(RuntimeDirProblem::SearchPathSeparator(separated.to_owned()))
            );
        }
        let text = RuntimeDirProblem::NoExec(dir.to_owned()).to_string();
        assert!(
            text.contains("noexec") && text.contains("ECLIPSE_APP_DATA_DIR"),
            "{text}"
        );
    }

    #[test]
    fn little_free_space_for_the_roblox_store_is_named() {
        let mut doctor = healthy();
        doctor.storage.places[1].free = Ok(512 * MIB);
        doctor.storage.places[0].free = Ok(10 * MIB);
        let problems = doctor.problems();
        assert_eq!(
            problems,
            [Problem::LowSpace {
                path: PathBuf::from("/data/eclipse/roblox"),
                free: 512 * MIB,
            }]
        );
        assert!(problems[0]
            .to_string()
            .starts_with("Only 512.0 MiB is free"));
    }

    #[test]
    fn an_invalid_config_is_named() {
        let mut doctor = healthy();
        doctor.config.problems =
            vec!["/config/eclipse/config.json:1:21: invalid JSON: trailing comma".to_owned()];
        doctor.config.repair = ConfigRepair::ByHand;
        let problems = doctor.problems();
        assert_eq!(
            problems,
            [Problem::InvalidConfig {
                repair: ConfigRepair::ByHand,
                packaging: flatpak(),
            }]
        );
        assert!(
            problems[0].to_string().contains(
                "Settings and `flatpak run io.github.kuenec.Eclipse config set` cannot change"
            ),
            "{}",
            problems[0]
        );
    }

    #[test]
    fn a_missing_install_and_a_failed_update_check_are_named() {
        let mut doctor = healthy();
        doctor.roblox.installed = Ok(None);
        assert_eq!(
            doctor.problems(),
            [Problem::NotInstalled {
                packaging: flatpak()
            }]
        );

        doctor = healthy();
        doctor.roblox.last_check = Ok(Some(UpdateCheck {
            at: std::time::UNIX_EPOCH,
            outcome: CheckOutcome::Failed,
        }));
        assert_eq!(
            doctor.problems(),
            [Problem::LastCheckFailed {
                packaging: flatpak()
            }]
        );
    }

    #[test]
    fn fixes_name_commands_the_way_this_install_runs_eclipse() {
        let mut doctor = healthy();
        doctor.roblox.installed = Ok(None);
        doctor.roblox.last_check = Ok(Some(UpdateCheck {
            at: std::time::UNIX_EPOCH,
            outcome: CheckOutcome::Failed,
        }));
        doctor.config.problems = vec!["config.json:1:2: invalid JSON".to_owned()];
        let in_flatpak = doctor.fixes();
        doctor.install = Ok(Install::Host {
            executable: PathBuf::from("/usr/bin/eclipse"),
        });
        let on_host = doctor.fixes();

        assert_eq!(
            in_flatpak,
            [
                "The settings file has problems, listed under Config. Eclipse uses the defaults \
                 for those settings until they are fixed in Settings or with `flatpak run \
                 io.github.kuenec.Eclipse config set`.",
                "Roblox is not installed. Start Eclipse to download it, or run `flatpak run \
                 io.github.kuenec.Eclipse update`.",
                "The last check for a Roblox update failed. The log of that launch says why; \
                 `flatpak run io.github.kuenec.Eclipse update` tries again.",
            ]
        );
        for (flatpak, host) in in_flatpak.iter().zip(&on_host) {
            assert_eq!(
                *host,
                flatpak.replace("flatpak run io.github.kuenec.Eclipse ", "eclipse ")
            );
        }
    }

    #[test]
    fn user_fast_flags_are_counted_and_named_without_their_values() {
        let flags = |count: usize| -> BTreeMap<String, serde_json::Value> {
            (0..count)
                .map(|index| (format!("FFlag{index:03}"), serde_json::Value::from("True")))
                .collect()
        };
        assert_eq!(flag_names(&flags(0)), "none");
        assert_eq!(
            flag_names(&flags(1)),
            "1 flag, its name only: [\"FFlag000\"]"
        );
        assert_eq!(
            flag_names(&flags(2)),
            "2 flags, names only: [\"FFlag000\",\"FFlag001\"]"
        );
        let many = flag_names(&flags(1_000));
        assert!(
            many.starts_with("1000 flags, the first 40 names only: [\"FFlag000\","),
            "{many}"
        );
        assert!(many.ends_with(",\"FFlag039\"]"), "{many}");
    }

    #[test]
    fn the_report_lists_every_section_and_problem() {
        let mut doctor = healthy();
        doctor.roblox.installed = Ok(None);
        doctor.roblox.skipped = Ok(vec![(VersionCode(3170), LeftBecause::RolledBack)]);
        assert_eq!(
            doctor.to_string(),
            "\
Eclipse
  Version: 0.1.9
  Flatpak: io.github.kuenec.Eclipse, branch stable, x86_64, commit 0123456789ab
  Runtime: runtime/org.gnome.Platform/x86_64/51, commit unknown
  Flatpak version: 1.16.1
System
  OS: Arch Linux
  Kernel: 6.17.1-arch1-1
  Desktop: KDE
  Session: wayland
  Eclipse's windows: Wayland
  Controllers: input devices are visible
  SDL: 3.2.24
  Memory: 32.0 GiB in all, 20.5 GiB available
CPU
  Model: AMD Ryzen 7 5800X 8-Core Processor, 16 logical CPUs
  Features for ART: ssse3,sse4.1,sse4.2,avx,avx2,popcnt
  Android x86-64 baseline: met
Graphics
  Device 0: AMD Radeon RX 6800 (RADV NAVI21) [1002:73bf], discrete, Vulkan 1.4.312, driver radv \
Mesa 25.2.4
  Roblox draws with: Vulkan
Storage
  App data: /data/eclipse/app-data (100.0 GiB free)
  Roblox store: /data/eclipse/roblox (100.0 GiB free)
  Cache: cannot resolve the cache directory
  Runtime directory: usable
Roblox
  Installed: no
  Last update check: 2027-01-15 08:00 UTC, completed
  Skipped by automatic updates: versionCode 3170, because you went back from it with \
`flatpak run io.github.kuenec.Eclipse rollback`
  Google Play sign-in: none
Config
  File: /config/eclipse/config.json
  Loads: yes
  In effect:
    allow_gamepad_permission: true
    audio_input_device: \"default\"
    audio_output_device: \"default\"
    close_on_leave: \"browser\"
    enable_gamemode: true
    fflags: 2 flags, names only: [\"DFIntTaskSchedulerTargetFps\",\
\"FFlagDebugGraphicsPreferVulkan\"]
    graphics_optimization_mode: \"balanced\"
    roblox_auto_update: true
    server_location_indicator_enabled: false
    touch_mode: \"off\"
    unfocused_fps_limit: null
    use_opengl: false
    vulkan_device: null
    webview_helper_path: null
Roblox links
  Handler: Eclipse cannot see which app opens them from inside Flatpak; to make Eclipse open \
them, run on the host: `xdg-mime default io.github.kuenec.Eclipse.UrlHandler.desktop \
x-scheme-handler/roblox-player x-scheme-handler/roblox`
Logs
  Directory: /data/eclipse/app-data/logs
  Kept runs: 2, 3.0 MiB
  Newest run: eclipse-20270115T080000.692Z.log
  How it ended: Roblox crashed (signal 11, SIGSEGV: invalid memory access)
    Last error: the engine stopped
Problems
  - Roblox is not installed. Start Eclipse to download it, or run `flatpak run \
io.github.kuenec.Eclipse update`.
"
        );
    }

    #[test]
    fn a_report_without_facts_names_each_failure() {
        let mut doctor = healthy();
        doctor.install = Err("cannot read /.flatpak-info: permission denied".to_owned());
        doctor.logs = Err(NO_APP_DATA_DIR.to_owned());
        doctor.config.problems = vec!["config.json:1:2: invalid JSON".to_owned()];
        doctor.system.sdl = Err("libSDL3.so.0 could not be loaded: not found".to_owned());
        let text = doctor.to_string();
        for line in [
            "  Installation: cannot read /.flatpak-info: permission denied\n",
            "  SDL: libSDL3.so.0 could not be loaded: not found\n",
            "  Problem: config.json:1:2: invalid JSON\n",
            "Logs\n  Directory: cannot resolve Eclipse's app-data directory",
            "  - The settings file has problems",
        ] {
            assert!(text.contains(line), "{line:?} in {text}");
        }
        assert!(!text.contains("Loads: yes"), "{text}");
    }

    #[test]
    fn eclipse_windows_follow_the_display_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        for (pairs, expected) in [
            (
                &[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":0")][..],
                WindowSystem::Wayland,
            ),
            (&[("WAYLAND_SOCKET", "3")][..], WindowSystem::Wayland),
            (
                &[
                    ("WAYLAND_DISPLAY", ""),
                    ("DISPLAY", ":0"),
                    ("XDG_SESSION_TYPE", "wayland"),
                ][..],
                WindowSystem::XWayland,
            ),
            (
                &[("DISPLAY", ":0"), ("XDG_SESSION_TYPE", "x11")][..],
                WindowSystem::X11,
            ),
            (&[("DISPLAY", "")][..], WindowSystem::Missing),
        ] {
            assert_eq!(WindowSystem::from_env(env(pairs)), expected, "{pairs:?}");
        }
    }

    #[test]
    fn system_files_are_parsed() {
        let fixture = |name| {
            std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/doctor")
                    .join(name),
            )
            .unwrap()
        };
        assert_eq!(
            parse_os_release(&fixture("os-release")),
            Some("Fedora Linux 42 (Workstation Edition)".to_owned())
        );
        assert_eq!(
            parse_os_release("PRETTY_NAME='Debian GNU/Linux 13'\n"),
            Some("Debian GNU/Linux 13".to_owned())
        );
        assert_eq!(
            parse_os_release("PRETTY_NAME=CachyOS\n"),
            Some("CachyOS".to_owned())
        );
        assert_eq!(parse_os_release("NAME=Arch\nPRETTY_NAME=\"\"\n"), None);

        assert_eq!(
            parse_mem_info(&fixture("meminfo")),
            Some(Memory {
                total: 32_768_000 * KIB,
                available: Some(20_480_000 * KIB),
            })
        );
        assert_eq!(
            parse_mem_info("MemTotal:  1024 kB\n"),
            Some(Memory {
                total: 1024 * KIB,
                available: None,
            })
        );
        assert_eq!(parse_mem_info("MemFree: 1 kB\n"), None);

        assert_eq!(
            parse_cpu_info(&fixture("cpuinfo")),
            CpuInfo {
                model: Some("Intel(R) Core(TM)2 Duo CPU     E8400  @ 3.00GHz".to_owned()),
                logical: 2,
            }
        );
    }
}
