use std::ffi::c_void;
use std::fmt;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JString};
use jni::signature::MethodSignature;
use jni::strings::{JNIStr, JNIString};
use jni::sys::jint;
use jni::{jni_sig, jni_str, Env, EnvUnowned, NativeMethod};

use super::{queue_host_clipboard_text, FrameworkError, ACTIVITY_CLASS, CONTEXT_CLASS};
use crate::portal::{self, ExternalUri, Notice, NoticeId, PortalError, PortalRequest, UriRefusal};

pub(super) const NATIVE_OPEN_URI_NAME: &JNIStr = jni_str!("nativeOpenURI");
pub(super) const NATIVE_OPEN_URI_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Ljava/lang/String;)V");
pub(super) const NATIVE_SHARE_FILE_NAME: &JNIStr = jni_str!("nativeShareFile");
pub(super) const NATIVE_SHARE_FILE_SIG: MethodSignature<'static, 'static> =
    jni_sig!("(Ljava/lang/String;I)V");

const ACTIVITY_NOT_FOUND_EXCEPTION: &JNIStr = jni_str!("android/content/ActivityNotFoundException");
const ACTIVITY_TARGET: &str = "android.app.Activity";
const CONTEXT_TARGET: &str = "android.content.Context";

const LINK_REFUSED_TITLE: &str = "Can't open this link";
const LINK_REFUSED_BODY: &str = "Eclipse opens web and email links only.";
const TEXT_SHARED_TITLE: &str = "Copied to clipboard";
const TEXT_SHARED_BODY: &str = "Paste it wherever you want to share it.";
const FILE_SHARE_REFUSED_TITLE: &str = "Can't share files";
const FILE_SHARE_REFUSED_BODY: &str = "Eclipse can share text and links only.";

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    for (class_name, name, sig, function) in [
        (
            ACTIVITY_CLASS,
            NATIVE_OPEN_URI_NAME,
            NATIVE_OPEN_URI_SIG.sig(),
            activity_native_open_uri as *mut c_void,
        ),
        (
            CONTEXT_CLASS,
            NATIVE_SHARE_FILE_NAME,
            NATIVE_SHARE_FILE_SIG.sig(),
            context_native_share_file as *mut c_void,
        ),
    ] {
        let class = env.find_class(class_name)?;
        let method = unsafe { NativeMethod::from_raw_parts(name, sig, function) };
        unsafe { env.register_native_methods(&class, std::slice::from_ref(&method)) }?;
    }
    tracing::info!(
        "registered Eclipse's backing for Activity.nativeOpenURI + Context.nativeShareFile"
    );
    Ok(())
}

enum LinkNotOpened {
    Refused(UriRefusal),
    NotQueued(PortalError),
}

impl fmt::Display for LinkNotOpened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(refusal) => write!(f, "{refusal}"),
            Self::NotQueued(error) => {
                write!(f, "the desktop portal did not take the link: {error}")
            }
        }
    }
}

fn open_link(
    text: &str,
    submit: impl Fn(PortalRequest) -> Result<(), PortalError>,
) -> Result<(), LinkNotOpened> {
    let uri = match ExternalUri::parse(text) {
        Ok(uri) => uri,
        Err(refusal) => {
            tracing::warn!(
                target: ACTIVITY_TARGET,
                %refusal,
                "Activity.nativeOpenURI: refused the link"
            );
            notify(
                &submit,
                NoticeId::Link,
                LINK_REFUSED_TITLE,
                LINK_REFUSED_BODY,
            );
            return Err(LinkNotOpened::Refused(refusal));
        }
    };
    if let Err(error) = submit(PortalRequest::OpenUri(uri)) {
        tracing::warn!(
            target: ACTIVITY_TARGET,
            %error,
            "Activity.nativeOpenURI: the desktop portal did not take the link"
        );
        return Err(LinkNotOpened::NotQueued(error));
    }
    tracing::info!(
        target: ACTIVITY_TARGET,
        "Activity.nativeOpenURI: handed the link to the desktop portal"
    );
    Ok(())
}

