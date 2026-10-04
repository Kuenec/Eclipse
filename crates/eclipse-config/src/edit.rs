use std::fmt;
use std::fs::{self, File, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;

use crate::document::Document;
use crate::temp_file::TempFile;
use crate::{containing_directory, Problem, Setting, SettingKey};

const OWNER_WRITE: u32 = 0o200;

const PERMISSION_BITS: u32 = 0o777;

const MISSING_FILE: &[u8] = b"{}";

const INDENT: &str = "  ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Set(Setting),
    Unset(SettingKey),
}

impl Change {
    #[must_use]
    pub const fn key(self) -> SettingKey {
        match self {
            Self::Set(setting) => setting.key(),
            Self::Unset(key) => key,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Written,
    Unchanged,
}

#[derive(Debug)]
pub enum EditError {
    Link {
        path: PathBuf,
        target: PathBuf,
    },

    ReadOnly {
        path: PathBuf,
    },

    Invalid(Problem),

    ChangedWhileSaving {
        path: PathBuf,
    },

    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl EditError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link { path, target } => write!(
                f,
                "{} links to {}, so it is managed outside Eclipse; change it where it is managed",
                path.display(),
                target.display()
            ),
            Self::ReadOnly { path } => write!(
                f,
                "{} is read-only, so it is managed outside Eclipse; change it where it is \
                 managed, or make it writable",
                path.display()
            ),
            Self::Invalid(problem) => write!(
                f,
                "{problem}; Eclipse changes the file only after it is fixed by hand"
            ),
            Self::ChangedWhileSaving { path } => write!(
                f,
                "{} changed while Eclipse was saving it, so Eclipse kept that change and saved \
                 nothing; try again",
                path.display()
            ),
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "cannot {action} {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Link { .. }
            | Self::ReadOnly { .. }
            | Self::Invalid(_)
            | Self::ChangedWhileSaving { .. } => None,
        }
    }
}

struct Original {
    bytes: Vec<u8>,
    permissions: Permissions,
}

pub fn apply(path: &Path, change: Change) -> Result<Applied, EditError> {
    let original = read_original(path)?;
    let document = editable_document(path, original.as_ref())?;
    let Some(contents) = edited(&document, change) else {
        return Ok(Applied::Unchanged);
    };
    commit(path, original.as_ref(), contents.as_bytes())?;
    Ok(Applied::Written)
}

pub fn check(path: &Path) -> Result<(), EditError> {
    let original = read_original(path)?;
    editable_document(path, original.as_ref()).map(drop)
}

fn editable_document<'a>(
    path: &Path,
    original: Option<&'a Original>,
) -> Result<Document<'a>, EditError> {
    let bytes = original.map_or(MISSING_FILE, |original| original.bytes.as_slice());
    let document = Document::parse(bytes)
        .map_err(|malformed| EditError::Invalid(Problem::malformed(path, malformed)))?;
    if let Some((key, value)) = document.repeats().next() {
        return Err(EditError::Invalid(Problem::DuplicateKey {
            path: path.to_owned(),
            key: key.clone(),
            at: document.position(value),
        }));
    }
    Ok(document)
}

fn read_original(path: &Path) -> Result<Option<Original>, EditError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(EditError::io("inspect", path, source)),
    };
    if metadata.is_symlink() {
        let target =
            fs::read_link(path).map_err(|source| EditError::io("read the link", path, source))?;
        return Err(EditError::Link {
            path: path.to_owned(),
            target,
        });
    }
    let permissions = metadata.permissions();
    if permissions.mode() & OWNER_WRITE == 0 {
        return Err(EditError::ReadOnly {
            path: path.to_owned(),
        });
    }
    let bytes = fs::read(path).map_err(|source| EditError::io("read", path, source))?;
    Ok(Some(Original { bytes, permissions }))
}

fn edited(document: &Document<'_>, change: Change) -> Option<String> {
    let name = change.key().name();
    let current = document
        .entries
        .iter()
        .find_map(|(key, value)| (key == name).then_some(*value));
    let value = match (change, current) {
        (Change::Set(setting), Some(current)) if holds(current, setting) => return None,
        (Change::Set(setting), _) => {
            Some(serde_json::to_string(&setting).expect("a setting has a JSON form"))
        }
        (Change::Unset(_), None) => return None,
        (Change::Unset(_), Some(_)) => None,
    };
    let mut entries = Vec::with_capacity(document.entries.len() + 1);
    for (key, raw) in &document.entries {
        if key != name {
            entries.push((key.as_str(), raw.get()));
        } else if let Some(value) = &value {
            entries.push((key.as_str(), value.as_str()));
        }
    }
    if let (None, Some(value)) = (current, &value) {
        entries.push((name, value.as_str()));
    }
    Some(layout(&entries))
}

