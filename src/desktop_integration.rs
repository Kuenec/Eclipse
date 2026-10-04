use directories::BaseDirs;
use eclipse::flatpak;
use eclipse_config::temp_file::TempFile;
use std::ffi::OsStr;
use std::io::{self, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DESKTOP_FILE_ID: &str = "dev.eclipse.RobloxPlayer.desktop";
const URL_HANDLER_MIMES: [&str; 2] = ["x-scheme-handler/roblox-player", "x-scheme-handler/roblox"];
pub(super) const BROWSER_HANDLER_COMMAND: &str = "__handle-roblox-player-url";

const FLATPAK_APPLICATIONS_DIR: &str = "/app/share/applications";
const FLATPAK_URL_HANDLER_SUFFIX: &str = ".UrlHandler.desktop";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KdeConfigWriter {
    Plasma6,
    Plasma5,
}

impl KdeConfigWriter {
    const PREFERENCE: [Self; 2] = [Self::Plasma6, Self::Plasma5];

    fn program(self) -> &'static str {
        match self {
            Self::Plasma6 => "kwriteconfig6",
            Self::Plasma5 => "kwriteconfig5",
        }
    }

    fn cache_builder(self) -> &'static str {
        match self {
            Self::Plasma6 => "kbuildsycoca6",
            Self::Plasma5 => "kbuildsycoca5",
        }
    }

    fn default_application_args(self, mime: &'static str) -> Vec<&'static str> {
        let mut args = vec![
            "--file",
            "mimeapps.list",
            "--group",
            "Default Applications",
            "--key",
            mime,
        ];
        match self {
            Self::Plasma6 => args.push("--notify"),
            Self::Plasma5 => {}
        }
        args.push(DESKTOP_FILE_ID);
        args
    }
}

#[derive(Debug)]
pub(super) enum UrlHandlerInstall {
    Registered {
        desktop_path: PathBuf,
    },
    FlatpakExport {
        app_id: String,
        desktop_path: PathBuf,
    },
}

pub(super) fn install_url_handler() -> Result<UrlHandlerInstall, Box<dyn std::error::Error>> {
    match sandbox_app_id()? {
        Some(app_id) => flatpak_exported_handler(app_id),
        None => install_host_url_handler()
            .map(|desktop_path| UrlHandlerInstall::Registered { desktop_path }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Packaging {
    Flatpak { app_id: String },
    Host,
}

impl Packaging {
    pub(super) fn detect() -> io::Result<Self> {
        Ok(match sandbox_app_id()? {
            Some(app_id) => Self::Flatpak { app_id },
            None => Self::Host,
        })
    }

    pub(super) fn command(&self, arguments: &str) -> String {
        match self {
            Self::Flatpak { app_id } => format!("flatpak run {app_id} {arguments}"),
            Self::Host => format!("eclipse {arguments}"),
        }
    }
}

pub(super) fn sandbox_app_id() -> io::Result<Option<String>> {
    let Some(info) = flatpak_info()? else {
        return Ok(None);
    };
    Ok(Some(flatpak_app_id(&info)?.to_owned()))
}

pub(super) fn flatpak_info() -> io::Result<Option<String>> {
    match std::fs::read_to_string(flatpak::INFO_PATH) {
        Ok(info) => Ok(Some(info)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot read {} to detect the Flatpak sandbox: {error}",
                flatpak::INFO_PATH
            ),
        )),
    }
}

pub(super) fn flatpak_app_id(info: &str) -> io::Result<&str> {
    flatpak::info_value(info, "Application", "name").ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{} names no [Application] id", flatpak::INFO_PATH),
        )
    })
}

fn flatpak_exported_handler(
    app_id: String,
) -> Result<UrlHandlerInstall, Box<dyn std::error::Error>> {
    let desktop_path = flatpak_url_handler_path(&app_id);
    if !desktop_path.is_file() {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "this Flatpak does not export its URL handler at {}; reinstall the Flatpak",
                desktop_path.display()
            ),
        )
        .into());
    }
    Ok(UrlHandlerInstall::FlatpakExport {
        app_id,
        desktop_path,
    })
}

