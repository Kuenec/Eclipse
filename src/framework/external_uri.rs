use std::ffi::OsStr;
use std::fmt;
use std::process::{Command, Stdio};

use jni::objects::{JClass, JString};
use jni::strings::{JNIStr, JNIString};
use jni::{jni_str, EnvUnowned};

use jni::errors::LogErrorAndDefault;

pub(super) const NATIVE_OPEN_URI_NAME: &JNIStr = jni_str!("nativeOpenURI");
pub(super) const NATIVE_OPEN_URI_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)V");

const ACTIVITY_NOT_FOUND_EXCEPTION: &JNIStr = jni_str!("android/content/ActivityNotFoundException");
const HOST_OPENER: &str = "xdg-open";
const MAX_URI_BYTES: usize = 8 * 1024;
const HOST_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct HostUri(String);

#[derive(Debug, Clone, PartialEq, Eq)]
enum UriRejection {
    Malformed,
    UnsupportedScheme(String),
}

impl fmt::Display for UriRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("the URI is malformed"),
            Self::UnsupportedScheme(scheme) => {
                write!(f, "no desktop application handles {scheme}: URIs")
            }
        }
    }
}

impl HostUri {
    fn parse(uri: &str) -> Result<Self, UriRejection> {
        if uri.is_empty()
            || uri.len() > MAX_URI_BYTES
            || !uri.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(UriRejection::Malformed);
        }
        let (scheme, rest) = uri.split_once(':').ok_or(UriRejection::Malformed)?;
        let scheme_is_valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_is_valid || rest.is_empty() {
            return Err(UriRejection::Malformed);
        }
        if !HOST_SCHEMES
            .iter()
            .any(|host| scheme.eq_ignore_ascii_case(host))
        {
            return Err(UriRejection::UnsupportedScheme(scheme.to_ascii_lowercase()));
        }
        Ok(Self(uri.to_owned()))
    }
}

fn open_with(opener: &OsStr, uri: &HostUri) -> std::io::Result<()> {
    let mut child = Command::new(opener)
        .arg(&uri.0)
        .stdin(Stdio::null())
        .spawn()?;
    std::thread::Builder::new()
        .name("eclipse-open-uri".to_owned())
        .spawn(move || match child.wait() {
            Ok(status) if status.success() => {}
            Ok(status) => tracing::warn!(
                target: "android.app.Activity",
                %status,
                "the desktop could not open the URI"
            ),
            Err(error) => tracing::warn!(
                target: "android.app.Activity",
                %error,
                "waiting for the desktop URI opener failed"
            ),
        })?;
    Ok(())
}

pub(super) extern "system" fn activity_native_open_uri<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    uri: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let uri = if uri.is_null() {
            String::new()
        } else {
            uri.try_to_string(env)?
        };
        let failure = match HostUri::parse(&uri) {
            Ok(host_uri) => match open_with(OsStr::new(HOST_OPENER), &host_uri) {
                Ok(()) => {
                    tracing::info!(
                        target: "android.app.Activity",
                        "Activity.nativeOpenURI: handed the URI to the desktop"
                    );
                    return Ok(());
                }
                Err(error) => format!("{HOST_OPENER} could not start: {error}"),
            },
            Err(rejection) => rejection.to_string(),
        };
        tracing::warn!(
            target: "android.app.Activity",
            reason = %failure,
            "Activity.nativeOpenURI: no handler for the URI"
        );
        let message = format!(
            "No Activity found to handle Intent {{ act=android.intent.action.VIEW dat={uri} }}: {failure}"
        );
        let _ = env.throw_new(ACTIVITY_NOT_FOUND_EXCEPTION, JNIString::from(message));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::fake_jvm;

    #[test]
    fn web_and_mail_uris_are_handed_to_the_desktop() {
        for uri in [
            "https://www.roblox.com/games/1",
            "HTTP://example.com",
            "mailto:info@roblox.com",
        ] {
            assert_eq!(HostUri::parse(uri), Ok(HostUri(uri.to_owned())));
        }
    }

    #[test]
    fn other_schemes_and_malformed_uris_are_rejected() {
        assert_eq!(
            HostUri::parse("roblox://placeId=1"),
            Err(UriRejection::UnsupportedScheme("roblox".to_owned()))
        );
        assert_eq!(
            HostUri::parse("file:///etc/passwd"),
            Err(UriRejection::UnsupportedScheme("file".to_owned()))
        );
        for malformed in [
            "",
            "https",
            "https:",
            "-https://x",
            "https://a b",
            "https://x\n",
        ] {
            assert_eq!(HostUri::parse(malformed), Err(UriRejection::Malformed));
        }
    }

    #[test]
    fn the_desktop_opener_receives_the_uri_as_one_argument() {
        let dir = std::env::temp_dir().join(format!("eclipse-open-uri-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let record = dir.join("argv");
        let opener = dir.join("opener");
        std::fs::write(
            &opener,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" > '{}.tmp' && mv '{0}.tmp' '{0}'\n",
                record.display()
            ),
        )
        .expect("write opener");
        std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("make opener executable");

        let uri = HostUri::parse("https://example.com/a?b=c&d=$(id)").expect("valid uri");
        open_with(opener.as_os_str(), &uri).expect("opener starts");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let argv = loop {
            if let Ok(argv) = std::fs::read_to_string(&record) {
                break argv;
            }
            assert!(std::time::Instant::now() < deadline, "the opener never ran");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(argv, "1\nhttps://example.com/a?b=c&d=$(id)\n");
    }

    #[test]
    fn an_unhandled_uri_raises_activity_not_found() {
        fake_jvm::with_env(|env| {
            let uri = env.new_string("roblox://placeId=1").expect("uri string");
            activity_native_open_uri(fake_jvm::native_env(), JClass::null(), uri);
        });
        assert_eq!(
            fake_jvm::take_exception().as_deref(),
            Some("android/content/ActivityNotFoundException")
        );
    }
}
