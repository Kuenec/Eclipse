use std::fs::{self, File, OpenOptions};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SUFFIX: &str = ".tmp";
const NAME_ATTEMPTS: usize = 16;
const ABANDONED_AGE: Duration = Duration::from_secs(60 * 60);

pub struct TempFile {
    path: PathBuf,
    file: File,
    persisted: bool,
}

impl TempFile {
    pub fn create(dir: &Path, name: &str) -> io::Result<Self> {
        let mut attempts = 1;
        loop {
            let path = dir.join(unique_name(name));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        persisted: false,
                    })
                }
                Err(error)
                    if error.kind() == io::ErrorKind::AlreadyExists && attempts < NAME_ATTEMPTS =>
                {
                    attempts += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn persist(mut self, dest: &Path) -> io::Result<()> {
        fs::rename(&self.path, dest)?;
        self.persisted = true;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn unique_name(name: &str) -> String {
    let unique = RandomState::new().build_hasher().finish();
    format!("{name}.{unique:016x}{SUFFIX}")
}

pub fn is_abandoned(entry: &fs::DirEntry) -> io::Result<bool> {
    let temp = entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.ends_with(SUFFIX));
    if !temp || !entry.file_type()?.is_file() {
        return Ok(false);
    }
    let modified = entry.metadata()?.modified()?;
    Ok(modified.elapsed().is_ok_and(|age| age >= ABANDONED_AGE))
}

pub fn remove_abandoned(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if is_abandoned(&entry)? {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-temp-file-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn concurrent_writers_of_one_file_never_share_a_temporary() {
        let dir = temp_dir("concurrent");
        let mut first = TempFile::create(&dir, "settings.json").unwrap();
        let mut second = TempFile::create(&dir, "settings.json").unwrap();
        assert_ne!(first.path(), second.path());
        first.write_all(b"first").unwrap();
        second.write_all(b"second").unwrap();
        first.persist(&dir.join("settings.json")).unwrap();
        assert_eq!(fs::read(dir.join("settings.json")).unwrap(), b"first");
        second.persist(&dir.join("settings.json")).unwrap();
        assert_eq!(fs::read(dir.join("settings.json")).unwrap(), b"second");

        drop(TempFile::create(&dir, "settings.json").unwrap());
        assert_eq!(
            names(&dir),
            ["settings.json"],
            "an unused temporary is removed"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_temporaries_older_than_an_hour_are_abandoned() {
        let dir = temp_dir("abandoned");
        for name in ["old.json.1.tmp", "new.json.2.tmp", "kept.json"] {
            fs::write(dir.join(name), b"{").unwrap();
        }
        fs::create_dir(dir.join("dir.tmp")).unwrap();
        for name in ["old.json.1.tmp", "kept.json"] {
            File::options()
                .write(true)
                .open(dir.join(name))
                .unwrap()
                .set_modified(SystemTime::now() - 2 * ABANDONED_AGE)
                .unwrap();
        }
        remove_abandoned(&dir).unwrap();
        assert_eq!(names(&dir), ["dir.tmp", "kept.json", "new.json.2.tmp"]);
        fs::remove_dir_all(&dir).ok();
    }
}
