use std::sync::{Mutex, PoisonError};

use jni::objects::JObject;
use jni::refs::Weak;
use jni::sys::jint;
use jni::Env;

use crate::apk::res_config::ResConfig;

const ATL_INITIAL_DENSITY: u16 = 160;

static CONFIGURATIONS: Mutex<Vec<(Weak<JObject<'static>>, ResConfig)>> = Mutex::new(Vec::new());

pub(super) struct JavaConfiguration<'a> {
    pub(super) mcc: jint,
    pub(super) mnc: jint,
    pub(super) locale: Option<&'a str>,
    pub(super) orientation: jint,
    pub(super) touchscreen: jint,
    pub(super) density: jint,
    pub(super) keyboard: jint,
    pub(super) keyboard_hidden: jint,
    pub(super) navigation: jint,
    pub(super) screen_width: jint,
    pub(super) screen_height: jint,
    pub(super) smallest_screen_width_dp: jint,
    pub(super) screen_width_dp: jint,
    pub(super) screen_height_dp: jint,
    pub(super) screen_layout: jint,
    pub(super) ui_mode: jint,
    pub(super) sdk_version: jint,
}

fn field<T: TryFrom<jint> + Default>(value: jint) -> T {
    T::try_from(value).unwrap_or_default()
}

impl JavaConfiguration<'_> {
    pub(super) fn to_res_config(&self) -> ResConfig {
        let mut config = ResConfig {
            mcc: field(self.mcc),
            mnc: field(self.mnc),
            orientation: field(self.orientation),
            touchscreen: field(self.touchscreen),
            density: field(self.density),
            keyboard: field(self.keyboard),
            navigation: field(self.navigation),
            input_flags: field(self.keyboard_hidden),
            screen_width: field(self.screen_width),
            screen_height: field(self.screen_height),
            smallest_screen_width_dp: field(self.smallest_screen_width_dp),
            screen_width_dp: field(self.screen_width_dp),
            screen_height_dp: field(self.screen_height_dp),
            screen_layout: field(self.screen_layout & 0xFF),
            screen_layout2: field((self.screen_layout >> 8) & 0x03),
            ui_mode: field(self.ui_mode),
            sdk_version: field(self.sdk_version),
            ..ResConfig::default()
        };
        if let Some(locale) = self.locale {
            config.set_locale(locale);
        }
        config
    }
}

pub(super) fn initial(sdk_version: jint) -> ResConfig {
    ResConfig {
        density: ATL_INITIAL_DENSITY,
        sdk_version: field(sdk_version),
        ..ResConfig::default()
    }
}

pub(super) fn record(env: &Env, manager: &JObject, config: ResConfig) -> jni::errors::Result<()> {
    let mut configurations = CONFIGURATIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    configurations.retain(|(weak, _)| !weak.is_garbage_collected(env).unwrap_or(false));
    for (weak, stored) in configurations.iter_mut() {
        if env.is_same_object(&*weak, manager)? {
            *stored = config;
            return Ok(());
        }
    }
    configurations.push((env.new_weak_ref(manager)?, config));
    Ok(())
}

pub(super) fn of(env: &Env, manager: &JObject) -> ResConfig {
    let configurations = CONFIGURATIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    configurations
        .iter()
        .find(|(weak, _)| env.is_same_object(weak, manager).unwrap_or(false))
        .map_or_else(ResConfig::default, |(_, config)| *config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::fake_jvm;

    fn java_configuration(locale: Option<&str>) -> JavaConfiguration<'_> {
        JavaConfiguration {
            mcc: 0,
            mnc: 0,
            locale,
            orientation: 0,
            touchscreen: 0,
            density: 0,
            keyboard: 0,
            keyboard_hidden: 0,
            navigation: 0,
            screen_width: 1920,
            screen_height: 1080,
            smallest_screen_width_dp: 0,
            screen_width_dp: 1280,
            screen_height_dp: 720,
            screen_layout: 0x0200 | 0x40 | 0x03,
            ui_mode: 0,
            sdk_version: 28,
        }
    }

    #[test]
    fn the_java_configuration_becomes_the_resource_request() {
        let config = java_configuration(Some("de-DE")).to_res_config();
        assert_eq!(config.language, *b"de");
        assert_eq!(config.country, *b"DE");
        assert_eq!(config.screen_width_dp, 1280);
        assert_eq!(config.sdk_version, 28);
        assert_eq!(config.screen_layout, 0x43);
        assert_eq!(config.screen_layout2, 0x02);

        let negative = JavaConfiguration {
            density: -1,
            ..java_configuration(None)
        };
        assert_eq!(negative.to_res_config().density, 0);
        assert_eq!(negative.to_res_config().language, [0, 0]);
    }

    #[test]
    fn each_asset_manager_keeps_its_own_configuration() {
        fake_jvm::with_env(|env| {
            let app = fake_jvm::new_object(env);
            let system = fake_jvm::new_object(env);
            let german = java_configuration(Some("de-DE")).to_res_config();
            record(env, &app, initial(28)).expect("init app");
            record(env, &system, initial(28)).expect("init system");
            record(env, &app, german).expect("configure app");

            assert_eq!(of(env, &app), german);
            assert_eq!(of(env, &system), initial(28));
            assert_eq!(of(env, &JObject::null()), ResConfig::default());
        });
    }
}
