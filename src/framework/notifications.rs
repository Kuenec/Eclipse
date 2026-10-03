use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::strings::JNIStr;
use jni::sys::{jboolean, jint, jlong};
use jni::{jni_str, Env, EnvUnowned, NativeMethod};

use super::{FrameworkError, NativeBinding};
use crate::portal::{self, Notice, NoticeId, PortalError, PortalRequest};

pub(super) const NOTIFICATION_MANAGER_CLASS: &JNIStr = jni_str!("android/app/NotificationManager");
const NOTIFICATION_TARGET: &str = "android.app.NotificationManager";

const INIT_BUILDER_NAME: &JNIStr = jni_str!("nativeInitBuilder");
const INIT_BUILDER_SIG: &JNIStr = jni_str!("()J");
const ADD_ACTION_NAME: &JNIStr = jni_str!("nativeAddAction");
const ADD_ACTION_SIG: &JNIStr = jni_str!("(JLjava/lang/String;ILandroid/content/Intent;)V");
const SHOW_NOTIFICATION_NAME: &JNIStr = jni_str!("nativeShowNotification");
const SHOW_NOTIFICATION_SIG: &JNIStr = jni_str!(
    "(JILjava/lang/String;Ljava/lang/String;Ljava/lang/String;ZILandroid/content/Intent;)V"
);
const CANCEL_NAME: &JNIStr = jni_str!("nativeCancel");
const CANCEL_SIG: &JNIStr = jni_str!("(I)V");
const SHOW_MPRIS_NAME: &JNIStr = jni_str!("nativeShowMPRIS");
const SHOW_MPRIS_SIG: &JNIStr = jni_str!("(Ljava/lang/String;Ljava/lang/String;)V");
const CANCEL_MPRIS_NAME: &JNIStr = jni_str!("nativeCancelMPRIS");
const CANCEL_MPRIS_SIG: &JNIStr = jni_str!("()V");

const NO_BUILDER: jlong = 0;
const TITLE_LIMIT: usize = 100;
const BODY_LIMIT: usize = 400;
const UNTITLED: &str = "Roblox";
const CUT_MARK: char = '…';

static HOST_WINDOW_FOCUSED: AtomicBool = AtomicBool::new(false);
static ACTIONS_REPORTED: AtomicBool = AtomicBool::new(false);
static MEDIA_CONTROLS_REPORTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_host_window_focused(focused: bool) {
    HOST_WINDOW_FOCUSED.store(focused, Ordering::Release);
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 6] = [
        (
            INIT_BUILDER_NAME,
            INIT_BUILDER_SIG,
            notification_manager_native_init_builder as *mut c_void,
        ),
        (
            ADD_ACTION_NAME,
            ADD_ACTION_SIG,
            notification_manager_native_add_action as *mut c_void,
        ),
        (
            SHOW_NOTIFICATION_NAME,
            SHOW_NOTIFICATION_SIG,
            notification_manager_native_show_notification as *mut c_void,
        ),
        (
            CANCEL_NAME,
            CANCEL_SIG,
            notification_manager_native_cancel as *mut c_void,
        ),
        (
            SHOW_MPRIS_NAME,
            SHOW_MPRIS_SIG,
            notification_manager_native_show_mpris as *mut c_void,
        ),
        (
            CANCEL_MPRIS_NAME,
            CANCEL_MPRIS_SIG,
            notification_manager_native_cancel_mpris as *mut c_void,
        ),
    ];
    let class = env.find_class(NOTIFICATION_MANAGER_CLASS)?;
    let methods = bindings
        .map(|(name, sig, function)| unsafe { NativeMethod::from_raw_parts(name, sig, function) });
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/app/NotificationManager",
        "registered Eclipse's desktop-portal backing for the notification natives"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotShown {
    Ongoing,
    WindowFocused,
    NoText,
}

fn desktop_notice(
    id: i32,
    title: Option<&str>,
    body: Option<&str>,
    ongoing: bool,
    window_focused: bool,
) -> Result<Notice, NotShown> {
    if ongoing {
        return Err(NotShown::Ongoing);
    }
    if window_focused {
        return Err(NotShown::WindowFocused);
    }
    let title = shown_text(title.unwrap_or_default(), TITLE_LIMIT);
    let body = shown_text(body.unwrap_or_default(), BODY_LIMIT);
    if title.is_empty() && body.is_empty() {
        return Err(NotShown::NoText);
    }
    let title = if title.is_empty() {
        UNTITLED.to_owned()
    } else {
        title
    };
    Ok(Notice {
        id: NoticeId::Client(id),
        title,
        body,
    })
}

fn shown_text(text: &str, limit: usize) -> String {
    let printable: String = text.chars().filter_map(printable).collect();
    let printable = printable.trim();
    if printable.chars().nth(limit).is_none() {
        return printable.to_owned();
    }
    let kept: String = printable.chars().take(limit - 1).collect();
    format!("{}{CUT_MARK}", kept.trim_end())
}

