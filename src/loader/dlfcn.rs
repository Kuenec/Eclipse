use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

use super::native_provider::{last_dl_error, EclipseNativeProvider};
use super::resolve::SymbolProvider;
use super::vulkan_wsi;
use crate::gpu::{client_graphics, Graphics};

const VULKAN_LOADERS: [&[u8]; 2] = [b"libvulkan.so", b"libvulkan.so.1"];

thread_local! {
    static REFUSAL: RefCell<Refusal> = const {
        RefCell::new(Refusal {
            pending: None,
            reported: None,
        })
    };
}

struct Refusal {
    pending: Option<CString>,
    reported: Option<CString>,
}

fn file_name(path: &CStr) -> &[u8] {
    path.to_bytes()
        .rsplit(|&b| b == b'/')
        .next()
        .unwrap_or_default()
}

fn hides_library(path: &CStr, graphics: Graphics) -> bool {
    graphics.hides_vulkan() && VULKAN_LOADERS.contains(&file_name(path))
}

fn hides_symbol(name: &CStr, graphics: Graphics) -> bool {
    graphics.hides_vulkan() && name.to_bytes().starts_with(b"vk")
}

fn refuse(name: &CStr, reason: &str) {
    tracing::debug!(
        target: "dlfcn",
        ?name,
        reason,
        "Vulkan is hidden from Roblox, which uses OpenGL ES"
    );
    unsafe { libc::dlerror() };
    let mut message = name.to_bytes().to_vec();
    message.extend_from_slice(b": ");
    message.extend_from_slice(reason.as_bytes());
    let message = CString::new(message).expect("a C string name and a reason hold no NUL");
    if REFUSAL
        .try_with(|refusal| refusal.borrow_mut().pending = Some(message))
        .is_err()
    {
        tracing::debug!(
            target: "dlfcn",
            ?name,
            "the refusal outlived this thread's dlerror state"
        );
    }
}

