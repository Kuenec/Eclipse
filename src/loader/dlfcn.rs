use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::OnceLock;

use super::native_provider::{last_dl_error, EclipseNativeProvider};
use super::resolve::SymbolProvider;
use super::vulkan_wsi;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformLibrary {
    OpenSles,
    AAudio,
}

static PLATFORM_LIBRARIES: [PlatformLibrary; 2] =
    [PlatformLibrary::OpenSles, PlatformLibrary::AAudio];

impl PlatformLibrary {
    fn from_path(path: &CStr) -> Option<Self> {
        let file_name = path.to_bytes().rsplit(|&b| b == b'/').next()?;
        match file_name {
            b"libOpenSLES.so" => Some(Self::OpenSles),
            b"libaaudio.so" => Some(Self::AAudio),
            _ => None,
        }
    }

    fn handle(self) -> *mut c_void {
        std::ptr::from_ref(&PLATFORM_LIBRARIES[self as usize])
            .cast_mut()
            .cast()
    }

    fn from_handle(handle: *mut c_void) -> Option<Self> {
        PLATFORM_LIBRARIES
            .iter()
            .copied()
            .find(|library| library.handle() == handle)
    }

    fn exports(self, name: &str) -> bool {
        match self {
            Self::OpenSles => name.starts_with("sl") || name.starts_with("SL_IID_"),
            Self::AAudio => name.starts_with("AAudio"),
        }
    }

    fn symbol(self, name: &CStr) -> *mut c_void {
        static NATIVES: OnceLock<EclipseNativeProvider> = OnceLock::new();
        let Ok(name) = name.to_str() else {
            return std::ptr::null_mut();
        };
        if !self.exports(name) {
            return std::ptr::null_mut();
        }
        NATIVES
            .get_or_init(EclipseNativeProvider::with_bionic_natives)
            .resolve(name)
            .map_or(std::ptr::null_mut(), |sym| sym.addr as *mut c_void)
    }
}

pub(crate) unsafe extern "C" fn eclipse_dlopen(
    filename: *const c_char,
    flags: c_int,
) -> *mut c_void {
    let name = (!filename.is_null()).then(|| unsafe { CStr::from_ptr(filename) });
    if let Some(library) = name.and_then(PlatformLibrary::from_path) {
        return library.handle();
    }
    let handle = unsafe { libc::dlopen(filename, flags) };
    if handle.is_null() && tracing::enabled!(target: "dlfcn", tracing::Level::DEBUG) {
        return unsafe { log_failure_and_rearm_dlerror(name, flags) };
    }
    handle
}

unsafe fn log_failure_and_rearm_dlerror(name: Option<&CStr>, flags: c_int) -> *mut c_void {
    tracing::debug!(
        target: "dlfcn",
        ?name,
        flags,
        reason = %last_dl_error(),
        "host dlopen failed"
    );
    unsafe { libc::dlopen(name.map_or(std::ptr::null(), CStr::as_ptr), flags) }
}

pub(crate) unsafe extern "C" fn eclipse_dlclose(handle: *mut c_void) -> c_int {
    if PlatformLibrary::from_handle(handle).is_some() {
        return 0;
    }
    unsafe { libc::dlclose(handle) }
}

