use eclipse_config::audio::Direction;

pub mod aaudio;
pub mod bionic_env;
pub mod bionic_locale;
pub mod bionic_pthread;
pub mod bionic_sysconf;
pub mod dlfcn;
mod egl_seam;
pub mod elf;
pub mod engine;
pub mod frame_log;
pub mod init_run;
pub mod jni_mangle;
pub mod jni_register;
pub mod link;
#[cfg(test)]
pub(crate) mod log_capture;
pub mod looper;
pub mod map;
pub mod mediacodec;
pub mod module_registry;
pub mod native_provider;
pub mod ndk_registry;
pub mod opensl;
pub(crate) mod present_pacing;
pub mod reloc;
pub mod resolve;
mod text_overlay;
pub mod tls;
pub mod vk_overlay;
pub mod vulkan_wsi;

pub fn reopen_audio_streams(direction: Direction) {
    aaudio::reopen_streams(direction);
    match direction {
        Direction::Output => opensl::reopen_players(),
        Direction::Input => {}
    }
}
