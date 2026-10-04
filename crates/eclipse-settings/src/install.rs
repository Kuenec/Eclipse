use std::collections::VecDeque;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use gtk4::prelude::*;
use gtk4::{gio, glib};

pub(crate) const SUFFIXES: [&str; 4] = ["apk", "apks", "xapk", "apkm"];

const KEPT_LINES: usize = 12;

const STEP_PREFIX: &str = "# ";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Installed,
    Failed { output: Vec<String> },
}

pub(crate) async fn run(
    eclipse: &Path,
    files: &[PathBuf],
    mut progress: impl FnMut(&str),
) -> Result<Outcome, glib::Error> {
    let mut argv = vec![eclipse.as_os_str(), OsStr::new("install")];
    argv.extend(files.iter().map(|file| file.as_os_str()));
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    )?;
    let output = gio::DataInputStream::new(
        &process
            .stdout_pipe()
            .expect("STDOUT_PIPE gives the process a stdout pipe"),
    );
    let mut kept = VecDeque::with_capacity(KEPT_LINES);
    while let Some(line) = output.read_line_future(glib::Priority::DEFAULT).await? {
        let line = String::from_utf8_lossy(&line);
        let line = line.strip_prefix(STEP_PREFIX).unwrap_or(&line);
        progress(line);
        if kept.len() == KEPT_LINES {
            kept.pop_front();
        }
        kept.push_back(line.to_owned());
    }
    process.wait_future().await?;
    if process.is_successful() {
        return Ok(Outcome::Installed);
    }
    if kept.is_empty() {
        kept.push_back(format!(
            "{} install stopped without output ({})",
            eclipse.display(),
            ending(&process)
        ));
    }
    Ok(Outcome::Failed {
        output: kept.into(),
    })
}

fn ending(process: &gio::Subprocess) -> String {
    if process.has_exited() {
        format!("exit status {}", process.exit_status())
    } else {
        format!("signal {}", process.term_sig())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::stub_script;

    fn stub(tag: &str, script: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("eclipse-settings-install-{tag}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the stub directory");
        let eclipse = dir.join("eclipse");
        stub_script::write(&eclipse, script);
        (dir, eclipse)
    }

    fn install(eclipse: &Path, files: &[PathBuf]) -> (Result<Outcome, glib::Error>, Vec<String>) {
        let mut shown = Vec::new();
        let outcome = glib::MainContext::new()
            .block_on(run(eclipse, files, |line| shown.push(line.to_owned())));
        (outcome, shown)
    }

    #[test]
    fn each_file_is_its_own_argument_and_every_line_is_shown() {
        let (dir, eclipse) = stub(
            "arguments",
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"${0%/*}/arguments\"\n\
             echo '# Verifying and installing the Roblox client…'\n\
             echo 'installed Roblox 2.700.1'\n",
        );
        let files = [dir.join("base one.apk"), dir.join("split; two.apkm")];

        let (outcome, shown) = install(&eclipse, &files);

        let arguments = fs::read_to_string(dir.join("arguments")).expect("the stub ran");
        fs::remove_dir_all(&dir).ok();
        assert_eq!(outcome.expect("the stub ran"), Outcome::Installed);
        assert_eq!(
            arguments,
            format!("install\n{}\n{}\n", files[0].display(), files[1].display())
        );
        assert_eq!(
            shown,
            [
                "Verifying and installing the Roblox client…",
                "installed Roblox 2.700.1"
            ]
        );
    }

    #[test]
    fn a_failure_keeps_the_last_lines_of_both_streams() {
        let (dir, eclipse) = stub(
            "failure",
            "#!/bin/sh\n\
             i=0\n\
             while [ $i -lt 20 ]; do echo \"line $i\"; i=$((i + 1)); done\n\
             echo 'not signed by Roblox' >&2\n\
             exit 1\n",
        );

        let (outcome, shown) = install(&eclipse, &[dir.join("base.apk")]);

        fs::remove_dir_all(&dir).ok();
        let Outcome::Failed { output } = outcome.expect("the stub ran") else {
            panic!("a failed install is reported");
        };
        assert_eq!(shown.len(), 21);
        assert_eq!(output.len(), KEPT_LINES);
        assert_eq!(output.first().map(String::as_str), Some("line 9"));
        assert_eq!(
            output.last().map(String::as_str),
            Some("not signed by Roblox")
        );
    }

    #[test]
    fn a_silent_failure_names_how_the_command_ended() {
        let (dir, eclipse) = stub("silent", "#!/bin/sh\nexit 3\n");

        let (outcome, shown) = install(&eclipse, &[dir.join("base.apk")]);

        fs::remove_dir_all(&dir).ok();
        assert!(shown.is_empty());
        assert_eq!(
            outcome.expect("the stub ran"),
            Outcome::Failed {
                output: vec![format!(
                    "{} install stopped without output (exit status 3)",
                    eclipse.display()
                )]
            }
        );
    }

    #[test]
    fn a_missing_command_is_an_error() {
        let missing = std::env::temp_dir().join("eclipse-settings-install-missing/eclipse");

        let (outcome, shown) = install(&missing, &[]);

        assert!(shown.is_empty());
        assert!(outcome.is_err());
    }
}
