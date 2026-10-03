use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use directories::ProjectDirs;

use crate::document::{self, Document, Malformed, Position};
use crate::{shell, Config, Key};

const NIX_STORE: &str = "/nix/store";

const EVERY_DEFAULT: &str = "Eclipse uses the default for every setting";

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub path: Option<PathBuf>,
    pub config: Config,
    pub problems: Vec<Problem>,
    pub unused_keys: Vec<String>,
}

impl Loaded {
    #[must_use]
    pub fn unused_keys_message(&self) -> Option<String> {
        let path = self.path.as_ref()?;
        if self.unused_keys.is_empty() {
            return None;
        }
        let keys: Vec<String> = self
            .unused_keys
            .iter()
            .map(|key| format!("{key:?}"))
            .collect();
        Some(format!(
            "{}: Eclipse does not use these keys: {}",
            path.display(),
            keys.join(", ")
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    NoConfigDir,

    Unreadable {
        path: PathBuf,
        reason: String,
    },

    DanglingLink {
        link: PathBuf,
        target: PathBuf,
        flatpak_id: Option<String>,
    },

    NotUtf8 {
        path: PathBuf,
        at: Position,
    },

    Syntax {
        path: PathBuf,
        at: Position,
        reason: String,
    },

    NotAnObject {
        path: PathBuf,
    },

    DuplicateKey {
        path: PathBuf,
        key: String,
        at: Position,
    },

    InvalidValue {
        path: PathBuf,
        key: String,
        at: Position,
        reason: String,
    },
}

impl Problem {
    fn malformed(path: &Path, malformed: Malformed) -> Self {
        let path = path.to_owned();
        match malformed {
            Malformed::NotUtf8(at) => Self::NotUtf8 { path, at },
            Malformed::Syntax { at, reason } => Self::Syntax { path, at, reason },
            Malformed::NotAnObject => Self::NotAnObject { path },
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigDir => write!(
                f,
                "cannot find Eclipse's config directory (is $HOME set?); {EVERY_DEFAULT}"
            ),
            Self::Unreadable { path, reason } => {
                write!(
                    f,
                    "{}: cannot read it: {reason}; {EVERY_DEFAULT}",
                    path.display()
                )
            }
            Self::DanglingLink {
                link,
                target,
                flatpak_id: None,
            } => write!(
                f,
                "{}: links to {}, which does not exist; {EVERY_DEFAULT}",
                link.display(),
                target.display()
            ),
            Self::DanglingLink {
                link,
                target,
                flatpak_id: Some(flatpak_id),
            } => write!(
                f,
                "{}: links to {}, which does not exist or is outside Eclipse's sandbox; \
                 {EVERY_DEFAULT}; to let Eclipse read it, run \
                 `flatpak override --user {} {}`",
                link.display(),
                target.display(),
                shell::word(&format!(
                    "--filesystem={}:ro",
                    sandbox_share(target).display()
                )),
                shell::word(flatpak_id)
            ),
            Self::NotUtf8 { path, at } => {
                write!(
                    f,
                    "{}:{at}: not UTF-8 text; {EVERY_DEFAULT}",
                    path.display()
                )
            }
            Self::Syntax { path, at, reason } => {
                write!(
                    f,
                    "{}:{at}: invalid JSON: {reason}; {EVERY_DEFAULT}",
                    path.display()
                )
            }
            Self::NotAnObject { path } => write!(
                f,
                "{}: the settings must be one JSON object; {EVERY_DEFAULT}",
                path.display()
            ),
            Self::DuplicateKey { path, key, at } => {
                write!(f, "{}:{at}: {key}: appears more than once", path.display())?;
                default_clause(f, key)
            }
            Self::InvalidValue {
                path,
                key,
                at,
                reason,
            } => {
                write!(f, "{}:{at}: {key}: {reason}", path.display())?;
                default_clause(f, key)
            }
        }
    }
}

fn default_clause(f: &mut fmt::Formatter<'_>, key: &str) -> fmt::Result {
    match Key::from_name(key) {
        Some(key) => write!(f, "; Eclipse uses the default ({})", key.default_json()),
        None => Ok(()),
    }
}

fn sandbox_share(target: &Path) -> &Path {
    if target.starts_with(NIX_STORE) {
        return Path::new(NIX_STORE);
    }
    target.parent().unwrap_or(target)
}

fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "eclipse").map(|dirs| dirs.config_dir().join("config.json"))
}

#[must_use]
pub fn load() -> Loaded {
    match config_path() {
        Some(path) => load_from(&path),
        None => Loaded {
            path: None,
            config: Config::default(),
            problems: vec![Problem::NoConfigDir],
            unused_keys: Vec::new(),
        },
    }
}

#[must_use]
pub fn load_from(path: &Path) -> Loaded {
    load_in(path, std::env::var("FLATPAK_ID").ok())
}

fn load_in(path: &Path, flatpak_id: Option<String>) -> Loaded {
    let mut loaded = Loaded {
        path: Some(path.to_owned()),
        config: Config::default(),
        problems: Vec::new(),
        unused_keys: Vec::new(),
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some((link, target)) = dangling_link(path) {
                loaded.problems.push(Problem::DanglingLink {
                    link,
                    target,
                    flatpak_id,
                });
            }
            return loaded;
        }
        Err(error) => {
            loaded.problems.push(Problem::Unreadable {
                path: path.to_owned(),
                reason: error.to_string(),
            });
            return loaded;
        }
    };
    match Document::parse(&bytes) {
        Ok(document) => read_entries(&document, path, &mut loaded),
        Err(malformed) => loaded.problems.push(Problem::malformed(path, malformed)),
    }
    loaded
}

