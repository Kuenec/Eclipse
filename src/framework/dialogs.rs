use std::panic::AssertUnwindSafe;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JObjectArray, JString};
use jni::refs::Global;
use jni::strings::JNIStr;
use jni::sys::{jboolean, jint, jlong};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use super::{checked, show_window_root, view_registry, window_registry, FrameworkError};
use crate::runtime::Vm;

pub(super) const DIALOG_CLASS: &JNIStr = jni_str!("android/app/Dialog");
pub(super) const ALERT_DIALOG_CLASS: &JNIStr = jni_str!("android/app/AlertDialog");

pub(super) const DIALOG_NATIVES: [(&JNIStr, &JNIStr); 6] = [
    (jni_str!("nativeInit"), jni_str!("()J")),
    (
        jni_str!("nativeSetTitle"),
        jni_str!("(JLjava/lang/String;)V"),
    ),
    (jni_str!("nativeSetContentView"), jni_str!("(JJ)V")),
    (jni_str!("nativeShow"), jni_str!("(J)V")),
    (jni_str!("nativeClose"), jni_str!("(J)V")),
    (jni_str!("nativeIsShowing"), jni_str!("(J)Z")),
];

pub(super) const ALERT_DIALOG_NATIVES: [(&JNIStr, &JNIStr); 3] = [
    (jni_str!("nativeConstruct"), jni_str!("(JJJJ)V")),
    (
        jni_str!("nativeSetMessage"),
        jni_str!("(JLjava/lang/String;)V"),
    ),
    (
        jni_str!("nativeSetItems"),
        jni_str!("(J[Ljava/lang/String;Landroid/content/DialogInterface$OnClickListener;)V"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    Click(view_registry::ViewHandle),
    Item(usize),
    Cancel,
}

fn report(what: &str, dialog: jlong, error: window_registry::WindowRegistryError) {
    tracing::error!(
        target: "android.app.Dialog",
        dialog,
        %error,
        "{what}: the dialog handle is not a live dialog"
    );
}

fn optional_string(env: &mut Env, text: &JString) -> jni::errors::Result<Option<String>> {
    if text.is_null() {
        return Ok(None);
    }
    text.try_to_string(env).map(Some)
}

extern "system" fn dialog_native_init<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let pruned = window_registry::retain_dialogs(|dialog| {
            !dialog.owner.is_garbage_collected(env).unwrap_or(false)
        });
        if let Err(error) = pruned {
            tracing::error!(target: "android.app.Dialog", %error, "collected dialogs could not be released");
        }
        let owner = env.new_weak_ref(&this)?;
        match window_registry::allocate_dialog(owner) {
            Ok(handle) => Ok(handle),
            Err(error) => {
                tracing::error!(target: "android.app.Dialog", %error, "Dialog.nativeInit: no dialog window could be allocated");
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn dialog_native_set_title<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
    title: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let title = optional_string(env, &title)?.unwrap_or_default();
        if let Err(error) = window_registry::with_window(dialog, |window| window.title = title) {
            report("Dialog.nativeSetTitle", dialog, error);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn dialog_native_set_content_view<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let root = view_registry::with_view(widget, |_view| ())
            .is_ok()
            .then_some(widget);
        if let Err(error) = set_dialog_root(dialog, root) {
            report("Dialog.nativeSetContentView", dialog, error);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

pub(super) fn set_dialog_root(
    dialog: window_registry::WindowHandle,
    root: Option<view_registry::ViewHandle>,
) -> Result<(), window_registry::WindowRegistryError> {
    let shown_root = window_registry::with_window(dialog, |window| {
        let shown = window.dialog.as_ref()?.shown.is_some();
        let previous = std::mem::replace(&mut window.root_view, root);
        Some(shown.then_some(previous))
    })?
    .ok_or(window_registry::WindowRegistryError::NotADialog)?;
    if let Some(previous) = shown_root.filter(|&previous| previous != root) {
        show_window_root(previous, false);
        show_window_root(root, true);
        crate::webview::client::refresh_visibility();
    }
    Ok(())
}

fn set_shown(
    dialog: window_registry::WindowHandle,
    shown: Option<Global<JObject<'static>>>,
) -> Result<(), window_registry::WindowRegistryError> {
    let showing = shown.is_some();
    let root = window_registry::with_window(dialog, |window| {
        window.dialog.as_mut()?.shown = shown;
        Some(window.root_view)
    })?
    .ok_or(window_registry::WindowRegistryError::NotADialog)?;
    show_window_root(root, showing);
    crate::webview::client::refresh_visibility();
    Ok(())
}

extern "system" fn dialog_native_show<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    dialog: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let shown = env.new_global_ref(&this)?;
        match set_shown(dialog, Some(shown)) {
            Ok(()) => tracing::info!(
                target: "android.app.Dialog",
                dialog,
                "Dialog.nativeShow: the dialog is shown in its own window"
            ),
            Err(error) => report("Dialog.nativeShow", dialog, error),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn dialog_native_close<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match set_shown(dialog, None) {
            Ok(()) => tracing::info!(
                target: "android.app.Dialog",
                dialog,
                "Dialog.nativeClose: the dialog window is closed"
            ),
            Err(error) => report("Dialog.nativeClose", dialog, error),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn dialog_native_is_showing<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
) -> jboolean {
    env.with_env(|_env| -> jni::errors::Result<jboolean> {
        match window_registry::with_dialog(dialog, |state| state.shown.is_some()) {
            Ok(showing) => Ok(showing),
            Err(error) => {
                report("Dialog.nativeIsShowing", dialog, error);
                Ok(false)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn alert_dialog_native_construct<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
    positive: jlong,
    negative: jlong,
    neutral: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(error) = window_registry::with_dialog(dialog, |state| {
            state.buttons = [positive, negative, neutral];
        }) {
            report("AlertDialog.nativeConstruct", dialog, error);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn alert_dialog_native_set_message<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
    message: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let message = optional_string(env, &message)?;
        if let Err(error) = window_registry::with_dialog(dialog, |state| state.message = message) {
            report("AlertDialog.nativeSetMessage", dialog, error);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn alert_dialog_native_set_items<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dialog: jlong,
    items: JObjectArray<'local, JString<'local>>,
    listener: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let mut labels = Vec::new();
        if !items.is_null() {
            for index in 0..items.len(env)? {
                let item = items.get_element(env, index)?;
                labels.push(optional_string(env, &item)?.unwrap_or_default());
            }
        }
        let listener = if listener.is_null() {
            None
        } else {
            Some(env.new_global_ref(&listener)?)
        };
        if let Err(error) = window_registry::with_dialog(dialog, |state| {
            state.items = labels;
            state.items_listener = listener;
        }) {
            report("AlertDialog.nativeSetItems", dialog, error);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

pub(super) fn register_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let dialog_functions: [*mut std::ffi::c_void; 6] = [
        dialog_native_init as *mut std::ffi::c_void,
        dialog_native_set_title as *mut std::ffi::c_void,
        dialog_native_set_content_view as *mut std::ffi::c_void,
        dialog_native_show as *mut std::ffi::c_void,
        dialog_native_close as *mut std::ffi::c_void,
        dialog_native_is_showing as *mut std::ffi::c_void,
    ];
    let alert_functions: [*mut std::ffi::c_void; 3] = [
        alert_dialog_native_construct as *mut std::ffi::c_void,
        alert_dialog_native_set_message as *mut std::ffi::c_void,
        alert_dialog_native_set_items as *mut std::ffi::c_void,
    ];
    for (class_name, natives, functions) in [
        (DIALOG_CLASS, &DIALOG_NATIVES[..], &dialog_functions[..]),
        (
            ALERT_DIALOG_CLASS,
            &ALERT_DIALOG_NATIVES[..],
            &alert_functions[..],
        ),
    ] {
        let class = env.find_class(class_name)?;
        let methods: Vec<NativeMethod> = natives
            .iter()
            .zip(functions)
            .map(|(&(name, sig), &function)| unsafe {
                NativeMethod::from_raw_parts(name, sig, function)
            })
            .collect();
        unsafe { env.register_native_methods(&class, &methods) }?;
    }
    tracing::info!(
        class = "android/app/Dialog",
        "registered Eclipse's dialog windows for Dialog and AlertDialog"
    );
    Ok(())
}

pub fn dispatch_dialog_action(
    vm: &Vm,
    dialog: window_registry::WindowHandle,
    action: DialogAction,
) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| perform(env, dialog, action))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

fn perform(
    env: &mut Env,
    dialog: window_registry::WindowHandle,
    action: DialogAction,
) -> Result<(), FrameworkError> {
    let (shown, listener) = window_registry::with_dialog(dialog, |state| {
        let shown = state
            .shown
            .as_ref()
            .map(|shown| env.new_local_ref(shown.as_obj()));
        let listener = state
            .items_listener
            .as_ref()
            .map(|listener| env.new_local_ref(listener.as_obj()));
        (shown, listener)
    })?;
    let Some(shown) = shown.transpose()? else {
        return Err(FrameworkError::DialogNotShowing(dialog));
    };
    match action {
        DialogAction::Click(view) => {
            let Ok(Ok(Some(target))) = view_registry::local_jobject(env, view) else {
                return Err(FrameworkError::DialogViewGone(view));
            };
            checked(env, "dialog View.performClick", |env| {
                env.call_method(&target, jni_str!("performClick"), jni_sig!("()Z"), &[])?
                    .z()
            })?;
        }
        DialogAction::Item(index) => {
            let Some(listener) = listener.transpose()? else {
                return Err(FrameworkError::DialogHasNoItems(dialog));
            };
            let which =
                jint::try_from(index).map_err(|_| FrameworkError::DialogItemOutOfRange(index))?;
            checked(
                env,
                "dialog DialogInterface.OnClickListener.onClick",
                |env| {
                    env.call_method(
                        &listener,
                        jni_str!("onClick"),
                        jni_sig!("(Landroid/content/DialogInterface;I)V"),
                        &[JValue::Object(&shown), JValue::Int(which)],
                    )?
                    .v()
                },
            )?;
        }
        DialogAction::Cancel => {
            checked(env, "Dialog.cancel", |env| {
                env.call_method(&shown, jni_str!("cancel"), jni_sig!("()V"), &[])?
                    .v()
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::fake_jvm;

    #[test]
    fn every_dialog_native_is_bound() {
        fake_jvm::with_env(register_natives).expect("register dialog natives");
        for (class, natives) in [
            ("android/app/Dialog", &DIALOG_NATIVES[..]),
            ("android/app/AlertDialog", &ALERT_DIALOG_NATIVES[..]),
        ] {
            for (name, sig) in natives {
                assert!(
                    fake_jvm::registered_native(class, &name.to_str(), &sig.to_str()).is_some(),
                    "{class}.{}{} is bound",
                    name.to_str(),
                    sig.to_str()
                );
            }
        }
    }

    #[test]
    fn a_dialog_content_view_is_the_root_its_window_draws() {
        fake_jvm::with_env(|env| {
            let owner = fake_jvm::new_object(env);
            let dialog = window_registry::allocate_dialog(env.new_weak_ref(&owner).expect("weak"))
                .expect("allocate dialog");
            let content = view_registry::allocate("android.widget.LinearLayout").expect("content");

            dialog_native_set_content_view(
                fake_jvm::native_env(),
                JObject::null(),
                dialog,
                content,
            );

            assert_eq!(
                window_registry::with_window(dialog, |window| window.root_view),
                Ok(Some(content))
            );
            view_registry::free(content).expect("free content");
            window_registry::free(dialog).expect("free dialog");
        });
    }

    #[test]
    fn a_dialog_content_view_stays_out_of_the_activity_tree() {
        fake_jvm::with_env(|env| {
            let owner = fake_jvm::new_object(env);
            let dialog = window_registry::allocate_dialog(env.new_weak_ref(&owner).expect("weak"))
                .expect("allocate dialog");
            let decor = view_registry::allocate("android.widget.FrameLayout").expect("decor");

            super::super::window_set_widget_as_root(
                fake_jvm::native_env(),
                JObject::null(),
                dialog,
                decor,
            );

            assert_ne!(view_registry::active_root(), decor);
            assert_eq!(
                window_registry::with_window(dialog, |window| window.root_view),
                Ok(Some(decor))
            );
            view_registry::free(decor).expect("free decor");
        });
    }

    fn dialog_with_web_view(env: &mut Env, dialog_object: &JObject) -> (jlong, jlong, jlong) {
        let dialog = dialog_native_init(
            fake_jvm::native_env(),
            env.new_local_ref(dialog_object).expect("receiver"),
        );
        let decor = view_registry::allocate("android.widget.FrameLayout").expect("decor");
        let web = view_registry::allocate("android.webkit.WebView").expect("WebView");
        view_registry::attach_child(decor, web, None).expect("attach WebView");
        (dialog, decor, web)
    }

    #[test]
    fn a_web_view_in_a_dialog_is_shown_only_while_the_dialog_shows() {
        fake_jvm::with_env(|env| {
            let dialog_object = fake_jvm::new_object(env);
            let (dialog, decor, web) = dialog_with_web_view(env, &dialog_object);
            super::super::window_set_widget_as_root(
                fake_jvm::native_env(),
                JObject::null(),
                dialog,
                decor,
            );
            let before_show = view_registry::is_shown(web);
            dialog_native_show(
                fake_jvm::native_env(),
                env.new_local_ref(&dialog_object).expect("receiver"),
                dialog,
            );
            let while_shown = view_registry::is_shown(web);
            dialog_native_close(fake_jvm::native_env(), JObject::null(), dialog);
            let after_close = view_registry::is_shown(web);

            assert_eq!(
                (before_show, while_shown, after_close),
                (false, true, false),
                "a dialog's WebView has a window only between Dialog.show and Dialog.dismiss"
            );
            for view in [web, decor] {
                view_registry::free(view).expect("free view");
            }
            window_registry::free(dialog).expect("free dialog");
        });
    }

    #[test]
    fn a_web_view_set_into_a_shown_dialog_is_shown_with_it() {
        fake_jvm::with_env(|env| {
            let dialog_object = fake_jvm::new_object(env);
            let (dialog, decor, web) = dialog_with_web_view(env, &dialog_object);
            dialog_native_show(
                fake_jvm::native_env(),
                env.new_local_ref(&dialog_object).expect("receiver"),
                dialog,
            );
            super::super::window_set_widget_as_root(
                fake_jvm::native_env(),
                JObject::null(),
                dialog,
                decor,
            );
            let shown = view_registry::is_shown(web);
            dialog_native_close(fake_jvm::native_env(), JObject::null(), dialog);

            assert!(
                shown,
                "content set after Dialog.show is shown while the dialog shows"
            );
            assert!(!view_registry::is_shown(web));
            for view in [web, decor] {
                view_registry::free(view).expect("free view");
            }
            window_registry::free(dialog).expect("free dialog");
        });
    }

    #[test]
    fn dialog_actions_reach_the_button_the_item_listener_and_cancel() {
        fake_jvm::with_env(|env| {
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let record = |object: &JObject, method: &'static str| {
                let calls = std::sync::Arc::clone(&calls);
                fake_jvm::on_call(object, method, move || {
                    calls.lock().expect("calls").push(method);
                });
            };
            let owner = fake_jvm::new_object(env);
            record(&owner, "cancel");
            let listener = fake_jvm::new_object(env);
            record(&listener, "onClick");
            let button_object = fake_jvm::new_object(env);
            record(&button_object, "performClick");
            let button = view_registry::allocate("android.widget.Button").expect("button");
            view_registry::set_jobject(
                button,
                view_registry::ViewObject::Owned(env.new_global_ref(&button_object).expect("ref")),
            )
            .expect("attach button object");

            let dialog = window_registry::allocate_dialog(env.new_weak_ref(&owner).expect("weak"))
                .expect("allocate dialog");
            assert_eq!(
                perform(env, dialog, DialogAction::Cancel).map_err(|e| e.to_string()),
                Err(FrameworkError::DialogNotShowing(dialog).to_string()),
                "a hidden dialog takes no actions"
            );
            let shown = env.new_global_ref(&owner).expect("shown");
            let items = env.new_global_ref(&listener).expect("listener");
            window_registry::with_dialog(dialog, |state| {
                state.shown = Some(shown);
                state.items_listener = Some(items);
            })
            .expect("show dialog");

            perform(env, dialog, DialogAction::Click(button)).expect("click");
            perform(env, dialog, DialogAction::Item(0)).expect("item");
            perform(env, dialog, DialogAction::Cancel).expect("cancel");
            assert_eq!(
                *calls.lock().expect("calls"),
                ["performClick", "onClick", "cancel"]
            );

            window_registry::with_dialog(dialog, |state| state.shown = None).expect("close");
            view_registry::free(button).expect("free button");
        });
    }

    #[test]
    fn collected_dialogs_release_their_windows() {
        fake_jvm::with_env(|env| {
            let owner = fake_jvm::new_object(env);
            let dialog = window_registry::allocate_dialog(env.new_weak_ref(&owner).expect("weak"))
                .expect("allocate dialog");
            assert!(fake_jvm::collect(&owner));

            window_registry::retain_dialogs(|state| {
                !state.owner.is_garbage_collected(env).unwrap_or(false)
            })
            .expect("prune");
            assert_eq!(
                window_registry::with_window(dialog, |_| ()),
                Err(window_registry::WindowRegistryError::StaleHandle)
            );
        });
    }

    #[test]
    fn a_shown_dialog_reports_showing_until_it_is_closed() {
        fake_jvm::with_env(|env| {
            let dialog_object = fake_jvm::new_object(env);
            let dialog = dialog_native_init(
                fake_jvm::native_env(),
                env.new_local_ref(&dialog_object).expect("receiver"),
            );
            assert_ne!(dialog, 0);
            assert_ne!(
                window_registry::active_window(),
                dialog,
                "a dialog never becomes the activity window"
            );
            let message = env.new_string("You were kicked").expect("message");
            alert_dialog_native_set_message(
                fake_jvm::native_env(),
                JObject::null(),
                dialog,
                message,
            );
            let showing = |env: &mut Env| {
                dialog_native_is_showing(
                    fake_jvm::native_env(),
                    env.new_local_ref(&dialog_object).expect("receiver"),
                    dialog,
                )
            };

            assert!(!showing(env));
            dialog_native_show(
                fake_jvm::native_env(),
                env.new_local_ref(&dialog_object).expect("receiver"),
                dialog,
            );
            assert!(showing(env));
            let listed = window_registry::showing_dialogs().expect("dialogs");
            let view = listed
                .iter()
                .find(|view| view.handle == dialog)
                .expect("the dialog is listed while shown");
            assert_eq!(view.message.as_deref(), Some("You were kicked"));

            dialog_native_close(fake_jvm::native_env(), JObject::null(), dialog);
            assert!(!showing(env));
            assert!(window_registry::showing_dialogs()
                .expect("dialogs")
                .iter()
                .all(|view| view.handle != dialog));
            assert_eq!(fake_jvm::take_exception(), None);
        });
    }
}