fn share(
    text: Option<String>,
    file_attached: bool,
    copy: impl FnOnce(String),
    submit: impl Fn(PortalRequest) -> Result<(), PortalError>,
) {
    let Some(text) = text else {
        tracing::warn!(
            target: CONTEXT_TARGET,
            file_attached,
            "Context.nativeShareFile: the share holds no text, and Eclipse shares text and links \
             only"
        );
        notify(
            &submit,
            NoticeId::Share,
            FILE_SHARE_REFUSED_TITLE,
            FILE_SHARE_REFUSED_BODY,
        );
        return;
    };
    tracing::info!(
        target: CONTEXT_TARGET,
        bytes = text.len(),
        file_attached,
        "Context.nativeShareFile: copied the shared text to the host clipboard"
    );
    copy(text);
    notify(
        &submit,
        NoticeId::Share,
        TEXT_SHARED_TITLE,
        TEXT_SHARED_BODY,
    );
}

fn notify(
    submit: &impl Fn(PortalRequest) -> Result<(), PortalError>,
    id: NoticeId,
    title: &str,
    body: &str,
) {
    let notice = Notice {
        id,
        title: title.to_owned(),
        body: body.to_owned(),
    };
    if let Err(error) = submit(PortalRequest::Notify(notice)) {
        tracing::warn!(%error, "could not post a desktop notification");
    }
}

extern "system" fn activity_native_open_uri<'local>(
    env: EnvUnowned<'local>,
    _class: JClass<'local>,
    uri: JString<'local>,
) {
    open_uri_from_java(env, &uri, portal::submit);
}

fn open_uri_from_java<'local>(
    mut env: EnvUnowned<'local>,
    uri: &JString<'local>,
    submit: impl Fn(PortalRequest) -> Result<(), PortalError>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let uri = if uri.is_null() {
            String::new()
        } else {
            uri.try_to_string(env)?
        };
        let Err(not_opened) = open_link(&uri, submit) else {
            return Ok(());
        };
        let message = format!(
            "No Activity found to handle Intent {{ act=android.intent.action.VIEW }}: {not_opened}"
        );
        let _ = env.throw_new(ACTIVITY_NOT_FOUND_EXCEPTION, JNIString::from(message));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn context_native_share_file<'local>(
    env: EnvUnowned<'local>,
    _class: JClass<'local>,
    text: JString<'local>,
    fd: jint,
) {
    share_from_java(env, &text, fd, queue_host_clipboard_text, portal::submit);
}