fn dangling_link(path: &Path) -> Option<(PathBuf, PathBuf)> {
    for candidate in path.ancestors() {
        let metadata = match fs::symlink_metadata(candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        if !metadata.is_symlink() || !matches!(candidate.try_exists(), Ok(false)) {
            return None;
        }
        let directory = candidate
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let target = directory
            .canonicalize()
            .ok()?
            .join(fs::read_link(candidate).ok()?);
        return Some((candidate.to_owned(), without_dot_components(&target)));
    }
    None
}

fn without_dot_components(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                resolved.push(component);
            }
        }
    }
    resolved
}

fn read_entries(document: &Document<'_>, path: &Path, loaded: &mut Loaded) {
    let mut seen = HashSet::new();
    let repeated: HashSet<&str> = document
        .entries
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !seen.insert(*key))
        .collect();
    seen.clear();
    for (key, value) in &document.entries {
        if !seen.insert(key.as_str()) {
            loaded.problems.push(Problem::DuplicateKey {
                path: path.to_owned(),
                key: key.clone(),
                at: document.position(value),
            });
            continue;
        }
        let Some(known) = Key::from_name(key) else {
            loaded.unused_keys.push(key.clone());
            continue;
        };
        if repeated.contains(key.as_str()) {
            continue;
        }
        let converted = serde_json::from_str(value.get())
            .map_err(|error| document::reason(&error))
            .and_then(|value| loaded.config.set_value(known, value));
        if let Err(reason) = converted {
            loaded.problems.push(Problem::InvalidValue {
                path: path.to_owned(),
                key: key.clone(),
                at: document.position(value),
                reason,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::{GraphicsOptimizationMode, TouchMode};

    const FLATPAK_ID: &str = "io.github.kuenec.Eclipse";

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eclipse-config-load-{tag}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the sandbox directory");
        dir
    }

    fn load_bytes(tag: &str, bytes: &[u8]) -> (PathBuf, Loaded) {
        let dir = sandbox(tag);
        let path = dir.join("config.json");
        fs::write(&path, bytes).expect("write config.json");
        let loaded = load_in(&path, None);
        fs::remove_dir_all(&dir).ok();
        (path, loaded)
    }

    fn at(line: usize, column: usize) -> Position {
        Position { line, column }
    }

    #[test]
    fn a_bad_value_keeps_its_default_and_the_other_keys_apply() {
        let (path, loaded) = load_bytes(
            "bad-value",
            br#"{"touch_mode": 5, "graphics_optimization_mode": "performance"}"#,
        );
        assert_eq!(
            loaded.config,
            Config {
                graphics_optimization_mode: GraphicsOptimizationMode::Performance,
                ..Config::default()
            }
        );
        let [problem] = loaded.problems.as_slice() else {
            panic!("expected one problem, got {:?}", loaded.problems);
        };
        let Problem::InvalidValue {
            path: reported,
            key,
            at: position,
            reason,
        } = problem
        else {
            panic!("expected InvalidValue, got {problem:?}");
        };
        assert_eq!(
            (reported, key.as_str(), *position),
            (&path, "touch_mode", at(1, 16))
        );
        assert_eq!(reason, "expected one of `off`, `on`, `fake-off`");
        assert_eq!(
            problem.to_string(),
            format!(
                "{}:1:16: touch_mode: {reason}; Eclipse uses the default (\"off\")",
                path.display()
            )
        );
    }

    #[test]
    fn every_bad_value_is_reported_in_one_pass() {
        let (_, loaded) = load_bytes(
            "bad-values",
            br#"{"touch_mode": "sideways", "graphics_optimization_mode": 3}"#,
        );
        assert_eq!(loaded.config, Config::default());
        let reported: Vec<(&str, Position)> = loaded
            .problems
            .iter()
            .map(|problem| match problem {
                Problem::InvalidValue { key, at, .. } => (key.as_str(), *at),
                other => panic!("expected InvalidValue, got {other:?}"),
            })
            .collect();
        assert_eq!(
            reported,
            [
                ("touch_mode", at(1, 16)),
                ("graphics_optimization_mode", at(1, 58))
            ]
        );
        assert!(loaded.problems[0].to_string().contains("`fake-off`"));
        assert!(loaded.problems[1]
            .to_string()
            .contains("expected one of `quality`, `balanced`, `performance`"));
    }

    #[test]
    fn a_value_that_is_not_a_string_lists_the_accepted_values() {
        for (tag, text) in [
            ("integer", r#"{"touch_mode": 5}"#),
            ("boolean", r#"{"touch_mode": true}"#),
            ("map", r#"{"touch_mode": {"on": null}}"#),
        ] {
            let (path, loaded) = load_bytes(tag, text.as_bytes());
            assert_eq!(loaded.config, Config::default(), "{tag}");
            assert_eq!(
                loaded
                    .problems
                    .iter()
                    .map(Problem::to_string)
                    .collect::<Vec<_>>(),
                [format!(
                    "{}:1:16: touch_mode: expected one of `off`, `on`, `fake-off`; Eclipse uses \
                     the default (\"off\")",
                    path.display()
                )],
                "{tag}"
            );
        }
    }

    #[test]
    fn a_value_on_a_later_line_is_located_in_the_file() {
        let (_, loaded) = load_bytes(
            "later-line",
            b"{\n  \"touch_mode\": \"on\",\n  \"fflags\": [1e400]\n}\n",
        );
        assert_eq!(loaded.config.touch_mode, TouchMode::On);
        let [Problem::InvalidValue {
            key,
            at: position,
            reason,
            ..
        }] = loaded.problems.as_slice()
        else {
            panic!("expected one InvalidValue, got {:?}", loaded.problems);
        };
        assert_eq!((key.as_str(), *position), ("fflags", at(3, 13)));
        assert!(reason.contains("out of range"), "{reason}");
        assert!(!reason.contains("line"), "{reason}");
    }

    #[test]
    fn a_syntax_error_keeps_every_default_and_uses_the_parser_position() {
        let text = r#"{"touch_mode": "on", "graphics_optimization_mode": "#;
        let (path, loaded) = load_bytes("syntax", text.as_bytes());
        let expected = serde_json::from_str::<serde_json::Value>(text).expect_err("truncated");
        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.problems,
            [Problem::Syntax {
                path: path.clone(),
                at: at(expected.line(), expected.column()),
                reason: "EOF while parsing a value".to_owned(),
            }]
        );
        assert_eq!(
            loaded.problems[0].to_string(),
            format!(
                "{}:{}:{}: invalid JSON: EOF while parsing a value; Eclipse uses the default \
                 for every setting",
                path.display(),
                expected.line(),
                expected.column()
            )
        );
    }

    #[test]
    fn an_empty_or_blank_file_is_located_at_a_column_editors_accept() {
        for (tag, text, position) in [("empty", "", at(1, 1)), ("blank", " \n\n", at(3, 1))] {
            let (path, loaded) = load_bytes(tag, text.as_bytes());
            assert_eq!(loaded.config, Config::default(), "{tag}");
            assert_eq!(
                loaded.problems,
                [Problem::Syntax {
                    path,
                    at: position,
                    reason: "EOF while parsing a value".to_owned(),
                }],
                "{tag}"
            );
        }
    }

    #[test]
    fn a_top_level_that_is_not_an_object_keeps_every_default() {
        for (tag, text) in [("array", "[]"), ("string", r#""on""#), ("null", "null")] {
            let (path, loaded) = load_bytes(tag, text.as_bytes());
            assert_eq!(loaded.config, Config::default(), "{tag}");
            assert_eq!(loaded.problems, [Problem::NotAnObject { path }], "{tag}");
        }
    }

    #[test]
    fn text_that_is_not_utf8_is_located_at_its_first_invalid_byte() {
        let (path, loaded) = load_bytes("not-utf8", b"{\"a\": \"\xff\"}");
        assert_eq!(loaded.problems, [Problem::NotUtf8 { path, at: at(1, 8) }]);
        let (_, loaded) = load_bytes("not-utf8-later", b"{\n  \"a\": \"\xff\"}");
        assert!(matches!(
            loaded.problems.as_slice(),
            [Problem::NotUtf8 { at: position, .. }] if *position == at(2, 9)
        ));
    }

    #[test]
    fn a_repeated_key_keeps_its_default_and_is_reported_where_it_repeats() {
        let (path, loaded) = load_bytes(
            "repeated",
            br#"{"touch_mode": "on", "graphics_optimization_mode": "performance", "touch_mode": "fake-off", "x": 1, "x": 2}"#,
        );
        assert_eq!(
            loaded.config,
            Config {
                graphics_optimization_mode: GraphicsOptimizationMode::Performance,
                ..Config::default()
            }
        );
        assert_eq!(loaded.unused_keys, ["x"]);
        assert_eq!(
            loaded.problems,
            [
                Problem::DuplicateKey {
                    path: path.clone(),
                    key: "touch_mode".to_owned(),
                    at: at(1, 81),
                },
                Problem::DuplicateKey {
                    path: path.clone(),
                    key: "x".to_owned(),
                    at: at(1, 106),
                },
            ]
        );
        assert_eq!(
            loaded.problems[0].to_string(),
            format!(
                "{}:1:81: touch_mode: appears more than once; Eclipse uses the default (\"off\")",
                path.display()
            )
        );
        assert_eq!(
            loaded.problems[1].to_string(),
            format!("{}:1:106: x: appears more than once", path.display())
        );
    }

    #[test]
    fn a_dangling_link_into_the_nix_store_names_the_flatpak_override() {
        let dir = sandbox("dangling-nix");
        let path = dir.join("config.json");
        let target = PathBuf::from("/nix/store/x-hm/config.json");
        symlink(&target, &path).expect("create the link");
        let loaded = load_in(&path, Some(FLATPAK_ID.to_owned()));
        fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.config, Config::default());
        assert_eq!(
            loaded.problems,
            [Problem::DanglingLink {
                link: path,
                target,
                flatpak_id: Some(FLATPAK_ID.to_owned()),
            }]
        );
        let message = loaded.problems[0].to_string();
        assert!(
            message.contains(
                "flatpak override --user --filesystem=/nix/store:ro io.github.kuenec.Eclipse"
            ),
            "{message}"
        );
    }

    #[test]
    fn a_dangling_link_elsewhere_names_the_directory_of_its_target() {
        let dir = sandbox("dangling-elsewhere");
        let path = dir.join("config.json");
        let gone = dir.join("gone");
        symlink(gone.join("missing.json"), &path).expect("create the link");
        let shared = load_in(&path, Some(FLATPAK_ID.to_owned()));
        let host = load_in(&path, None);
        fs::remove_dir_all(&dir).ok();

        let message = shared.problems[0].to_string();
        let expected = format!(
            "flatpak override --user --filesystem={}:ro {FLATPAK_ID}",
            gone.display()
        );
        assert!(message.contains(&expected), "{message}");
        assert_eq!(
            host.problems[0].to_string(),
            format!(
                "{}: links to {}, which does not exist; Eclipse uses the default for every \
                 setting",
                path.display(),
                gone.join("missing.json").display()
            )
        );
    }

    #[test]
    fn the_flatpak_override_quotes_a_directory_the_shell_would_split() {
        let dir = sandbox("dangling-spaced");
        let path = dir.join("config.json");
        symlink(dir.join("Kue's dots").join("gone.json"), &path).expect("create the link");
        let loaded = load_in(&path, Some(FLATPAK_ID.to_owned()));
        fs::remove_dir_all(&dir).ok();

        let message = loaded.problems[0].to_string();
        let expected = format!(
            r"`flatpak override --user '--filesystem={}/Kue'\''s dots:ro' {FLATPAK_ID}`",
            dir.display()
        );
        assert!(message.contains(&expected), "{message}");
    }

    #[test]
    fn a_relative_dangling_link_is_resolved_from_its_directory() {
        let dir = sandbox("dangling-relative");
        let path = dir.join("config.json");
        symlink("missing.json", &path).expect("create the link");
        let loaded = load_in(&path, None);
        let real_dir = dir.canonicalize().expect("resolve the sandbox");
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            loaded.problems,
            [Problem::DanglingLink {
                link: path,
                target: real_dir.join("missing.json"),
                flatpak_id: None,
            }]
        );
    }

    #[test]
    fn a_stow_link_out_of_the_config_directory_names_a_path_flatpak_accepts() {
        let dir = sandbox("dangling-stow");
        let config_dir = dir.join("home").join(".config").join("eclipse");
        fs::create_dir_all(&config_dir).expect("create the config directory");
        let path = config_dir.join("config.json");
        symlink("../../dotfiles/eclipse/config.json", &path).expect("create the link");
        let loaded = load_in(&path, Some(FLATPAK_ID.to_owned()));
        let home = dir
            .join("home")
            .canonicalize()
            .expect("resolve the sandbox");
        fs::remove_dir_all(&dir).ok();

        let target = home.join("dotfiles").join("eclipse").join("config.json");
        assert_eq!(
            loaded.problems,
            [Problem::DanglingLink {
                link: path,
                target,
                flatpak_id: Some(FLATPAK_ID.to_owned()),
            }]
        );
        let message = loaded.problems[0].to_string();
        let expected = format!(
            "flatpak override --user --filesystem={}:ro {FLATPAK_ID}",
            home.join("dotfiles").join("eclipse").display()
        );
        assert!(message.contains(&expected), "{message}");
        assert!(!message.contains(".."), "{message}");
    }

    #[test]
    fn a_relative_link_target_climbs_from_where_its_directory_really_is() {
        let dir = sandbox("dangling-moved-config");
        let real_config = dir.join("dots").join("config");
        fs::create_dir_all(real_config.join("eclipse")).expect("create the real config");
        fs::create_dir_all(dir.join("home")).expect("create the home directory");
        let linked_config = dir.join("home").join(".config");
        symlink(&real_config, &linked_config).expect("link the config directory");
        let path = linked_config.join("eclipse").join("config.json");
        symlink("../../dotfiles/config.json", &path).expect("create the link");
        let loaded = load_in(&path, None);
        let dots = dir
            .join("dots")
            .canonicalize()
            .expect("resolve the sandbox");
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            loaded.problems,
            [Problem::DanglingLink {
                link: path,
                target: dots.join("dotfiles").join("config.json"),
                flatpak_id: None,
            }]
        );
    }

    #[test]
    fn a_dangling_link_in_place_of_the_config_directory_is_reported() {
        let dir = sandbox("dangling-directory");
        let config_dir = dir.join("eclipse");
        symlink(dir.join("gone"), &config_dir).expect("create the link");
        let loaded = load_in(&config_dir.join("config.json"), None);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            loaded.problems,
            [Problem::DanglingLink {
                link: config_dir,
                target: dir.join("gone"),
                flatpak_id: None,
            }]
        );
    }

    #[test]
    fn a_missing_file_gives_defaults_and_no_problem() {
        let dir = sandbox("missing");
        let missing_file = load_in(&dir.join("config.json"), None);
        let missing_dir = load_in(&dir.join("eclipse").join("config.json"), None);
        fs::remove_dir_all(&dir).ok();

        for loaded in [missing_file, missing_dir] {
            assert_eq!(loaded.config, Config::default());
            assert_eq!(loaded.problems, []);
            assert!(loaded.unused_keys.is_empty());
        }
    }

    #[test]
    fn a_directory_at_the_config_path_is_unreadable() {
        let dir = sandbox("directory");
        let path = dir.join("config.json");
        fs::create_dir(&path).expect("create a directory at the config path");
        let loaded = load_in(&path, None);
        fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.config, Config::default());
        let [Problem::Unreadable { path: reported, .. }] = loaded.problems.as_slice() else {
            panic!("expected Unreadable, got {:?}", loaded.problems);
        };
        assert_eq!(reported, &path);
        assert!(loaded.problems[0]
            .to_string()
            .ends_with("; Eclipse uses the default for every setting"));
    }

    #[test]
    fn keys_eclipse_does_not_use_are_listed_in_file_order() {
        let (path, loaded) = load_bytes(
            "sober",
            br#"{
                "use_opengl": true,
                "touch_mode": "fake_off",
                "close_on_leave": false,
                "enable_mobile_home_screen": false
            }"#,
        );
        assert_eq!(loaded.problems, []);
        assert_eq!(
            loaded.unused_keys,
            ["use_opengl", "close_on_leave", "enable_mobile_home_screen"]
        );
        assert_eq!(
            loaded.unused_keys_message().as_deref(),
            Some(
                format!(
                    "{}: Eclipse does not use these keys: \"use_opengl\", \"close_on_leave\", \
                     \"enable_mobile_home_screen\"",
                    path.display()
                )
                .as_str()
            )
        );
        assert_eq!(
            loaded.config,
            Config {
                touch_mode: TouchMode::FakeOff,
                ..Config::default()
            }
        );
    }

    #[test]
    fn an_unused_key_is_named_on_one_line_whatever_it_contains() {
        let (_, loaded) = load_bytes("unused-newline", b"{\"a\\nb\": 1}");
        assert_eq!(loaded.unused_keys, ["a\nb"]);
        let message = loaded.unused_keys_message().expect("one unused key");
        assert!(
            message.ends_with(r#": Eclipse does not use these keys: "a\nb""#),
            "{message}"
        );
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn the_written_form_of_a_config_loads_back_unchanged() {
        let configs = [
            Config::default(),
            Config {
                graphics_optimization_mode: GraphicsOptimizationMode::Quality,
                touch_mode: TouchMode::FakeOff,
                enable_gamemode: false,
                roblox_auto_update: false,
                fflags: BTreeMap::from([("DFIntExample".to_owned(), 42.into())]),
                webview_helper_path: Some(PathBuf::from("/opt/eclipse-webview")),
            },
        ];
        for (index, config) in configs.into_iter().enumerate() {
            let json = serde_json::to_string_pretty(&config).expect("serialize");
            let (_, loaded) = load_bytes(&format!("written-{index}"), json.as_bytes());
            assert_eq!(loaded.problems, [], "{json}");
            assert!(loaded.unused_keys.is_empty(), "{json}");
            assert_eq!(loaded.unused_keys_message(), None, "{json}");
            assert_eq!(loaded.config, config, "{json}");
        }
    }

    #[test]
    fn a_sober_enable_gamemode_switch_is_read() {
        let (_, loaded) = load_bytes("gamemode-off", br#"{"enable_gamemode": false}"#);
        assert_eq!(loaded.problems, []);
        assert!(loaded.unused_keys.is_empty(), "{:?}", loaded.unused_keys);
        assert!(!loaded.config.enable_gamemode);
        let (_, loaded) = load_bytes("gamemode-absent", b"{}");
        assert!(loaded.config.enable_gamemode);
    }

    #[test]
    fn an_enable_gamemode_that_is_not_a_boolean_keeps_gamemode_on() {
        let (path, loaded) = load_bytes("gamemode-string", br#"{"enable_gamemode": "no"}"#);
        assert!(loaded.config.enable_gamemode);
        assert_eq!(
            loaded
                .problems
                .iter()
                .map(Problem::to_string)
                .collect::<Vec<_>>(),
            [format!(
                "{}:1:21: enable_gamemode: expected one of `true`, `false`; Eclipse uses the \
                 default (true)",
                path.display()
            )]
        );
    }

    #[test]
    fn roblox_auto_update_can_be_turned_off() {
        let (_, loaded) = load_bytes("auto-update-off", br#"{"roblox_auto_update": false}"#);
        assert_eq!(loaded.problems, []);
        assert!(loaded.unused_keys.is_empty(), "{:?}", loaded.unused_keys);
        assert!(!loaded.config.roblox_auto_update);
        let (_, loaded) = load_bytes("auto-update-absent", b"{}");
        assert!(loaded.config.roblox_auto_update);
    }

    #[test]
    fn a_roblox_auto_update_that_is_not_a_boolean_keeps_updates_on() {
        let (path, loaded) = load_bytes("auto-update-string", br#"{"roblox_auto_update": "no"}"#);
        assert!(loaded.config.roblox_auto_update);
        assert_eq!(
            loaded
                .problems
                .iter()
                .map(Problem::to_string)
                .collect::<Vec<_>>(),
            [format!(
                "{}:1:24: roblox_auto_update: expected one of `true`, `false`; Eclipse uses the \
                 default (true)",
                path.display()
            )]
        );
    }

    #[test]
    fn an_empty_webview_helper_path_means_unset() {
        let (_, loaded) = load_bytes("helper-empty", br#"{"webview_helper_path": ""}"#);
        assert_eq!(loaded.config.webview_helper_path, None);
        assert_eq!(loaded.problems, []);
    }

    #[test]
    fn no_config_directory_is_reported_without_a_path() {
        assert_eq!(
            Problem::NoConfigDir.to_string(),
            "cannot find Eclipse's config directory (is $HOME set?); Eclipse uses the default \
             for every setting"
        );
    }

    #[test]
    fn config_path_lives_under_eclipse_dir() {
        if let Some(path) = config_path() {
            assert!(path.ends_with("eclipse/config.json"), "got {path:?}");
        }
    }
}
