use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JString};
use jni::strings::JNIStr;
use jni::{jni_str, Env, EnvUnowned};

use super::{
    app_data_dir, register_class_natives_best_effort, FrameworkError, NativeBinding,
    ENVIRONMENT_CLASS,
};

const USER_PICTURES_FOLDER: &str = "Roblox";
const APP_DATA_PICTURES_FOLDER: &str = "Pictures";

const GET_PICTURES_DIR_NAME: &JNIStr = jni_str!("native_get_pictures_dir");
const GET_PICTURES_DIR_SIG: &JNIStr = jni_str!("()Ljava/lang/String;");

static PICTURES_DIR: OnceLock<Option<String>> = OnceLock::new();

fn pictures_dir(app_data: &Path, user_pictures: Option<&Path>) -> PathBuf {
    let user_pictures = user_pictures.filter(|user_pictures| {
        let utf8 = user_pictures.to_str().is_some();
        if !utf8 {
            tracing::warn!(
                path = %user_pictures.display(),
                "the Pictures folder path is not UTF-8, which Roblox cannot open; captures go to \
                 app data"
            );
        }
        utf8
    });
    if let Some(user_pictures) = user_pictures {
        let roblox = user_pictures.join(USER_PICTURES_FOLDER);
        if roblox.is_dir() {
            return roblox;
        }
        if user_pictures.is_dir() {
            match std::fs::create_dir(&roblox) {
                Ok(()) => return roblox,
                Err(error) => tracing::warn!(
                    path = %roblox.display(),
                    %error,
                    "cannot create the captures folder in Pictures; captures go to app data"
                ),
            }
        }
    }
    let app_data_pictures = app_data.join(APP_DATA_PICTURES_FOLDER);
    if let Err(error) = std::fs::create_dir_all(&app_data_pictures) {
        tracing::warn!(
            path = %app_data_pictures.display(),
            %error,
            "cannot create the captures folder in app data"
        );
    }
    app_data_pictures
}

fn resolved_pictures_dir() -> Option<&'static str> {
    PICTURES_DIR
        .get_or_init(|| {
            let app_data = app_data_dir()?;
            let user_dirs = directories::UserDirs::new();
            let user_pictures = user_dirs
                .as_ref()
                .and_then(directories::UserDirs::picture_dir);
            let dir = pictures_dir(&app_data, user_pictures);
            match dir.into_os_string().into_string() {
                Ok(dir) => {
                    tracing::info!(path = %dir, "captures are saved here");
                    Some(dir)
                }
                Err(dir) => {
                    tracing::error!(
                        path = %Path::new(&dir).display(),
                        "the captures folder path is not UTF-8, which Roblox cannot open"
                    );
                    None
                }
            }
        })
        .as_deref()
}

