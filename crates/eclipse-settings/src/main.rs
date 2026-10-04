mod failure;
mod install;
mod window;

#[cfg(test)]
mod headless;

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{gio, glib};

use crate::failure::Ending;

const APP_ID: &str = "io.github.kuenec.Eclipse.Settings";

const ECLIPSE_APP_ID: &str = "io.github.kuenec.Eclipse";

const ECLIPSE: &str = "eclipse";

const FAILURE_REPORTS: [(&str, Ending); 2] = [
    ("--failure-report=", Ending::Stopped),
    ("--start-failure-report=", Ending::CouldNotStart),
];

const USAGE: &str =
    "usage: eclipse-settings [--failure-report=REPORT | --start-failure-report=REPORT]";

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Settings,
    FailureReport { report: PathBuf, ending: Ending },
}

fn mode(arguments: &[OsString]) -> Result<Mode, &'static str> {
    match arguments {
        [] => Ok(Mode::Settings),
        [argument] => FAILURE_REPORTS
            .into_iter()
            .find_map(|(option, ending)| {
                let report = argument.as_bytes().strip_prefix(option.as_bytes())?;
                (!report.is_empty()).then(|| Mode::FailureReport {
                    report: PathBuf::from(OsStr::from_bytes(report)),
                    ending,
                })
            })
            .ok_or(USAGE),
        _ => Err(USAGE),
    }
}

fn main() -> glib::ExitCode {
    let mut arguments = std::env::args_os();
    let program: Vec<String> = arguments
        .next()
        .map(|program| program.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    let arguments: Vec<OsString> = arguments.collect();
    match mode(&arguments) {
        Ok(Mode::Settings) => settings(&program),
        Ok(Mode::FailureReport { report, ending }) => failure_report(&program, report, ending),
        Err(usage) => {
            eprintln!("{usage}");
            glib::ExitCode::FAILURE
        }
    }
}

fn failure_report(program: &[String], path: PathBuf, ending: Ending) -> glib::ExitCode {
    let report = match failure::read(&path) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("eclipse-settings: {error}");
            return glib::ExitCode::FAILURE;
        }
    };
    let app = adw::Application::builder()
        .application_id(ECLIPSE_APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        failure::open(app, &report, &path, ending);
    });
    app.run_with_args(program)
}

fn settings(program: &[String]) -> glib::ExitCode {
    let Some(config) = eclipse_config::config_path() else {
        eprintln!("eclipse-settings: cannot find Eclipse's config directory; is $HOME set?");
        return glib::ExitCode::FAILURE;
    };
    let eclipse = match std::env::current_exe() {
        Ok(executable) => executable.with_file_name(ECLIPSE),
        Err(error) => {
            eprintln!("eclipse-settings: cannot find its own executable: {error}");
            return glib::ExitCode::FAILURE;
        }
    };
    let failed = Rc::new(Cell::new(false));
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate({
        let failed = Rc::clone(&failed);
        move |app| {
            if let Some(window) = app.active_window() {
                window.present();
                return;
            }
            if let Err(error) = window::open(app, config.clone(), eclipse.clone()) {
                eprintln!("eclipse-settings: {error}");
                failed.set(true);
                app.quit();
            }
        }
    });
    let status = app.run_with_args(program);
    if failed.get() {
        return glib::ExitCode::FAILURE;
    }
    status
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_settings_window_takes_no_arguments_and_the_failure_window_takes_one_report() {
        let parse =
            |arguments: &[&str]| mode(&arguments.iter().map(OsString::from).collect::<Vec<_>>());

        assert_eq!(parse(&[]), Ok(Mode::Settings));
        assert_eq!(
            parse(&["--failure-report=/logs/eclipse-20261003T091434.289Z.report.txt"]),
            Ok(Mode::FailureReport {
                report: PathBuf::from("/logs/eclipse-20261003T091434.289Z.report.txt"),
                ending: Ending::Stopped,
            })
        );
        assert_eq!(
            parse(&["--start-failure-report=/logs/eclipse-20261003T091434.289Z.report.txt"]),
            Ok(Mode::FailureReport {
                report: PathBuf::from("/logs/eclipse-20261003T091434.289Z.report.txt"),
                ending: Ending::CouldNotStart,
            })
        );
        assert_eq!(
            mode(&[OsString::from(OsStr::from_bytes(
                b"--failure-report=/l\xffgs/r.txt"
            ))]),
            Ok(Mode::FailureReport {
                report: PathBuf::from(OsStr::from_bytes(b"/l\xffgs/r.txt")),
                ending: Ending::Stopped,
            })
        );
        for refused in [
            &["--failure-report="][..],
            &["--start-failure-report="],
            &["--failure-report", "/logs/r.txt"],
            &[
                "--failure-report=/logs/a.txt",
                "--failure-report=/logs/b.txt",
            ],
            &["--failure-report=/logs/r.txt", "--gapplication-service"],
            &["--help"],
            &["/logs/r.txt"],
        ] {
            assert_eq!(parse(refused), Err(USAGE), "{refused:?}");
        }
    }

    #[test]
    fn the_desktop_file_named_after_the_app_id_opens_settings_through_eclipse() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/flatpak")
            .join(format!("{APP_ID}.desktop"));

        let entry = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

        assert!(entry.contains("\nExec=eclipse settings\n"), "{entry}");
        assert!(
            entry.contains("\nIcon=io.github.kuenec.Eclipse\n"),
            "{entry}"
        );
    }
}