pub(super) fn flatpak_url_handler_path(app_id: &str) -> PathBuf {
    Path::new(FLATPAK_APPLICATIONS_DIR).join(format!("{app_id}{FLATPAK_URL_HANDLER_SUFFIX}"))
}

pub(super) fn flatpak_handler_notice(app_id: &str, desktop_path: &Path) -> String {
    format!(
        "Eclipse is running inside the {app_id} Flatpak, which installed its own {} handler ({}) \
         together with the app, so nothing was written to the sandbox's private data directory. If \
         another app is the default handler, run this on the host: {}",
        URL_HANDLER_MIMES.join(" and "),
        desktop_path.display(),
        make_default_handler_command(desktop_path)
    )
}

pub(super) fn make_default_handler_command(desktop_path: &Path) -> String {
    let desktop_id = desktop_path
        .file_name()
        .map(OsStr::to_string_lossy)
        .unwrap_or_default();
    format!(
        "xdg-mime default {desktop_id} {}",
        URL_HANDLER_MIMES.join(" ")
    )
}

fn install_host_url_handler() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot locate the Eclipse executable for the URL handler: {error}"),
        )
    })?;
    let handler = executable.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot resolve {}: {error}", executable.display()),
        )
    })?;
    let base_dirs = BaseDirs::new().ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            "cannot resolve the user data directory for the URL handler",
        )
    })?;
    let applications = base_dirs.data_dir().join("applications");
    let desktop_path = write_desktop_file(&applications, &handler)?;

    if let Some(validator) = find_on_path("desktop-file-validate") {
        run_checked(
            Command::new(validator).arg(&desktop_path),
            "validate the desktop entry",
        )?;
    }
    if let Some(database_updater) = find_on_path("update-desktop-database") {
        run_checked(
            Command::new(database_updater).arg(&applications),
            "update the desktop MIME database",
        )?;
    }

    let xdg_mime = find_on_path("xdg-mime").ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            "xdg-mime is required to register the Roblox URL handlers",
        )
    })?;
    if kde_session() {
        let (writer, kwriteconfig) = select_kde_config_writer(find_on_path).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                "kwriteconfig6 (Plasma 6) or kwriteconfig5 (Plasma 5) is required to register the \
                 URL handler in this KDE session",
            )
        })?;
        for mime in URL_HANDLER_MIMES {
            run_checked(
                Command::new(&kwriteconfig).args(writer.default_application_args(mime)),
                "set the KDE Roblox URL handler",
            )?;
        }
        if let Some(cache_builder) = find_on_path(writer.cache_builder()) {
            run_checked(
                Command::new(cache_builder).arg("--noincremental"),
                "rebuild the KDE application cache",
            )?;
        }
    } else {
        run_checked(
            Command::new(&xdg_mime)
                .arg("default")
                .arg(DESKTOP_FILE_ID)
                .args(URL_HANDLER_MIMES),
            "set the Roblox URL handlers",
        )?;
    }

    for mime in URL_HANDLER_MIMES {
        if default_handler(&xdg_mime, mime)?.as_deref() != Some(DESKTOP_FILE_ID) {
            return Err(io::Error::other(format!(
                "the desktop environment did not retain Eclipse as the {mime} handler"
            ))
            .into());
        }
    }

    Ok(desktop_path)
}

#[derive(Debug)]
pub(super) enum HostHandler {
    Eclipse,
    Other(String),
    Nothing,
}

pub(super) fn host_url_handler() -> io::Result<HostHandler> {
    let xdg_mime = find_on_path("xdg-mime").ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            "xdg-mime is not installed, so the handler of Roblox links is unknown",
        )
    })?;
    Ok(match default_handler(&xdg_mime, URL_HANDLER_MIMES[0])? {
        Some(desktop_id) if desktop_id == DESKTOP_FILE_ID => HostHandler::Eclipse,
        Some(desktop_id) => HostHandler::Other(desktop_id),
        None => HostHandler::Nothing,
    })
}