pub(crate) unsafe extern "C" fn eclipse_dlsym(
    handle: *mut c_void,
    symbol: *const c_char,
) -> *mut c_void {
    if symbol.is_null() {
        return unsafe { libc::dlsym(handle, symbol) };
    }
    let name = unsafe { CStr::from_ptr(symbol) };
    if let Some(library) = PlatformLibrary::from_handle(handle) {
        return library.symbol(name);
    }
    if name == c"vkGetInstanceProcAddr" {
        vulkan_wsi::eclipse_vk_get_instance_proc_addr as *const () as *mut c_void
    } else if name == c"vkCreateInstance" {
        vulkan_wsi::eclipse_vk_create_instance as *const () as *mut c_void
    } else if name == c"vkCreateAndroidSurfaceKHR" {
        vulkan_wsi::eclipse_vk_create_android_surface_khr as *const () as *mut c_void
    } else {
        unsafe { libc::dlsym(handle, symbol) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type DlopenFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
    type DlsymFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
    type DlcloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;

    const OPENSLES_NAMES: [&CStr; 6] = [
        c"slCreateEngine",
        c"SL_IID_ENGINE",
        c"SL_IID_ANDROIDSIMPLEBUFFERQUEUE",
        c"SL_IID_ANDROIDCONFIGURATION",
        c"SL_IID_PLAY",
        c"SL_IID_RECORD",
    ];

    const AAUDIO_NAMES: [&CStr; 26] = [
        c"AAudio_createStreamBuilder",
        c"AAudioStreamBuilder_openStream",
        c"AAudioStreamBuilder_setBufferCapacityInFrames",
        c"AAudioStreamBuilder_setDirection",
        c"AAudioStreamBuilder_setPerformanceMode",
        c"AAudioStreamBuilder_setDataCallback",
        c"AAudioStreamBuilder_setErrorCallback",
        c"AAudioStreamBuilder_delete",
        c"AAudioStream_getFramesPerBurst",
        c"AAudioStream_getBufferCapacityInFrames",
        c"AAudioStream_getBufferSizeInFrames",
        c"AAudioStream_setBufferSizeInFrames",
        c"AAudioStream_getFormat",
        c"AAudioStream_getSampleRate",
        c"AAudioStream_getState",
        c"AAudioStream_getChannelCount",
        c"AAudioStream_getXRunCount",
        c"AAudioStream_requestStart",
        c"AAudioStream_requestPause",
        c"AAudioStream_requestStop",
        c"AAudioStream_close",
        c"AAudioStream_read",
        c"AAudioStream_waitForStateChange",
        c"AAudioStreamBuilder_setFormat",
        c"AAudioStreamBuilder_setUsage",
        c"AAudioStreamBuilder_setInputPreset",
    ];

    struct Dl {
        provider: EclipseNativeProvider,
        open: DlopenFn,
        sym: DlsymFn,
        close: DlcloseFn,
    }

    fn dl_through_provider() -> Dl {
        let provider = EclipseNativeProvider::with_bionic_natives();
        let addr = |name: &str| {
            provider
                .resolve(name)
                .unwrap_or_else(|| panic!("{name} must be an Eclipse native"))
                .addr as usize as *const ()
        };
        let (open, sym, close) = unsafe {
            (
                std::mem::transmute::<*const (), DlopenFn>(addr("dlopen")),
                std::mem::transmute::<*const (), DlsymFn>(addr("dlsym")),
                std::mem::transmute::<*const (), DlcloseFn>(addr("dlclose")),
            )
        };
        Dl {
            provider,
            open,
            sym,
            close,
        }
    }

    fn provider_addr(dl: &Dl, name: &CStr) -> *mut c_void {
        dl.provider
            .resolve(name.to_str().unwrap())
            .map_or(std::ptr::null_mut(), |sym| sym.addr as *mut c_void)
    }

    #[test]
    fn opensles_soname_serves_eclipse_opensl_symbols() {
        let dl = dl_through_provider();
        let handle = unsafe { (dl.open)(c"libOpenSLES.so".as_ptr(), libc::RTLD_LAZY) };
        assert!(
            !handle.is_null(),
            "libOpenSLES.so must open without a host library"
        );

        for name in OPENSLES_NAMES {
            let found = unsafe { (dl.sym)(handle, name.as_ptr()) };
            assert!(!found.is_null(), "{name:?} must resolve");
            assert_eq!(found, provider_addr(&dl, name), "{name:?} is Eclipse's");
        }
        assert!(
            unsafe { (dl.sym)(handle, c"AAudio_createStreamBuilder".as_ptr()) }.is_null(),
            "libOpenSLES.so does not export AAudio"
        );
        assert_eq!(unsafe { (dl.close)(handle) }, 0);
    }

    #[test]
    fn aaudio_soname_serves_every_symbol_fmod_requires() {
        let dl = dl_through_provider();
        let handle = unsafe { (dl.open)(c"libaaudio.so".as_ptr(), libc::RTLD_LAZY) };
        assert!(
            !handle.is_null(),
            "libaaudio.so must open without a host library"
        );

        for name in AAUDIO_NAMES {
            let found = unsafe { (dl.sym)(handle, name.as_ptr()) };
            assert!(!found.is_null(), "{name:?} must resolve");
            assert_eq!(found, provider_addr(&dl, name), "{name:?} is Eclipse's");
        }
        assert!(
            unsafe { (dl.sym)(handle, c"slCreateEngine".as_ptr()) }.is_null(),
            "libaaudio.so does not export OpenSL ES"
        );
        assert_eq!(unsafe { (dl.close)(handle) }, 0);
    }

    #[test]
    fn platform_handles_are_stable_and_distinct_and_accept_full_paths() {
        let dl = dl_through_provider();
        let opensles = unsafe { (dl.open)(c"libOpenSLES.so".as_ptr(), libc::RTLD_NOW) };
        let aaudio = unsafe { (dl.open)(c"libaaudio.so".as_ptr(), libc::RTLD_NOW) };
        let aaudio_path =
            unsafe { (dl.open)(c"/system/lib64/libaaudio.so".as_ptr(), libc::RTLD_NOW) };
        assert_ne!(opensles, aaudio);
        assert_eq!(aaudio, aaudio_path);
        assert_eq!(
            opensles,
            unsafe { (dl.open)(c"libOpenSLES.so".as_ptr(), libc::RTLD_LAZY) },
            "reopening yields the same handle"
        );
    }

    #[test]
    fn other_libraries_keep_host_behaviour() {
        let dl = dl_through_provider();
        assert!(unsafe {
            (dl.open)(
                c"libeclipse_definitely_no_such_lib_77c1.so".as_ptr(),
                libc::RTLD_NOW,
            )
        }
        .is_null());
        assert!(unsafe { (dl.open)(c"libaaudio.so.1".as_ptr(), libc::RTLD_NOW) }.is_null());

        let libc_handle = unsafe { (dl.open)(c"libc.so.6".as_ptr(), libc::RTLD_NOW) };
        assert!(!libc_handle.is_null());
        let host_strlen = unsafe { libc::dlsym(libc_handle, c"strlen".as_ptr()) };
        assert_eq!(
            unsafe { (dl.sym)(libc_handle, c"strlen".as_ptr()) },
            host_strlen
        );
        assert_eq!(
            unsafe { (dl.sym)(libc_handle, c"vkCreateInstance".as_ptr()) },
            vulkan_wsi::eclipse_vk_create_instance as *const () as *mut c_void,
            "Vulkan entry points stay routed through Eclipse's WSI"
        );
        assert_eq!(unsafe { (dl.close)(libc_handle) }, 0);
    }

    #[test]
    fn failed_host_dlopen_logs_its_cause_and_leaves_it_for_dlerror() {
        let dl = dl_through_provider();
        let absent = c"libeclipse_absent_media_3b9e.so";
        let mut caller_reason = String::new();
        let log = crate::loader::log_capture::formatted_log("dlfcn=debug", || {
            assert!(unsafe { (dl.open)(absent.as_ptr(), libc::RTLD_NOW) }.is_null());
            caller_reason = last_dl_error();
        });

        let line = log
            .lines()
            .find(|line| line.contains("host dlopen failed"))
            .unwrap_or_else(|| panic!("no dlopen failure line in {log:?}"));
        assert!(line.contains("DEBUG dlfcn:"), "{line}");
        assert!(line.contains(&format!("name=Some({absent:?})")), "{line}");
        assert!(
            line.contains("reason=libeclipse_absent_media_3b9e.so: cannot open shared object"),
            "{line}"
        );
        assert!(
            caller_reason.contains("libeclipse_absent_media_3b9e.so")
                && caller_reason.contains("cannot open shared object"),
            "the caller's dlerror() must still report the failure after logging: {caller_reason}"
        );
    }
}