fn printable(character: char) -> Option<char> {
    match character {
        _ if !character.is_control() => Some(character),
        _ if character.is_whitespace() => Some(' '),
        _ => None,
    }
}

fn show(
    id: i32,
    title: Option<&str>,
    body: Option<&str>,
    ongoing: bool,
    window_focused: bool,
    submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>,
) {
    let notice = match desktop_notice(id, title, body, ongoing, window_focused) {
        Ok(notice) => notice,
        Err(reason) => {
            tracing::debug!(
                target: NOTIFICATION_TARGET,
                id,
                ?reason,
                "nativeShowNotification: not shown on the desktop"
            );
            return;
        }
    };
    match submit(PortalRequest::Notify(notice)) {
        Ok(()) => tracing::info!(
            target: NOTIFICATION_TARGET,
            id,
            "nativeShowNotification: handed the notification to the desktop portal"
        ),
        Err(error) => tracing::warn!(
            target: NOTIFICATION_TARGET,
            id,
            %error,
            "nativeShowNotification: the desktop portal did not take the notification"
        ),
    }
}

fn withdraw(id: i32, submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>) {
    if let Err(error) = submit(PortalRequest::Withdraw(NoticeId::Client(id))) {
        tracing::warn!(
            target: NOTIFICATION_TARGET,
            id,
            %error,
            "nativeCancel: the desktop portal did not take the withdrawal"
        );
    }
}

fn optional_string(env: &mut Env, text: &JString) -> jni::errors::Result<Option<String>> {
    if text.is_null() {
        return Ok(None);
    }
    text.try_to_string(env).map(Some)
}

fn report_media_controls_unsupported() {
    if !MEDIA_CONTROLS_REPORTED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            target: NOTIFICATION_TARGET,
            "the client's media controls are not supported on the desktop; ignoring them"
        );
    }
}

extern "system" fn notification_manager_native_init_builder<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(NO_BUILDER) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn notification_manager_native_add_action<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _builder: jlong,
    _title: JString<'local>,
    _intent_type: jint,
    _intent: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if !ACTIONS_REPORTED.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                target: NOTIFICATION_TARGET,
                "nativeAddAction: notification actions are not shown on the desktop"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn notification_manager_native_show_notification<'local>(
    env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _builder: jlong,
    id: jint,
    title: JString<'local>,
    text: JString<'local>,
    _icon_path: JString<'local>,
    ongoing: jboolean,
    _intent_type: jint,
    _intent: JObject<'local>,
) {
    show_from_java(
        env,
        id,
        &title,
        &text,
        ongoing,
        HOST_WINDOW_FOCUSED.load(Ordering::Acquire),
        portal::submit,
    );
}