fn default_handler(xdg_mime: &Path, mime: &str) -> io::Result<Option<String>> {
    let query = Command::new(xdg_mime)
        .arg("query")
        .arg("default")
        .arg(mime)
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot run {} to query the {mime} handler: {error}",
                    xdg_mime.display()
                ),
            )
        })?;
    if !query.status.success() {
        return Err(io::Error::other(format!(
            "{} query default {mime} failed: {}",
            xdg_mime.display(),
            query.status
        )));
    }
    let desktop_id = String::from_utf8_lossy(&query.stdout).trim().to_owned();
    Ok(Some(desktop_id).filter(|desktop_id| !desktop_id.is_empty()))
}

fn write_desktop_file(
    applications: &Path,
    handler: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(applications).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot create {}: {error}", applications.display()),
        )
    })?;
    let desktop_path = applications.join(DESKTOP_FILE_ID);
    let write_error = |error: io::Error| {
        io::Error::new(
            error.kind(),
            format!("cannot write {}: {error}", desktop_path.display()),
        )
    };
    let mut temporary = TempFile::create(applications, DESKTOP_FILE_ID).map_err(write_error)?;
    temporary
        .write_all(desktop_entry(handler)?.as_bytes())
        .map_err(write_error)?;
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o644))
        .map_err(write_error)?;
    temporary.persist(&desktop_path).map_err(write_error)?;
    Ok(desktop_path)
}

fn select_kde_config_writer(
    find: impl Fn(&str) -> Option<PathBuf>,
) -> Option<(KdeConfigWriter, PathBuf)> {
    KdeConfigWriter::PREFERENCE
        .into_iter()
        .find_map(|writer| find(writer.program()).map(|path| (writer, path)))
}

fn desktop_entry(handler: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let handler = desktop_exec_argument(handler.as_os_str())?;
    let mime_types: String = URL_HANDLER_MIMES
        .iter()
        .map(|mime| format!("{mime};"))
        .collect();
    Ok(format!(
        "[Desktop Entry]\n\
         Version=1.5\n\
         Type=Application\n\
         Name=Eclipse Roblox Player\n\
         Comment=Launch Roblox experiences through Eclipse\n\
         NoDisplay=true\n\
         Terminal=false\n\
         StartupNotify=true\n\
         Exec={handler} {BROWSER_HANDLER_COMMAND} %u\n\
         MimeType={mime_types}\n"
    ))
}