fn forget_refusal() {
    let _nothing_is_pending_after_thread_teardown =
        REFUSAL.try_with(|refusal| refusal.borrow_mut().pending = None);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformLibrary {
    OpenSles,
    AAudio,
}

static PLATFORM_LIBRARIES: [PlatformLibrary; 2] =
    [PlatformLibrary::OpenSles, PlatformLibrary::AAudio];

impl PlatformLibrary {
    fn from_path(path: &CStr) -> Option<Self> {
        match file_name(path) {
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
    if let Some(name) = name.filter(|name| hides_library(name, client_graphics())) {
        refuse(
            name,
            "cannot open shared object file: No such file or directory",
        );
        return std::ptr::null_mut();
    }
    forget_refusal();
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
    forget_refusal();
    if PlatformLibrary::from_handle(handle).is_some() {
        return 0;
    }
    unsafe { libc::dlclose(handle) }
}

pub(crate) unsafe extern "C" fn eclipse_dlerror() -> *mut c_char {
    let refused = REFUSAL.try_with(|refusal| {
        let mut refusal = refusal.borrow_mut();
        let message = refusal.pending.take()?;
        Some(refusal.reported.insert(message).as_ptr().cast_mut())
    });
    match refused {
        Ok(Some(message)) => message,
        Ok(None) | Err(_) => unsafe { libc::dlerror() },
    }
}

pub(crate) unsafe extern "C" fn eclipse_dlsym(
    handle: *mut c_void,
    symbol: *const c_char,
) -> *mut c_void {
    if symbol.is_null() {
        forget_refusal();
        return unsafe { libc::dlsym(handle, symbol) };
    }
    let name = unsafe { CStr::from_ptr(symbol) };
    if hides_symbol(name, client_graphics()) {
        refuse(name, "undefined symbol");
        return std::ptr::null_mut();
    }
    forget_refusal();
    if let Some(library) = PlatformLibrary::from_handle(handle) {
        return library.symbol(name);
    }
    if name == c"vkGetInstanceProcAddr" {
        vulkan_wsi::eclipse_vk_get_instance_proc_addr as *const () as *mut c_void
    } else if name == c"vkCreateInstance" {
        vulkan_wsi::eclipse_vk_create_instance as *const () as *mut c_void
    } else if name == c"vkCreateAndroidSurfaceKHR" {
        vulkan_wsi::eclipse_vk_create_android_surface_khr as *const () as *mut c_void
    } else if name == c"vkEnumeratePhysicalDevices" {
        vulkan_wsi::eclipse_vk_enumerate_physical_devices as *const () as *mut c_void
    } else if name == c"vkEnumeratePhysicalDeviceGroups" {
        vulkan_wsi::eclipse_vk_enumerate_physical_device_groups as *const () as *mut c_void
    } else {
        unsafe { libc::dlsym(handle, symbol) }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ash::vk;

    use super::*;
    use crate::gpu::GlesReason;

    type DlopenFn = unsafe extern "C" fn(*const c_char, c_int) -> *mut c_void;
    type DlsymFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
    type DlcloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;
    type DlerrorFn = unsafe extern "C" fn() -> *mut c_char;

    const HIDDEN_VULKAN_CHILD: &str = "ECLIPSE_TEST_HIDDEN_VULKAN_CHILD";

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
        assert_eq!(
            unsafe { (dl.sym)(libc_handle, c"vkEnumeratePhysicalDevices".as_ptr()) },
            vulkan_wsi::eclipse_vk_enumerate_physical_devices as *const () as *mut c_void,
            "the client's device list goes through the chosen-GPU filter"
        );
        assert_eq!(
            unsafe { (dl.sym)(libc_handle, c"vkEnumeratePhysicalDeviceGroups".as_ptr()) },
            vulkan_wsi::eclipse_vk_enumerate_physical_device_groups as *const () as *mut c_void,
            "the client's device groups go through the chosen-GPU filter"
        );
        assert!(
            unsafe { (dl.sym)(libc_handle, c"vkEnumeratePhysicalDeviceGroupsKHR".as_ptr()) }
                .is_null(),
            "no Vulkan loader exports the KHR alias, so dlsym finds none"
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

    #[test]
    fn opengl_es_hides_only_the_vulkan_loader_and_vulkan_symbols() {
        let gles = Graphics::Gles(GlesReason::Configured);
        for loader in [
            c"libvulkan.so",
            c"/system/lib64/libvulkan.so",
            c"libvulkan.so.1",
        ] {
            assert!(hides_library(loader, gles), "{loader:?}");
            assert!(
                hides_library(loader, Graphics::Gles(GlesReason::NoUsableVulkan)),
                "{loader:?}"
            );
            assert!(!hides_library(loader, Graphics::Vulkan), "{loader:?}");
        }
        for other in [c"libvulkan_foo.so", c"libEGL.so", c"libGLESv3.so"] {
            assert!(!hides_library(other, gles), "{other:?}");
        }
        for vulkan in [c"vkCreateInstance", c"vkGetInstanceProcAddr"] {
            assert!(hides_symbol(vulkan, gles), "{vulkan:?}");
            assert!(!hides_symbol(vulkan, Graphics::Vulkan), "{vulkan:?}");
        }
        for other in [
            c"eglSwapBuffers",
            c"glDrawArrays",
            c"AAudio_createStreamBuilder",
        ] {
            assert!(!hides_symbol(other, gles), "{other:?}");
        }
    }

    #[test]
    fn opengl_es_closes_every_native_route_to_vulkan() {
        if std::env::var_os(HIDDEN_VULKAN_CHILD).is_none() {
            let output = crate::bounded_child::output(
                std::process::Command::new(
                    std::env::current_exe().expect("the test harness executable must have a path"),
                )
                .args([
                    "--exact",
                    "loader::dlfcn::tests::opengl_es_closes_every_native_route_to_vulkan",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(HIDDEN_VULKAN_CHILD, "1"),
                Duration::from_secs(60),
            );
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && report.contains("1 passed"),
                "status={:?}, stdout={report}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        crate::gpu::give_client(Graphics::Gles(GlesReason::Configured));
        let dl = dl_through_provider();
        let error = unsafe {
            std::mem::transmute::<*mut c_void, DlerrorFn>(provider_addr(&dl, c"dlerror"))
        };

        assert!(unsafe { (dl.open)(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW) }.is_null());
        let refusal = unsafe { error() };
        assert!(
            !refusal.is_null(),
            "a refused dlopen leaves a dlerror message"
        );
        let refusal = unsafe { CStr::from_ptr(refusal) }
            .to_string_lossy()
            .into_owned();
        assert!(refusal.contains("libvulkan.so.1"), "{refusal}");
        assert!(
            unsafe { error() }.is_null(),
            "dlerror reports the refusal once"
        );

        let libc_handle = unsafe { (dl.open)(c"libc.so.6".as_ptr(), libc::RTLD_NOW) };
        assert!(!libc_handle.is_null());
        assert!(unsafe { (dl.sym)(libc_handle, c"vkCreateInstance".as_ptr()) }.is_null());
        assert_eq!(
            unsafe { (dl.sym)(libc_handle, c"strlen".as_ptr()) },
            unsafe { libc::dlsym(libc_handle, c"strlen".as_ptr()) },
            "other symbols still come from the host"
        );

        let create_instance = unsafe {
            std::mem::transmute::<*mut c_void, vk::PFN_vkCreateInstance>(provider_addr(
                &dl,
                c"vkCreateInstance",
            ))
        };
        let mut instance = vk::Instance::null();
        let info = vk::InstanceCreateInfo::default();
        assert_eq!(
            unsafe { create_instance(&info, std::ptr::null(), &mut instance) },
            vk::Result::ERROR_INCOMPATIBLE_DRIVER
        );
        let get_instance_proc_addr = unsafe {
            std::mem::transmute::<*mut c_void, vk::PFN_vkGetInstanceProcAddr>(provider_addr(
                &dl,
                c"vkGetInstanceProcAddr",
            ))
        };
        assert!(
            unsafe { get_instance_proc_addr(instance, c"vkCreateInstance".as_ptr()) }.is_none()
        );
        let create_surface = unsafe {
            std::mem::transmute::<*mut c_void, vk::PFN_vkCreateAndroidSurfaceKHR>(provider_addr(
                &dl,
                c"vkCreateAndroidSurfaceKHR",
            ))
        };
        let mut surface = vk::SurfaceKHR::null();
        assert_eq!(
            unsafe {
                create_surface(
                    instance,
                    &vk::AndroidSurfaceCreateInfoKHR::default(),
                    std::ptr::null(),
                    &mut surface,
                )
            },
            vk::Result::ERROR_INITIALIZATION_FAILED
        );

        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        assert!(
            !maps.contains("libvulkan"),
            "hiding Vulkan loaded the Vulkan loader"
        );
    }
}
