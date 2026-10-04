#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const RUN_LIMIT: Duration = Duration::from_secs(60);
const SECTIONS: [&str; 10] = [
    "Eclipse",
    "System",
    "CPU",
    "Graphics",
    "Storage",
    "Roblox",
    "Config",
    "Roblox links",
    "Logs",
    "Problems",
];
const NOT_INSTALLED: &str = "  - Roblox is not installed.";
const NOT_INSTALLED_FIX: &str =
    "Problem: Roblox is not installed. Start Eclipse to download it, or run `eclipse update`.\n";
const RUN: &str = "eclipse-20261003T091434.289Z";
const SECRET: &str = "9F1C2B3A4D5E6F708192A3B4C5D6E7F8";
const TESTER: &str = "eclipse-tester";

fn sandbox(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("eclipse-doctor-{tag}"));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(&root).expect("create the sandbox directory");
    root
}

fn doctor(root: &Path, arguments: &[&str]) -> Output {
    bounded_child::output(
        Command::new(env!("CARGO_BIN_EXE_eclipse"))
            .arg("doctor")
            .args(arguments)
            .env("HOME", root)
            .env("USER", TESTER)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("ECLIPSE_APP_DATA_DIR", root.join("app-data"))
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET")
            .env_remove("DISPLAY"),
        RUN_LIMIT,
    )
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn summary(report: &str) -> (&str, &str) {
    report
        .split_once("\n\n```text\nEclipse\n")
        .unwrap_or_else(|| panic!("no summary paragraph before the check in {report}"))
}

fn assert_sections(stdout: &str) {
    for section in SECTIONS {
        assert!(
            stdout.lines().any(|line| line == section),
            "{section} in {stdout}"
        );
    }
}

#[test]
fn doctor_checks_every_section_without_an_install_or_a_display() {
    let root = sandbox("no-install");

    let check = doctor(&root, &[]);
    let report = doctor(&root, &["--report"]);
    let created: Vec<bool> = ["app-data", "data", "config"]
        .iter()
        .map(|dir| root.join(dir).exists())
        .collect();
    std::fs::remove_dir_all(&root).ok();

    let stdout = text(&check.stdout);
    assert_eq!(check.status.code(), Some(0), "{}", text(&check.stderr));
    assert_sections(&stdout);
    assert!(stdout.contains(NOT_INSTALLED), "{stdout}");
    assert!(
        stdout.contains("  Eclipse's windows: none, because neither WAYLAND_DISPLAY nor DISPLAY"),
        "{stdout}"
    );
    assert!(stdout.contains("  Kept runs: none\n"), "{stdout}");

    let stdout = text(&report.stdout);
    assert_eq!(report.status.code(), Some(0), "{}", text(&report.stderr));
    let (summary, _) = summary(&stdout);
    assert!(
        summary.starts_with("Outcome: no launch is logged yet\nProblem: "),
        "{summary}"
    );
    assert!(summary.contains(NOT_INSTALLED_FIX), "{summary}");
    assert!(
        summary.ends_with("\nLog: none in ~/app-data/logs"),
        "{summary}"
    );
    assert_sections(&stdout);
    assert!(stdout.contains(NOT_INSTALLED), "{stdout}");
    assert!(stdout.ends_with("Log excerpt: none, because no launch is logged yet\n```\n"));
    assert_eq!(created, [false, false, false], "doctor changes nothing");
}

#[test]
fn the_report_is_one_redacted_paste_of_the_newest_run() {
    let root = sandbox("report");
    let logs = root.join("app-data").join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let head = logs.join(format!("{RUN}.log"));
    let records = [
        "2026-10-03T09:14:34.300000Z  INFO eclipse::status: Starting Roblox".to_owned(),
        format!(
            "2026-10-03T09:14:40.100000Z  INFO android.util.Log: cookie \
             .ROBLOSECURITY=_|WARNING:-DO-NOT-SHARE-THIS.--Sharing-this-will-allow-someone-to-\
             log-in-as-you-and-to-steal-your-ROBUX-and-items.|_{SECRET}; placeId 1818"
        ),
        format!(
            "2026-10-03T09:14:41.200000Z  INFO stdout: saved /run/media/{TESTER}/games/a.txt \
             and {}/notes.txt",
            root.display()
        ),
        "2026-10-03T09:14:42.300000Z ERROR eclipse::status: the engine stopped".to_owned(),
        "2026-10-03T09:14:43.400000Z ERROR eclipse::supervisor: Roblox crashed (signal 11, \
         SIGSEGV: invalid memory access)\nLast error: the engine stopped"
            .to_owned(),
    ];
    std::fs::write(&head, records.join("\n") + "\n").unwrap();

    let report = doctor(&root, &["--report"]);
    let named = doctor(&root, &["--report", &head.display().to_string()]);
    std::fs::remove_dir_all(&root).ok();

    let stdout = text(&report.stdout);
    assert_eq!(report.status.code(), Some(0), "{}", text(&report.stderr));
    let (summary, _) = summary(&stdout);
    assert!(
        summary.starts_with(
            "Outcome: Roblox crashed (signal 11, SIGSEGV: invalid memory access)\nLast error: \
             the engine stopped\nProblem: "
        ),
        "{summary}"
    );
    assert!(summary.contains(NOT_INSTALLED_FIX), "{summary}");
    assert!(
        summary.ends_with(&format!("\nLog: ~/app-data/logs/{RUN}.log")),
        "{summary}"
    );
    assert_sections(&stdout);
    assert!(stdout.contains("Log excerpt: 5 of 5 records\n"), "{stdout}");
    assert!(
        stdout.contains(" INFO android.util.Log: cookie .ROBLOSECURITY=<redacted>; placeId 1818\n")
    );
    assert!(
        stdout.contains("saved /run/media/<user>/games/a.txt and ~/notes.txt\n"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("Last error: the engine stopped\n```\n"),
        "{stdout}"
    );
    assert!(!stdout.contains(SECRET), "{stdout}");
    assert!(!stdout.contains(&root.display().to_string()), "{stdout}");
    assert!(!stdout.contains(TESTER), "{stdout}");
    assert!(stdout.len() <= 60_000);
    let named = text(&named.stdout);
    let intro = |report: &str| report.split_once("\n\n").map(|(intro, _)| intro.to_owned());
    let excerpt = |report: &str| {
        report
            .split_once("Log excerpt:")
            .map(|(_, rest)| rest.to_owned())
    };
    assert_eq!(intro(&named), intro(&stdout), "{named}");
    assert_eq!(excerpt(&named), excerpt(&stdout), "{named}");
}

#[test]
fn a_report_names_only_run_logs_in_the_log_directory() {
    let root = sandbox("outside");
    let outside = root.join(format!("{RUN}.log"));
    std::fs::write(&outside, b"not in the log directory\n").unwrap();

    let output = doctor(&root, &["--report", &outside.display().to_string()]);
    std::fs::remove_dir_all(&root).ok();

    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.starts_with("eclipse doctor: ")
            && stderr.contains("is not one of Eclipse's run logs in"),
        "{stderr}"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn a_long_run_fits_one_issue_body() {
    let root = sandbox("long-run");
    let logs = root.join("app-data").join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let padding = "x".repeat(600);
    let mut log: String = (0..5_000)
        .map(|index| {
            format!("2026-10-03T09:14:34.{index:06}Z ERROR liblog: record {index} {padding}\n")
        })
        .collect();
    log.push_str(
        "2026-10-03T09:14:35.000000Z ERROR eclipse::supervisor: Roblox crashed (signal 6)\n",
    );
    std::fs::write(logs.join(format!("{RUN}.log")), log).unwrap();

    let report = doctor(&root, &["--report"]);
    std::fs::remove_dir_all(&root).ok();

    let stdout = text(&report.stdout);
    assert_eq!(report.status.code(), Some(0), "{}", text(&report.stderr));
    assert!(stdout.len() <= 60_000, "{} bytes", stdout.len());
    let (_, excerpt) = stdout.split_once("Log excerpt: ").unwrap();
    let (kept, _) = excerpt.split_once(" of 5001 records\n").unwrap();
    let printed = excerpt
        .lines()
        .filter(|line| line.starts_with("2026-"))
        .count();
    assert_eq!(kept.parse::<usize>().unwrap(), printed, "{stdout}");
    assert!(printed > 100, "{printed} records in {} bytes", stdout.len());
    assert!(
        stdout.contains(" ERROR liblog: record 4999 xxx"),
        "{stdout}"
    );
    assert!(!stdout.contains(" ERROR liblog: record 0 "), "{stdout}");
    assert!(stdout.ends_with(" ERROR eclipse::supervisor: Roblox crashed (signal 6)\n```\n"));
}

fn config_problem(root: &Path) -> Option<String> {
    let output = doctor(root, &[]);
    text(&output.stdout)
        .lines()
        .find(|line| line.starts_with("  - The settings file has problems"))
        .map(str::to_owned)
}

#[test]
fn a_config_problem_says_whether_settings_can_fix_it() {
    let root = sandbox("config-repair");
    let config = root.join("config").join("eclipse").join("config.json");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(&config, "{\"touch_mode\": \"on\",}").unwrap();
    let syntax = config_problem(&root);
    std::fs::write(&config, "{\"touch_mode\": 5}").unwrap();
    let value = config_problem(&root);
    let managed = root.join("dotfiles.json");
    std::fs::rename(&config, &managed).unwrap();
    std::os::unix::fs::symlink(&managed, &config).unwrap();
    let linked = config_problem(&root);
    std::fs::remove_dir_all(&root).ok();

    let line = |fix: &str| {
        Some(format!(
            "  - The settings file has problems, listed under Config. {fix}"
        ))
    };
    assert_eq!(
        syntax,
        line(
            "Settings and `eclipse config set` cannot change the file until it is fixed by hand \
             where each problem points; until then Eclipse uses the defaults they name."
        )
    );
    assert_eq!(
        value,
        line(
            "Eclipse uses the defaults for those settings until they are fixed in Settings or \
             with `eclipse config set`."
        )
    );
    assert_eq!(
        linked,
        line(
            "The file is managed outside Eclipse, so fix them where it is managed; until then \
             Eclipse uses the defaults they name."
        )
    );
}