fn desktop_exec_argument(value: &OsStr) -> Result<String, Box<dyn std::error::Error>> {
    let value = value.to_str().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            "the Eclipse handler path is not valid UTF-8",
        )
    })?;
    if !value.is_ascii() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "the Eclipse handler path must contain printable ASCII characters",
        )
        .into());
    }

    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\\\\\"),
            '"' => escaped.push_str("\\\\\""),
            '`' => escaped.push_str("\\\\`"),
            '$' => escaped.push_str("\\\\$"),
            '%' => escaped.push_str("%%"),
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn kde_session() -> bool {
    std::env::var_os("KDE_SESSION_VERSION").is_some()
        || std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| {
            desktops
                .split(':')
                .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
        })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn run_checked(
    command: &mut Command,
    action: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    let status = command.status().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot run {} to {action}: {error}",
                Path::new(command.get_program()).display()
            ),
        )
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("failed to {action}: {status}")).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_desktop_file_that_cannot_be_written_leaves_no_temporary() {
        let applications = std::env::temp_dir().join(format!(
            "eclipse-desktop-file-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&applications).ok();
        std::fs::create_dir_all(applications.join(DESKTOP_FILE_ID)).unwrap();

        let result = write_desktop_file(&applications, Path::new("/usr/bin/eclipse"));
        let left: Vec<_> = std::fs::read_dir(&applications)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        std::fs::remove_dir_all(&applications).ok();
        assert!(result.is_err(), "the desktop file path is a directory");
        assert_eq!(left, [DESKTOP_FILE_ID]);
    }

    #[test]
    fn desktop_file_errors_name_the_step_and_path_that_failed() {
        let root = std::env::temp_dir().join(format!(
            "eclipse-desktop-file-errors-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let uncreatable = root.join("file").join("applications");
        std::fs::write(root.join("file"), b"").unwrap();
        let applications = root.join("applications");
        std::fs::create_dir_all(applications.join(DESKTOP_FILE_ID)).unwrap();
        let handler = Path::new("/usr/bin/eclipse");

        let create = write_desktop_file(&uncreatable, handler);
        let persist = write_desktop_file(&applications, handler);
        std::fs::remove_dir_all(&root).ok();
        let create = create.unwrap_err().to_string();
        assert!(
            create.starts_with(&format!("cannot create {}: ", uncreatable.display())),
            "{create}"
        );
        let persist = persist.unwrap_err().to_string();
        let desktop_path = applications.join(DESKTOP_FILE_ID);
        assert!(
            persist.starts_with(&format!("cannot write {}: ", desktop_path.display())),
            "{persist}"
        );
    }

    #[test]
    fn commands_that_cannot_start_name_the_program_and_step() {
        let program = std::env::temp_dir()
            .join(format!(
                "eclipse-desktop-missing-program-{:?}",
                std::thread::current().id()
            ))
            .join("xdg-mime");

        let error = run_checked(&mut Command::new(&program), "set the Roblox URL handlers")
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!(
                "cannot run {} to set the Roblox URL handlers: ",
                program.display()
            )),
            "{error}"
        );
    }

    #[test]
    fn desktop_entry_passes_one_unquoted_url_field() {
        let entry = desktop_entry(Path::new("/work/Eclipse/run-roblox.sh")).unwrap();
        assert!(
            entry.contains("Exec=\"/work/Eclipse/run-roblox.sh\" __handle-roblox-player-url %u\n")
        );
        assert!(!entry.contains("\"%u\""));
        assert!(entry.contains("MimeType=x-scheme-handler/roblox-player;"));
    }

    #[test]
    fn desktop_entries_claim_exactly_the_schemes_the_link_parser_accepts() {
        let mime_line = "\nMimeType=x-scheme-handler/roblox-player;x-scheme-handler/roblox;\n";
        let entry = desktop_entry(Path::new("/usr/bin/eclipse")).unwrap();
        assert!(entry.contains(mime_line), "{entry}");
        let exported =
            include_str!("../packaging/flatpak/io.github.kuenec.Eclipse.UrlHandler.desktop");
        assert!(exported.contains(mime_line), "{exported}");
        let listing = include_str!("../packaging/flatpak/io.github.kuenec.Eclipse.metainfo.xml");
        let provided: Vec<&str> = listing
            .split("<mediatype>")
            .skip(1)
            .filter_map(|rest| rest.split_once("</mediatype>"))
            .map(|(mime, _)| mime)
            .collect();
        assert_eq!(provided, URL_HANDLER_MIMES);

        let player_launch = "roblox-player:1+launchmode:play+gameinfo:TICKET+placelauncherurl:\
                             https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3F\
                             request%3DRequestGame%26placeId%3D90441122676618";
        for mime in URL_HANDLER_MIMES {
            let launch = match mime.strip_prefix("x-scheme-handler/").unwrap() {
                "roblox-player" => player_launch,
                "roblox" => "roblox://experiences/start?placeId=90441122676618",
                other => panic!("no launch sample for the {other} scheme"),
            };
            assert_eq!(
                eclipse::links::parse(launch),
                eclipse::links::parse("90441122676618"),
                "{mime}"
            );
        }
    }

    #[test]
    fn desktop_exec_escapes_spaces_and_literal_percent_signs() {
        assert_eq!(
            desktop_exec_argument(OsStr::new("/work/100% ready/eclipse")).unwrap(),
            "\"/work/100%% ready/eclipse\""
        );
    }

    #[test]
    fn kde_writer_prefers_plasma6_and_falls_back_to_plasma5() {
        let only = |available: &'static [&'static str]| {
            move |program: &str| {
                available
                    .contains(&program)
                    .then(|| PathBuf::from("/usr/bin").join(program))
            }
        };
        assert_eq!(
            select_kde_config_writer(only(&["kwriteconfig6", "kwriteconfig5"])),
            Some((
                KdeConfigWriter::Plasma6,
                PathBuf::from("/usr/bin/kwriteconfig6")
            ))
        );
        assert_eq!(
            select_kde_config_writer(only(&["kwriteconfig5"])),
            Some((
                KdeConfigWriter::Plasma5,
                PathBuf::from("/usr/bin/kwriteconfig5")
            ))
        );
        assert_eq!(select_kde_config_writer(only(&[])), None);
        assert_eq!(KdeConfigWriter::Plasma5.cache_builder(), "kbuildsycoca5");
        assert_eq!(KdeConfigWriter::Plasma6.cache_builder(), "kbuildsycoca6");
    }

    #[test]
    fn kwriteconfig5_arguments_omit_the_plasma6_only_notify_flag() {
        let expected = |notify: bool| {
            let mut args = vec![
                "--file",
                "mimeapps.list",
                "--group",
                "Default Applications",
                "--key",
                "x-scheme-handler/roblox",
            ];
            if notify {
                args.push("--notify");
            }
            args.push("dev.eclipse.RobloxPlayer.desktop");
            args
        };
        assert_eq!(
            KdeConfigWriter::Plasma6.default_application_args("x-scheme-handler/roblox"),
            expected(true)
        );
        assert_eq!(
            KdeConfigWriter::Plasma5.default_application_args("x-scheme-handler/roblox"),
            expected(false)
        );
    }

    #[test]
    fn the_flatpak_app_id_is_the_application_name() {
        let info = "[Application]\n\
                    name=io.github.kuenec.Eclipse\n\
                    \n\
                    [Instance]\n\
                    name=other\n";
        assert_eq!(flatpak_app_id(info).ok(), Some("io.github.kuenec.Eclipse"));
        let runtime_only = "[Runtime]\nname=org.gnome.Platform\n\n[Instance]\nname=other\n";
        assert!(flatpak_app_id(runtime_only).is_err());
        assert!(flatpak_app_id("[Application]\nname=\n").is_err());
    }

    #[test]
    fn commands_are_named_the_way_this_install_runs_eclipse() {
        let flatpak = Packaging::Flatpak {
            app_id: "io.github.kuenec.Eclipse".to_owned(),
        };
        assert_eq!(
            flatpak.command("doctor --report"),
            "flatpak run io.github.kuenec.Eclipse doctor --report"
        );
        assert_eq!(Packaging::Host.command("update"), "eclipse update");
    }

    #[test]
    fn flatpak_handler_is_the_exported_desktop_file_and_the_notice_says_so() {
        let path = flatpak_url_handler_path("io.github.kuenec.Eclipse");
        assert_eq!(
            path,
            PathBuf::from("/app/share/applications/io.github.kuenec.Eclipse.UrlHandler.desktop")
        );
        let notice = flatpak_handler_notice("io.github.kuenec.Eclipse", &path);
        assert!(
            notice.contains("inside the io.github.kuenec.Eclipse Flatpak"),
            "{notice}"
        );
        assert!(
            notice.contains(
                "installed its own x-scheme-handler/roblox-player and x-scheme-handler/roblox \
                 handler (/app/share/applications/io.github.kuenec.Eclipse.UrlHandler.desktop) \
                 together with the app"
            ),
            "{notice}"
        );
        assert!(notice.contains("nothing was written"), "{notice}");
        assert!(
            notice.ends_with(
                "xdg-mime default io.github.kuenec.Eclipse.UrlHandler.desktop \
                 x-scheme-handler/roblox-player x-scheme-handler/roblox"
            ),
            "{notice}"
        );
    }
}