fn share_from_java<'local>(
    mut env: EnvUnowned<'local>,
    text: &JString<'local>,
    fd: jint,
    copy: impl FnOnce(String),
    submit: impl Fn(PortalRequest) -> Result<(), PortalError>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let text = if text.is_null() {
            None
        } else {
            Some(text.try_to_string(env)?)
        };
        share(text, fd >= 0, copy, submit);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::fake_jvm;
    use std::cell::RefCell;

    const ACTIVITY_NOT_FOUND: &str = "android/content/ActivityNotFoundException";
    const NO_FILE: jint = -1;
    const SHARED_FILE: jint = 7;

    type Posted = (NoticeId, String, String);

    fn notices_into(
        posted: &RefCell<Vec<Posted>>,
    ) -> impl Fn(PortalRequest) -> Result<(), PortalError> + '_ {
        |request| {
            let PortalRequest::Notify(notice) = request else {
                panic!("only notices are expected here");
            };
            posted
                .borrow_mut()
                .push((notice.id, notice.title, notice.body));
            Ok(())
        }
    }

    fn posted(id: NoticeId, title: &str, body: &str) -> Vec<Posted> {
        vec![(id, title.to_owned(), body.to_owned())]
    }

    struct Refused {
        exception: Option<String>,
        message: Option<String>,
        posted: Vec<Posted>,
    }

    fn refuse_from_java(uri: Option<&str>) -> Refused {
        let posted = RefCell::new(Vec::new());
        fake_jvm::with_env(|env| {
            let uri = match uri {
                Some(uri) => env.new_string(uri).expect("uri string"),
                None => JString::null(),
            };
            open_uri_from_java(fake_jvm::native_env(), &uri, notices_into(&posted));
        });
        Refused {
            exception: fake_jvm::take_exception(),
            message: fake_jvm::take_thrown_message(),
            posted: posted.into_inner(),
        }
    }

    fn share_from_java_with(text: Option<&str>, fd: jint) -> (Option<String>, Vec<Posted>) {
        let posted = RefCell::new(Vec::new());
        let copied = RefCell::new(None);
        fake_jvm::with_env(|env| {
            let text = match text {
                Some(text) => env.new_string(text).expect("shared text"),
                None => JString::null(),
            };
            share_from_java(
                fake_jvm::native_env(),
                &text,
                fd,
                |text| *copied.borrow_mut() = Some(text),
                notices_into(&posted),
            );
        });
        assert_eq!(fake_jvm::take_exception(), None);
        (copied.into_inner(), posted.into_inner())
    }

    #[test]
    fn a_web_link_goes_to_the_portal_without_a_notice_or_an_exception() {
        let link = "https://example.org/a?b=c";
        let opened = RefCell::new(Vec::new());
        fake_jvm::with_env(|env| {
            let uri = env.new_string(link).expect("uri string");
            open_uri_from_java(fake_jvm::native_env(), &uri, |request| {
                let PortalRequest::OpenUri(uri) = request else {
                    panic!("a web link must only be opened");
                };
                opened.borrow_mut().push(uri);
                Ok(())
            });
        });
        assert_eq!(fake_jvm::take_exception(), None);
        assert!(opened.into_inner() == [ExternalUri::parse(link).expect("a web link")]);
    }

    #[test]
    fn a_refused_link_posts_the_link_notice_and_throws_activity_not_found() {
        for uri in [
            Some("roblox://placeId=1"),
            Some("intent://x#Intent;end"),
            None,
        ] {
            let refused = refuse_from_java(uri);
            assert_eq!(
                refused.exception.as_deref(),
                Some(ACTIVITY_NOT_FOUND),
                "{uri:?}"
            );
            assert_eq!(
                refused.posted,
                posted(NoticeId::Link, LINK_REFUSED_TITLE, LINK_REFUSED_BODY),
                "{uri:?}"
            );
        }
    }

    #[test]
    fn the_refusal_exception_names_the_scheme_but_never_the_link() {
        let refused = refuse_from_java(Some("roblox://placeId=1"));
        let message = refused.message.expect("the exception carries a message");
        assert!(message.contains("roblox"), "{message}");
        assert!(!message.contains("placeId"), "{message}");
    }

    #[test]
    fn a_link_the_portal_cannot_queue_throws_without_a_notice() {
        let attempts = RefCell::new(0);
        fake_jvm::with_env(|env| {
            let uri = env.new_string("https://example.org/x").expect("uri string");
            open_uri_from_java(fake_jvm::native_env(), &uri, |_| {
                *attempts.borrow_mut() += 1;
                Err(PortalError::Busy)
            });
        });
        assert_eq!(
            fake_jvm::take_exception().as_deref(),
            Some(ACTIVITY_NOT_FOUND)
        );
        let message = fake_jvm::take_thrown_message().expect("a message");
        assert!(message.contains("queue is full"), "{message}");
        assert_eq!(attempts.into_inner(), 1);
    }

    #[test]
    fn shared_text_is_copied_and_announced() {
        let text = "hi https://www.roblox.com/share?code=X";
        for fd in [NO_FILE, SHARED_FILE] {
            assert_eq!(
                share_from_java_with(Some(text), fd),
                (
                    Some(text.to_owned()),
                    posted(NoticeId::Share, TEXT_SHARED_TITLE, TEXT_SHARED_BODY)
                )
            );
        }
    }

    #[test]
    fn a_share_without_text_copies_nothing_and_says_files_are_unsupported() {
        for fd in [SHARED_FILE, NO_FILE] {
            assert_eq!(
                share_from_java_with(None, fd),
                (
                    None,
                    posted(
                        NoticeId::Share,
                        FILE_SHARE_REFUSED_TITLE,
                        FILE_SHARE_REFUSED_BODY
                    )
                )
            );
        }
    }
}