fn holds(value: &RawValue, setting: Setting) -> bool {
    serde_json::from_str(value.get()).is_ok_and(|value| setting.key().parse(value) == Ok(setting))
}

fn layout(entries: &[(&str, &str)]) -> String {
    if entries.is_empty() {
        return "{}\n".to_owned();
    }
    let lines: Vec<String> = entries
        .iter()
        .map(|(key, value)| {
            let key = serde_json::to_string(key).expect("a key has a JSON form");
            format!("{INDENT}{key}: {value}")
        })
        .collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

fn commit(path: &Path, original: Option<&Original>, contents: &[u8]) -> Result<(), EditError> {
    let directory = containing_directory(path);
    if original.is_none() {
        fs::create_dir_all(directory)
            .map_err(|source| EditError::io("create", directory, source))?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let mut temp = temporary(directory, &name, original)
        .map_err(|source| EditError::io("create a file in", directory, source))?;
    write_temporary(&mut temp, original, contents)
        .map_err(|source| EditError::io("write", temp.path(), source))?;
    let on_disk = read_if_present(path)?;
    if on_disk.as_deref() != original.map(|original| original.bytes.as_slice()) {
        return Err(EditError::ChangedWhileSaving {
            path: path.to_owned(),
        });
    }
    temp.persist(path)
        .map_err(|source| EditError::io("replace", path, source))?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| EditError::io("sync", directory, source))
}

fn temporary(directory: &Path, name: &str, original: Option<&Original>) -> io::Result<TempFile> {
    match original {
        Some(original) => TempFile::create_with_mode(
            directory,
            name,
            original.permissions.mode() & PERMISSION_BITS,
        ),
        None => TempFile::create(directory, name),
    }
}

fn write_temporary(
    temp: &mut TempFile,
    original: Option<&Original>,
    contents: &[u8],
) -> io::Result<()> {
    if let Some(original) = original {
        temp.set_permissions(original.permissions.clone())?;
    }
    temp.write_all(contents)?;
    temp.sync()
}

fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, EditError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(EditError::io("read", path, source)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::{load_from, GraphicsOptimizationMode, TouchMode};

    const FIXTURE: &str = r#"{
  "zeta_unknown": 1,
  "touch_mode": "fake-off",
  "allow_gamepad_permission": false,
  "alpha_unknown": {"b": 2, "a": 1},
  "fflags": {"FFlagGameBasicSettingsFramerateCap5": "True"}
}
"#;

    const TOUCH_ON: Change = Change::Set(Setting::TouchMode(TouchMode::On));

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eclipse-config-edit-{tag}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the sandbox directory");
        dir
    }

    fn config_with(tag: &str, text: &[u8]) -> (PathBuf, PathBuf) {
        let dir = sandbox(tag);
        let path = dir.join("config.json");
        fs::write(&path, text).expect("write config.json");
        (dir, path)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("list the sandbox")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .into_string()
                    .expect("UTF-8")
            })
            .collect();
        names.sort();
        names
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o7777
    }

    fn rewritten(tag: &str, text: &str, change: Change) -> String {
        let (dir, path) = config_with(tag, text.as_bytes());
        let applied = apply(&path, change).expect("apply");
        let after = fs::read_to_string(&path).expect("read config.json back");
        let entries = names(&dir);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(applied, Applied::Written);
        assert_eq!(entries, ["config.json"]);
        after
    }

    #[test]
    fn setting_a_key_changes_only_its_line() {
        let written = rewritten("set", FIXTURE, TOUCH_ON);
        let changed: Vec<(&str, &str)> = FIXTURE
            .lines()
            .zip(written.lines())
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed,
            [(r#"  "touch_mode": "fake-off","#, r#"  "touch_mode": "on","#)]
        );
        assert_eq!(
            written,
            FIXTURE.replace(r#""touch_mode": "fake-off""#, r#""touch_mode": "on""#)
        );
    }

    #[test]
    fn a_new_key_is_appended_last() {
        let performance = GraphicsOptimizationMode::Performance;
        let written = rewritten(
            "append",
            FIXTURE,
            Change::Set(Setting::GraphicsOptimizationMode(performance)),
        );
        assert_eq!(
            written,
            FIXTURE.replace(
                "\"True\"}\n}",
                "\"True\"},\n  \"graphics_optimization_mode\": \"performance\"\n}"
            )
        );
    }

    #[test]
    fn unset_removes_only_that_line() {
        let written = rewritten("unset", FIXTURE, Change::Unset(SettingKey::TouchMode));
        assert_eq!(
            written,
            FIXTURE.replace("  \"touch_mode\": \"fake-off\",\n", "")
        );
    }

    #[test]
    fn unsetting_the_last_key_leaves_an_empty_object() {
        let written = rewritten(
            "unset-last",
            r#"{"touch_mode": "on"}"#,
            Change::Unset(SettingKey::TouchMode),
        );
        assert_eq!(written, "{}\n");
    }

    #[test]
    fn only_the_top_level_layout_is_normalized() {
        let one_line = rewritten("one-line", r#"{"touch_mode": 5, "x": [1,2]}"#, TOUCH_ON);
        assert_eq!(
            one_line,
            "{\n  \"touch_mode\": \"on\",\n  \"x\": [1,2]\n}\n"
        );

        let nested = r#"{
  "fflags": {
      "A": 1,
   "B": [1,2]
  },
  "touch_mode": "off"
}
"#;
        assert_eq!(
            rewritten("nested", nested, TOUCH_ON),
            nested.replace("\"off\"", "\"on\"")
        );
    }

    #[test]
    fn a_missing_file_is_created_with_one_key() {
        let dir = sandbox("missing");
        let config_dir = dir.join("eclipse");
        let path = config_dir.join("config.json");
        assert_eq!(
            apply(&path, Change::Unset(SettingKey::TouchMode)).expect("unset"),
            Applied::Unchanged
        );
        assert!(!config_dir.exists());

        assert_eq!(
            apply(&path, Change::Set(Setting::EnableGamemode(false))).expect("set"),
            Applied::Written
        );
        let written = fs::read_to_string(&path).expect("read config.json");
        assert_eq!(names(&config_dir), ["config.json"]);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(written, "{\n  \"enable_gamemode\": false\n}\n");
    }

    #[test]
    fn setting_the_value_already_there_writes_nothing() {
        let fake_off = Change::Set(Setting::TouchMode(TouchMode::FakeOff));
        for (tag, text) in [
            ("same", FIXTURE),
            ("same-sober-spelling", r#"{"touch_mode": "fake_off"}"#),
        ] {
            let (dir, path) = config_with(tag, text.as_bytes());
            File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(SystemTime::now() - Duration::from_secs(3600)))
                .expect("age config.json");
            let modified = fs::metadata(&path).and_then(|metadata| metadata.modified());

            let applied = apply(&path, fake_off).expect("apply");

            let after = fs::metadata(&path).and_then(|metadata| metadata.modified());
            let bytes = fs::read(&path).expect("read config.json");
            fs::remove_dir_all(&dir).ok();
            assert_eq!(applied, Applied::Unchanged, "{tag}");
            assert_eq!(after.expect("mtime"), modified.expect("mtime"), "{tag}");
            assert_eq!(bytes, text.as_bytes(), "{tag}");
        }
    }

    #[test]
    fn check_accepts_what_apply_would_write_and_writes_nothing() {
        for (tag, text) in [
            ("check", FIXTURE),
            ("check-invalid-value", r#"{"touch_mode": 5}"#),
        ] {
            let (dir, path) = config_with(tag, text.as_bytes());
            let checked = check(&path);
            let after = fs::read(&path).expect("read config.json");
            fs::remove_dir_all(&dir).ok();
            assert!(checked.is_ok(), "{tag}: {checked:?}");
            assert_eq!(after, text.as_bytes(), "{tag}");
        }
        let dir = sandbox("check-missing");
        let path = dir.join("eclipse").join("config.json");
        let checked = check(&path);
        let created = dir.join("eclipse").exists();
        fs::remove_dir_all(&dir).ok();
        assert!(checked.is_ok(), "{checked:?}");
        assert!(!created);
    }

    #[test]
    fn a_link_is_refused_and_left_alone() {
        let dir = sandbox("link");
        let target = dir.join("dotfiles.json");
        fs::write(&target, FIXTURE).expect("write the link target");
        let link = dir.join("config.json");
        symlink(&target, &link).expect("create the link");
        let gone = dir.join("gone.json");
        let dangling = dir.join("dangling.json");
        symlink(&gone, &dangling).expect("create the dangling link");

        for (config, destination) in [(&link, &target), (&dangling, &gone)] {
            assert!(
                matches!(check(config), Err(EditError::Link { path, target: reported })
                    if &path == config && &reported == destination),
                "{config:?}"
            );
            let error = apply(config, TOUCH_ON).expect_err("a link");
            assert!(
                matches!(&error, EditError::Link { path, target: reported }
                    if path == config && reported == destination),
                "{error:?}"
            );
            assert!(
                error.to_string().contains("managed outside Eclipse"),
                "{error}"
            );
            assert_eq!(&fs::read_link(config).expect("still a link"), destination);
        }
        let target_text = fs::read_to_string(&target).expect("read the link target");
        let entries = names(&dir);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(target_text, FIXTURE);
        assert_eq!(entries, ["config.json", "dangling.json", "dotfiles.json"]);
    }

    #[test]
    fn a_read_only_file_is_refused_and_left_alone() {
        let (dir, path) = config_with("read-only", FIXTURE.as_bytes());
        fs::set_permissions(&path, Permissions::from_mode(0o444)).expect("make it read-only");

        let checked = check(&path);
        let error = apply(&path, TOUCH_ON).expect_err("read-only");

        let after = (fs::read(&path).expect("read config.json"), mode(&path));
        let entries = names(&dir);
        fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(&error, EditError::ReadOnly { path: reported } if reported == &path),
            "{error:?}"
        );
        assert!(
            matches!(&checked, Err(EditError::ReadOnly { path: reported }) if reported == &path),
            "{checked:?}"
        );
        assert!(
            error.to_string().contains("managed outside Eclipse"),
            "{error}"
        );
        assert_eq!(after, (FIXTURE.as_bytes().to_vec(), 0o444));
        assert_eq!(entries, ["config.json"]);
    }

    #[test]
    fn a_file_eclipse_cannot_read_is_never_overwritten() {
        let cases: [(&str, &[u8]); 4] = [
            ("syntax", br#"{"touch_mode": "#),
            ("array", b"[]"),
            ("not-utf8", b"{\"a\": \"\xff\"}"),
            (
                "duplicate",
                br#"{"touch_mode": "on", "x": 1, "touch_mode": "off"}"#,
            ),
        ];
        for (tag, bytes) in cases {
            let (dir, path) = config_with(tag, bytes);

            let checked = check(&path).expect_err(tag);
            let error = apply(&path, TOUCH_ON).expect_err(tag);

            let reported = load_from(&path).problems;
            let after = fs::read(&path).expect("read config.json");
            let entries = names(&dir);
            fs::remove_dir_all(&dir).ok();
            let EditError::Invalid(problem) = &error else {
                panic!("{tag}: expected Invalid, got {error:?}");
            };
            assert!(
                matches!(&checked, EditError::Invalid(checked) if checked == problem),
                "{tag}: {checked:?}"
            );
            assert_eq!(reported.first(), Some(problem), "{tag}");
            assert!(
                error.to_string().starts_with(&problem.to_string()),
                "{tag}: {error}"
            );
            assert_eq!(after, bytes, "{tag}");
            assert_eq!(entries, ["config.json"], "{tag}");
        }
    }

    #[test]
    fn a_write_keeps_the_file_mode() {
        let (dir, path) = config_with("mode", FIXTURE.as_bytes());
        fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("make it private");

        let applied = apply(&path, TOUCH_ON).expect("apply");

        let after = mode(&path);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(applied, Applied::Written);
        assert_eq!(after, 0o600);
    }

    #[test]
    fn the_temporary_of_a_private_file_is_private_before_anything_is_written() {
        let (dir, path) = config_with("private-temporary", FIXTURE.as_bytes());
        fs::set_permissions(&path, Permissions::from_mode(0o600)).expect("make it private");
        let original = read_original(&path)
            .expect("read config.json")
            .expect("config.json exists");

        let temp = temporary(&dir, "config.json", Some(&original)).expect("create the temporary");

        let created = mode(temp.path());
        let size = fs::metadata(temp.path()).expect("stat").len();
        drop(temp);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(size, 0);
        assert_eq!(created & !0o600, 0, "{created:o}");
    }

    #[test]
    fn a_file_changed_while_saving_keeps_the_other_change() {
        let (dir, path) = config_with("changed", FIXTURE.as_bytes());
        let read_earlier = Original {
            bytes: MISSING_FILE.to_vec(),
            permissions: fs::metadata(&path).expect("stat").permissions(),
        };

        let changed = commit(&path, Some(&read_earlier), b"{}\n").expect_err("changed");
        let created = commit(&path, None, b"{}\n").expect_err("created meanwhile");

        let after = fs::read_to_string(&path).expect("read config.json");
        let entries = names(&dir);
        fs::remove_dir_all(&dir).ok();
        for error in [&changed, &created] {
            assert!(
                matches!(error, EditError::ChangedWhileSaving { path: reported }
                    if reported == &path),
                "{error:?}"
            );
        }
        assert_eq!(after, FIXTURE);
        assert_eq!(entries, ["config.json"]);
    }
}