fn show_from_java<'local>(
    mut env: EnvUnowned<'local>,
    id: jint,
    title: &JString<'local>,
    text: &JString<'local>,
    ongoing: bool,
    window_focused: bool,
    submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let title = optional_string(env, title)?;
        let text = optional_string(env, text)?;
        show(
            id,
            title.as_deref(),
            text.as_deref(),
            ongoing,
            window_focused,
            submit,
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn notification_manager_native_cancel<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    id: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        withdraw(id, portal::submit);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn notification_manager_native_show_mpris<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _package_name: JString<'local>,
    _identity: JString<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        report_media_controls_unsupported();
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn notification_manager_native_cancel_mpris<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        report_media_controls_unsupported();
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::framework::fake_jvm;

    fn notice(id: i32, title: &str, body: &str) -> Notice {
        Notice {
            id: NoticeId::Client(id),
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    fn shown(
        title: Option<&str>,
        body: Option<&str>,
        ongoing: bool,
        window_focused: bool,
    ) -> Vec<PortalRequest> {
        let sent = RefCell::new(Vec::new());
        show(7, title, body, ongoing, window_focused, |request| {
            sent.borrow_mut().push(request);
            Ok(())
        });
        sent.into_inner()
    }

    fn notified(requests: Vec<PortalRequest>) -> Vec<Notice> {
        requests
            .into_iter()
            .map(|request| match request {
                PortalRequest::Notify(notice) => notice,
                _ => panic!("only notices are expected here"),
            })
            .collect()
    }

    #[test]
    fn the_natives_are_bound_with_the_api_impl_signatures() {
        fake_jvm::with_env(register_natives).expect("register the natives");
        for (name, sig) in [
            ("nativeInitBuilder", "()J"),
            (
                "nativeAddAction",
                "(JLjava/lang/String;ILandroid/content/Intent;)V",
            ),
            (
                "nativeShowNotification",
                "(JILjava/lang/String;Ljava/lang/String;Ljava/lang/String;ZILandroid/content/Intent;)V",
            ),
            ("nativeCancel", "(I)V"),
            (
                "nativeShowMPRIS",
                "(Ljava/lang/String;Ljava/lang/String;)V",
            ),
            ("nativeCancelMPRIS", "()V"),
        ] {
            assert!(
                fake_jvm::registered_native("android/app/NotificationManager", name, sig)
                    .is_some(),
                "NotificationManager.{name}{sig} is not bound"
            );
        }
    }

    #[test]
    fn a_notification_reaches_the_desktop_with_its_title_and_body() {
        assert_eq!(
            notified(shown(Some("Title"), Some("Body"), false, false)),
            [notice(7, "Title", "Body")]
        );
    }

    #[test]
    fn ongoing_notifications_and_a_focused_window_show_nothing() {
        for (ongoing, window_focused) in [(true, false), (false, true), (true, true)] {
            assert!(
                shown(Some("Title"), Some("Body"), ongoing, window_focused).is_empty(),
                "ongoing {ongoing}, focused {window_focused}"
            );
        }
        assert_eq!(
            desktop_notice(7, Some("Title"), None, true, true).err(),
            Some(NotShown::Ongoing)
        );
        assert_eq!(
            desktop_notice(7, Some("Title"), None, false, true).err(),
            Some(NotShown::WindowFocused)
        );
    }

    #[test]
    fn a_missing_title_becomes_roblox() {
        for title in [None, Some(""), Some(" \n\u{7}")] {
            assert_eq!(
                notified(shown(title, Some("Body"), false, false)),
                [notice(7, UNTITLED, "Body")],
                "{title:?}"
            );
        }
        assert_eq!(
            notified(shown(Some("Title"), None, false, false)),
            [notice(7, "Title", "")]
        );
    }

    #[test]
    fn a_notification_without_text_shows_nothing() {
        for (title, body) in [
            (None, None),
            (Some(""), None),
            (Some("\u{1b}"), Some(" \t")),
        ] {
            assert!(
                shown(title, body, false, false).is_empty(),
                "{title:?} {body:?}"
            );
            assert_eq!(
                desktop_notice(7, title, body, false, false).err(),
                Some(NotShown::NoText)
            );
        }
    }

    #[test]
    fn control_characters_are_removed_and_line_breaks_become_spaces() {
        assert_eq!(
            shown_text("a\u{0}b\u{1b}c\u{7f}d\u{9b}e", BODY_LIMIT),
            "abcde"
        );
        assert_eq!(
            shown_text("\none\ntwo\r\nthree\tfour\u{85}", BODY_LIMIT),
            "one two  three four"
        );
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let title = "é".repeat(TITLE_LIMIT + 1);
        let cut = shown_text(&title, TITLE_LIMIT);
        assert_eq!(cut.chars().count(), TITLE_LIMIT);
        assert_eq!(cut, format!("{}{CUT_MARK}", "é".repeat(TITLE_LIMIT - 1)));

        let body = "語".repeat(BODY_LIMIT + 50);
        let cut = shown_text(&body, BODY_LIMIT);
        assert_eq!(cut.chars().count(), BODY_LIMIT);
        assert!(cut.ends_with(CUT_MARK), "{cut}");

        let exact = "😀".repeat(TITLE_LIMIT);
        assert_eq!(shown_text(&exact, TITLE_LIMIT), exact);
    }

    #[test]
    fn a_cut_drops_the_trailing_space_before_the_mark() {
        let text = format!("{} {}", "a".repeat(TITLE_LIMIT - 2), "b".repeat(10));
        assert_eq!(
            shown_text(&text, TITLE_LIMIT),
            format!("{}{CUT_MARK}", "a".repeat(TITLE_LIMIT - 2))
        );
    }

    #[test]
    fn cancel_withdraws_the_client_notice() {
        let sent = RefCell::new(Vec::new());
        withdraw(7, |request| {
            sent.borrow_mut().push(request);
            Ok(())
        });
        assert!(matches!(
            sent.into_inner().as_slice(),
            [PortalRequest::Withdraw(NoticeId::Client(7))]
        ));
    }

    #[test]
    fn java_strings_reach_the_notice_and_null_ones_count_as_missing() {
        let sent = RefCell::new(Vec::new());
        fake_jvm::with_env(|env| {
            let title = env.new_string("Party invite").expect("title string");
            let null = JString::null();
            for (id, title) in [(3, &title), (4, &null)] {
                show_from_java(
                    fake_jvm::native_env(),
                    id,
                    title,
                    &null,
                    false,
                    false,
                    |request| {
                        sent.borrow_mut().push(request);
                        Ok(())
                    },
                );
            }
        });
        assert_eq!(fake_jvm::take_exception(), None);
        assert_eq!(notified(sent.into_inner()), [notice(3, "Party invite", "")]);
    }
}
