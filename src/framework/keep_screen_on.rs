use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::errors::LogErrorAndDefault;
use jni::objects::JClass;
use jni::strings::JNIStr;
use jni::sys::jboolean;
use jni::{jni_str, Env, EnvUnowned};

use super::{
    register_class_natives_best_effort, wake_main_looper, FrameworkError, NativeBinding,
    WINDOW_CLASS,
};
use crate::portal::{self, IdleInhibit, PortalError, PortalRequest};

const SET_KEEP_SCREEN_ON_NAME: &JNIStr = jni_str!("nativeSetKeepScreenOn");
const SET_KEEP_SCREEN_ON_SIG: &JNIStr = jni_str!("(Z)V");

static CLIENT_KEEPS_SCREEN_ON: AtomicBool = AtomicBool::new(false);

pub(super) fn client_keeps_screen_on() -> bool {
    CLIENT_KEEPS_SCREEN_ON.load(Ordering::Acquire)
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let bindings: [NativeBinding; 1] = [(
        SET_KEEP_SCREEN_ON_NAME,
        SET_KEEP_SCREEN_ON_SIG,
        window_native_set_keep_screen_on as *mut c_void,
    )];
    register_class_natives_best_effort(env, WINDOW_CLASS, &bindings)?;
    Ok(())
}

extern "system" fn window_native_set_keep_screen_on<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    keep_screen_on: jboolean,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if CLIENT_KEEPS_SCREEN_ON.swap(keep_screen_on, Ordering::AcqRel) != keep_screen_on {
            tracing::info!(
                target: "android.view.Window",
                keep_screen_on,
                "the client changed FLAG_KEEP_SCREEN_ON"
            );
            wake_main_looper();
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

pub(crate) fn sync_idle_inhibit(held: &mut IdleInhibit, window_focused: bool) {
    apply_idle_inhibit(
        held,
        client_keeps_screen_on(),
        window_focused,
        portal::submit,
    );
}

fn apply_idle_inhibit(
    held: &mut IdleInhibit,
    client_keeps_screen_on: bool,
    window_focused: bool,
    submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>,
) {
    let wanted = if client_keeps_screen_on && window_focused {
        IdleInhibit::Hold
    } else {
        IdleInhibit::Release
    };
    if wanted == *held {
        return;
    }
    *held = wanted;
    match wanted {
        IdleInhibit::Hold => {
            tracing::info!("keeping the screen awake while Roblox asks for it and has focus");
        }
        IdleInhibit::Release => tracing::info!("letting the screen idle again"),
    }
    if let Err(error) = submit(PortalRequest::IdleInhibit(wanted)) {
        tracing::warn!(%error, ?wanted, "could not change the desktop idle inhibit");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn requested_inhibits(
        steps: &[(bool, bool)],
        answer: fn() -> Result<(), PortalError>,
    ) -> (Vec<IdleInhibit>, IdleInhibit) {
        let mut held = IdleInhibit::Release;
        let sent = RefCell::new(Vec::new());
        for &(client_flag, focused) in steps {
            apply_idle_inhibit(&mut held, client_flag, focused, |request| {
                let PortalRequest::IdleInhibit(step) = request else {
                    panic!("only idle inhibit requests are expected here");
                };
                sent.borrow_mut().push(step);
                answer()
            });
        }
        (sent.into_inner(), held)
    }

    #[test]
    fn the_native_matches_the_overlay_declaration() {
        let generator = include_str!("../../tools/framework-overlay/patch-framework.sh");
        let declaration = format!(
            ".method private static native {}{}",
            SET_KEEP_SCREEN_ON_NAME.to_str(),
            SET_KEEP_SCREEN_ON_SIG.to_str()
        );
        assert!(generator.contains(&declaration), "{declaration}");
        assert_eq!(WINDOW_CLASS.to_str(), "android/view/Window");
    }

    #[test]
    fn the_screen_stays_awake_only_while_the_client_asks_and_the_window_has_focus() {
        for (client_flag, focused, sent) in [
            (false, false, &[][..]),
            (false, true, &[][..]),
            (true, false, &[][..]),
            (true, true, &[IdleInhibit::Hold][..]),
        ] {
            assert_eq!(
                requested_inhibits(&[(client_flag, focused); 3], || Ok(())).0,
                sent,
                "client flag {client_flag}, focused {focused}"
            );
        }
    }

    #[test]
    fn each_change_is_requested_once() {
        let steps = [
            (true, true),
            (true, true),
            (true, false),
            (true, false),
            (true, true),
            (false, true),
            (false, true),
            (false, false),
            (true, true),
            (false, false),
        ];
        assert_eq!(
            requested_inhibits(&steps, || Ok(())),
            (
                vec![
                    IdleInhibit::Hold,
                    IdleInhibit::Release,
                    IdleInhibit::Hold,
                    IdleInhibit::Release,
                    IdleInhibit::Hold,
                    IdleInhibit::Release,
                ],
                IdleInhibit::Release
            )
        );
    }

    #[test]
    fn a_refused_request_is_recorded_and_not_retried() {
        let steps = [(true, true), (true, true), (true, true), (false, true)];
        assert_eq!(
            requested_inhibits(&steps, || Err(PortalError::Stopped)),
            (
                vec![IdleInhibit::Hold, IdleInhibit::Release],
                IdleInhibit::Release
            )
        );
    }
}