extern "system" fn native_get_pictures_dir<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let dir = resolved_pictures_dir()
            .ok_or(jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;
        env.new_string(dir)
    })
    .resolve::<LogErrorAndDefault>()
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 1] = [(
        GET_PICTURES_DIR_NAME,
        GET_PICTURES_DIR_SIG,
        native_get_pictures_dir as *mut c_void,
    )];
    register_class_natives_best_effort(env, ENVIRONMENT_CLASS, &bindings)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;

    use super::*;
    use crate::loader::log_capture::formatted_log;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "eclipse-captures-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&root).ok();
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn app_data(&self) -> PathBuf {
            self.0.join("app-data")
        }

        fn pictures(&self) -> PathBuf {
            self.0.join("Pictures")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn the_native_matches_the_overlay_declaration() {
        let generator = include_str!("../../tools/framework-overlay/patch-framework.sh");
        let declaration = format!(
            ".method private static native {}{}",
            GET_PICTURES_DIR_NAME.to_str(),
            GET_PICTURES_DIR_SIG.to_str()
        );
        assert!(generator.contains(&declaration), "{declaration}");
        assert_eq!(ENVIRONMENT_CLASS.to_str(), "android/os/Environment");
    }

    #[test]
    fn an_existing_roblox_folder_in_pictures_is_used_as_is() {
        let scratch = Scratch::new("existing");
        let roblox = scratch.pictures().join("Roblox");
        fs::create_dir_all(&roblox).unwrap();
        fs::write(roblox.join("earlier.png"), b"png").unwrap();

        let dir = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));

        assert_eq!(dir, roblox);
        assert_eq!(fs::read(roblox.join("earlier.png")).unwrap(), b"png");
        assert!(!scratch.app_data().exists());
    }

    #[test]
    fn a_roblox_folder_is_created_in_an_existing_pictures_folder() {
        let scratch = Scratch::new("create");
        fs::create_dir_all(scratch.pictures()).unwrap();

        let dir = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));

        assert_eq!(dir, scratch.pictures().join("Roblox"));
        assert!(dir.is_dir());
        assert!(!scratch.app_data().exists());
    }

    #[test]
    fn without_a_pictures_folder_setting_captures_go_to_app_data() {
        let scratch = Scratch::new("unset");

        let dir = pictures_dir(&scratch.app_data(), None);

        assert_eq!(dir, scratch.app_data().join("Pictures"));
        assert!(dir.is_dir());
    }

    #[test]
    fn a_missing_pictures_folder_is_not_created() {
        let scratch = Scratch::new("missing");

        let dir = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));

        assert_eq!(dir, scratch.app_data().join("Pictures"));
        assert!(dir.is_dir());
        assert!(!scratch.pictures().exists());
    }

    #[test]
    fn a_second_resolution_changes_nothing() {
        let scratch = Scratch::new("repeat");
        fs::create_dir_all(scratch.pictures()).unwrap();
        let first = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));
        fs::write(first.join("capture.png"), b"png").unwrap();

        let second = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));

        assert_eq!(second, first);
        let entries: Vec<_> = fs::read_dir(&second)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, ["capture.png"]);
        assert!(!scratch.app_data().exists());
    }

    #[test]
    fn a_pictures_folder_java_cannot_name_sends_captures_to_app_data_with_a_warning() {
        let scratch = Scratch::new("not-utf8");
        let pictures = scratch.0.join(OsStr::from_bytes(b"Bilder-\xff-not-utf8"));
        fs::create_dir_all(&pictures).unwrap();
        let mut dir = PathBuf::new();

        let log = formatted_log("eclipse=warn", || {
            dir = pictures_dir(&scratch.app_data(), Some(&pictures));
        });

        assert_eq!(dir, scratch.app_data().join("Pictures"));
        assert!(dir.is_dir());
        assert_eq!(fs::read_dir(&pictures).unwrap().count(), 0);
        let line = log
            .lines()
            .find(|line| line.contains("the Pictures folder path is not UTF-8"))
            .unwrap_or_else(|| panic!("no warning in {log:?}"));
        assert!(line.contains("WARN"), "{line}");
        assert!(
            line.contains(&format!("path={}", pictures.display())),
            "{line}"
        );
    }

    #[test]
    fn a_file_named_roblox_in_pictures_sends_captures_to_app_data_with_a_warning() {
        let scratch = Scratch::new("file");
        fs::create_dir_all(scratch.pictures()).unwrap();
        let blocker = scratch.pictures().join("Roblox");
        fs::write(&blocker, b"not a folder").unwrap();
        let mut dir = PathBuf::new();

        let log = formatted_log("eclipse=warn", || {
            dir = pictures_dir(&scratch.app_data(), Some(&scratch.pictures()));
        });

        assert_eq!(dir, scratch.app_data().join("Pictures"));
        assert!(dir.is_dir());
        assert_eq!(fs::read(&blocker).unwrap(), b"not a folder");
        let line = log
            .lines()
            .find(|line| line.contains("cannot create the captures folder in Pictures"))
            .unwrap_or_else(|| panic!("no warning in {log:?}"));
        assert!(line.contains("WARN"), "{line}");
        assert!(
            line.contains(&format!("path={}", blocker.display())),
            "{line}"
        );
    }
}
