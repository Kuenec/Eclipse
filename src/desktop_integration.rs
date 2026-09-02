use directories::BaseDirs;
use std::ffi::OsStr;
use std::io::{self, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DESKTOP_FILE_ID: &str = "dev.eclipse.RobloxPlayer.desktop";
const URL_HANDLER_MIME: &str = "x-scheme-handler/roblox-player";
pub(super) const BROWSER_HANDLER_COMMAND: &str = "__handle-roblox-player-url";

pub(super) fn install_url_handler() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let handler = std::env::current_exe()?.canonicalize()?;
    let base_dirs = BaseDirs::new().ok_or_else(|| {
        io::Error::new(
            ErrorKind::NotFound,
            "cannot resolve the user data directory for the URL handler",
        )
    })?;
    let applications = base_dirs.data_dir().join("applications");
    std::fs::create_dir_all(&applications)?;

    let desktop_path = applications.join(DESKTOP_FILE_ID);
    let temporary = applications.join(format!(".{DESKTOP_FILE_ID}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, desktop_entry(&handler)?)?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&temporary, &desktop_path)?;

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
            "xdg-mime is required to register the roblox-player URL handler",
        )
    })?;
    if kde_session() {
        let kwriteconfig = find_on_path("kwriteconfig6").ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                "kwriteconfig6 is required to register the URL handler in this KDE session",
            )
        })?;
        run_checked(
            Command::new(kwriteconfig)
                .arg("--file")
                .arg("mimeapps.list")
                .arg("--group")
                .arg("Default Applications")
                .arg("--key")
                .arg(URL_HANDLER_MIME)
                .arg("--notify")
                .arg(DESKTOP_FILE_ID),
            "set the KDE roblox-player URL handler",
        )?;
        if let Some(cache_builder) = find_on_path("kbuildsycoca6") {
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
                .arg(URL_HANDLER_MIME),
            "set the roblox-player URL handler",
        )?;
    }

    let query = Command::new(xdg_mime)
        .arg("query")
        .arg("default")
        .arg(URL_HANDLER_MIME)
        .output()?;
    if !query.status.success() || String::from_utf8_lossy(&query.stdout).trim() != DESKTOP_FILE_ID {
        return Err(io::Error::other(
            "the desktop environment did not retain Eclipse as the roblox-player handler",
        )
        .into());
    }

    Ok(desktop_path)
}

fn desktop_entry(handler: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let handler = desktop_exec_argument(handler.as_os_str())?;
    Ok(format!(
        "[Desktop Entry]\n\
         Version=1.5\n\
         Type=Application\n\
         Name=Eclipse Roblox Player\n\
         Comment=Launch Roblox experiences through Eclipse\n\
         NoDisplay=true\n\
         Terminal=false\n\
         StartupNotify=false\n\
         Exec={handler} {BROWSER_HANDLER_COMMAND} %u\n\
         MimeType={URL_HANDLER_MIME};\n"
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
    let status = command.status()?;
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
    fn desktop_entry_passes_one_unquoted_url_field() {
        let entry = desktop_entry(Path::new("/work/Eclipse/run-roblox.sh")).unwrap();
        assert!(
            entry.contains("Exec=\"/work/Eclipse/run-roblox.sh\" __handle-roblox-player-url %u\n")
        );
        assert!(!entry.contains("\"%u\""));
        assert!(entry.contains("MimeType=x-scheme-handler/roblox-player;"));
    }

    #[test]
    fn desktop_exec_escapes_spaces_and_literal_percent_signs() {
        assert_eq!(
            desktop_exec_argument(OsStr::new("/work/100% ready/eclipse")).unwrap(),
            "\"/work/100%% ready/eclipse\""
        );
    }
}
