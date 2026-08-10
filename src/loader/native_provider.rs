use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_long, c_void};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use super::init_run::{write_bytes, write_dec, write_hex};
use super::ndk_registry::{
    self, AssetManagerState, AssetState, ConfigurationState, LooperState, NativeWindowState,
};
use super::resolve::{ResolvedSym, SymbolProvider};

pub struct EclipseNativeProvider {
    natives: HashMap<&'static str, u64>,
}

impl EclipseNativeProvider {
    pub fn empty() -> Self {
        Self {
            natives: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &'static str, addr: u64) -> &mut Self {
        self.natives.insert(name, addr);
        self
    }

    pub fn len(&self) -> usize {
        self.natives.len()
    }

    pub fn is_empty(&self) -> bool {
        self.natives.is_empty()
    }

    pub fn with_bionic_natives() -> Self {
        let mut p = Self::empty();

        p.register(
            "__android_log_write",
            eclipse_android_log_write as *const () as u64,
        );
        p.register(
            "__android_log_buf_write",
            eclipse_android_log_buf_write as *const () as u64,
        );
        p.register(
            "android_set_abort_message",
            eclipse_android_set_abort_message as *const () as u64,
        );

        p.register(
            "__android_log_print",
            __android_log_print as *const () as u64,
        );
        p.register(
            "__android_log_assert",
            __android_log_assert as *const () as u64,
        );

        p.register(
            "__android_log_vprint",
            __android_log_vprint as *const () as u64,
        );

        p.register("__strlen_chk", eclipse_strlen_chk as *const () as u64);
        p.register("__strchr_chk", eclipse_strchr_chk as *const () as u64);
        p.register("__strncpy_chk2", eclipse_strncpy_chk2 as *const () as u64);
        p.register("__write_chk", eclipse_write_chk as *const () as u64);
        p.register("__fwrite_chk", eclipse_fwrite_chk as *const () as u64);
        p.register("__sendto_chk", eclipse_sendto_chk as *const () as u64);
        p.register("__FD_SET_chk", eclipse_fd_set_chk as *const () as u64);
        p.register("__FD_CLR_chk", eclipse_fd_clr_chk as *const () as u64);
        p.register("__FD_ISSET_chk", eclipse_fd_isset_chk as *const () as u64);
        p.register("__umask_chk", eclipse_umask_chk as *const () as u64);

        p.register("__errno", eclipse_errno as *const () as u64);
        p.register("__assert2", eclipse_assert2 as *const () as u64);
        p.register(
            "__gnu_strerror_r",
            eclipse_gnu_strerror_r as *const () as u64,
        );
        p.register(
            "__system_property_get",
            eclipse_system_property_get as *const () as u64,
        );

        p.register("__stack_chk_guard", eclipse_stack_chk_guard_addr());
        p.register("__sF", eclipse_sf_addr());

        p.register("clearerr", eclipse_clearerr as *const () as u64);
        p.register("fclose", eclipse_fclose as *const () as u64);
        p.register("feof", eclipse_feof as *const () as u64);
        p.register("ferror", eclipse_ferror as *const () as u64);
        p.register("fflush", eclipse_fflush as *const () as u64);
        p.register("fgets", eclipse_fgets as *const () as u64);
        p.register("fileno", eclipse_fileno as *const () as u64);
        p.register("fputc", eclipse_fputc as *const () as u64);
        p.register("fputs", eclipse_fputs as *const () as u64);
        p.register("fputwc", eclipse_fputwc as *const () as u64);
        p.register("fread", eclipse_fread as *const () as u64);
        p.register("__fread_chk", eclipse_fread_chk as *const () as u64);
        p.register("fseek", eclipse_fseek as *const () as u64);
        p.register("fseeko", eclipse_fseeko as *const () as u64);
        p.register("ftell", eclipse_ftell as *const () as u64);
        p.register("ftello", eclipse_ftello as *const () as u64);
        p.register("fwrite", eclipse_fwrite as *const () as u64);
        p.register("getc", eclipse_getc as *const () as u64);
        p.register("getwc", eclipse_getwc as *const () as u64);
        p.register("setvbuf", eclipse_setvbuf as *const () as u64);
        p.register("ungetc", eclipse_ungetc as *const () as u64);
        p.register("ungetwc", eclipse_ungetwc as *const () as u64);

        p.register("fprintf", eclipse_fprintf as *const () as u64);
        p.register("fscanf", eclipse_fscanf as *const () as u64);
        p.register("vfprintf", eclipse_vfprintf as *const () as u64);

        p.register("sigaction", eclipse_sigaction as *const () as u64);
        p.register("sigemptyset", eclipse_sigemptyset as *const () as u64);
        p.register("sigaddset", eclipse_sigaddset as *const () as u64);
        p.register("sigfillset", eclipse_sigfillset as *const () as u64);
        p.register("sigprocmask", eclipse_sigprocmask as *const () as u64);
        p.register(
            "pthread_sigmask",
            eclipse_pthread_sigmask as *const () as u64,
        );

        p.register("sigaltstack", eclipse_sigaltstack as *const () as u64);

        p.register(
            "dl_iterate_phdr",
            super::module_registry::eclipse_dl_iterate_phdr as *const () as u64,
        );
        p.register(
            "dladdr",
            super::module_registry::eclipse_dladdr as *const () as u64,
        );

        p.register("getaddrinfo", eclipse_getaddrinfo as *const () as u64);
        p.register("freeaddrinfo", eclipse_freeaddrinfo as *const () as u64);
        p.register("gai_strerror", eclipse_gai_strerror as *const () as u64);
        p.register("getnameinfo", eclipse_getnameinfo as *const () as u64);

        p.register(
            "AAssetManager_fromJava",
            eclipse_aassetmanager_fromjava as *const () as u64,
        );
        p.register(
            "AAssetManager_open",
            eclipse_aassetmanager_open as *const () as u64,
        );
        p.register("AAsset_close", eclipse_aasset_close as *const () as u64);
        p.register(
            "AAsset_getBuffer",
            eclipse_aasset_getbuffer as *const () as u64,
        );
        p.register(
            "AAsset_getLength",
            eclipse_aasset_getlength as *const () as u64,
        );
        p.register(
            "AAsset_openFileDescriptor",
            eclipse_aasset_openfiledescriptor as *const () as u64,
        );

        p.register(
            "AConfiguration_new",
            eclipse_aconfiguration_new as *const () as u64,
        );
        p.register(
            "AConfiguration_delete",
            eclipse_aconfiguration_delete as *const () as u64,
        );
        p.register(
            "AConfiguration_fromAssetManager",
            eclipse_aconfiguration_fromassetmanager as *const () as u64,
        );
        p.register(
            "AConfiguration_getCountry",
            eclipse_aconfiguration_getcountry as *const () as u64,
        );
        p.register(
            "AConfiguration_getLanguage",
            eclipse_aconfiguration_getlanguage as *const () as u64,
        );
        p.register(
            "AConfiguration_getNavHidden",
            eclipse_aconfiguration_getnavhidden as *const () as u64,
        );
        p.register(
            "AConfiguration_getScreenHeightDp",
            eclipse_aconfiguration_getscreenheightdp as *const () as u64,
        );
        p.register(
            "AConfiguration_getScreenSize",
            eclipse_aconfiguration_getscreensize as *const () as u64,
        );
        p.register(
            "AConfiguration_getScreenWidthDp",
            eclipse_aconfiguration_getscreenwidthdp as *const () as u64,
        );

        p.register(
            "ALooper_prepare",
            eclipse_alooper_prepare as *const () as u64,
        );
        p.register(
            "ALooper_forThread",
            eclipse_alooper_forthread as *const () as u64,
        );
        p.register(
            "ALooper_acquire",
            eclipse_alooper_acquire as *const () as u64,
        );
        p.register(
            "ALooper_release",
            eclipse_alooper_release as *const () as u64,
        );
        p.register(
            "ALooper_pollOnce",
            eclipse_alooper_pollonce as *const () as u64,
        );
        p.register("ALooper_addFd", eclipse_alooper_addfd as *const () as u64);
        p.register(
            "ALooper_removeFd",
            eclipse_alooper_removefd as *const () as u64,
        );

        p.register("eglGetDisplay", eclipse_egl_get_display as *const () as u64);

        p.register(
            "vkGetInstanceProcAddr",
            super::vulkan_wsi::eclipse_vk_get_instance_proc_addr as *const () as u64,
        );
        p.register(
            "vkCreateInstance",
            super::vulkan_wsi::eclipse_vk_create_instance as *const () as u64,
        );
        p.register(
            "vkCreateAndroidSurfaceKHR",
            super::vulkan_wsi::eclipse_vk_create_android_surface_khr as *const () as u64,
        );

        p.register(
            "dlsym",
            super::vulkan_wsi::eclipse_dlsym as *const () as u64,
        );

        p.register(
            "ANativeWindow_fromSurface",
            eclipse_anativewindow_fromsurface as *const () as u64,
        );
        p.register(
            "ANativeWindow_getWidth",
            eclipse_anativewindow_getwidth as *const () as u64,
        );
        p.register(
            "ANativeWindow_getHeight",
            eclipse_anativewindow_getheight as *const () as u64,
        );
        p.register(
            "ANativeWindow_getFormat",
            eclipse_anativewindow_getformat as *const () as u64,
        );
        p.register(
            "ANativeWindow_acquire",
            eclipse_anativewindow_acquire as *const () as u64,
        );
        p.register(
            "ANativeWindow_release",
            eclipse_anativewindow_release as *const () as u64,
        );

        p.register(
            "AMediaCodec_configure",
            eclipse_amediacodec_configure as *const () as u64,
        );
        p.register(
            "AMediaCodec_createDecoderByType",
            eclipse_amediacodec_createdecoderbytype as *const () as u64,
        );
        p.register(
            "AMediaCodec_createEncoderByType",
            eclipse_amediacodec_createencoderbytype as *const () as u64,
        );
        p.register(
            "AMediaCodec_delete",
            eclipse_amediacodec_delete as *const () as u64,
        );
        p.register(
            "AMediaCodec_dequeueInputBuffer",
            eclipse_amediacodec_dequeueinputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_dequeueOutputBuffer",
            eclipse_amediacodec_dequeueoutputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_flush",
            eclipse_amediacodec_flush as *const () as u64,
        );
        p.register(
            "AMediaCodec_getInputBuffer",
            eclipse_amediacodec_getinputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_getOutputBuffer",
            eclipse_amediacodec_getoutputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_getOutputFormat",
            eclipse_amediacodec_getoutputformat as *const () as u64,
        );
        p.register(
            "AMediaCodec_queueInputBuffer",
            eclipse_amediacodec_queueinputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_releaseOutputBuffer",
            eclipse_amediacodec_releaseoutputbuffer as *const () as u64,
        );
        p.register(
            "AMediaCodec_start",
            eclipse_amediacodec_start as *const () as u64,
        );
        p.register(
            "AMediaCodec_stop",
            eclipse_amediacodec_stop as *const () as u64,
        );

        p.register(
            "AMediaFormat_delete",
            eclipse_amediaformat_delete as *const () as u64,
        );
        p.register(
            "AMediaFormat_getBuffer",
            eclipse_amediaformat_getbuffer as *const () as u64,
        );
        p.register(
            "AMediaFormat_getInt32",
            eclipse_amediaformat_getint32 as *const () as u64,
        );
        p.register(
            "AMediaFormat_new",
            eclipse_amediaformat_new as *const () as u64,
        );
        p.register(
            "AMediaFormat_setBuffer",
            eclipse_amediaformat_setbuffer as *const () as u64,
        );
        p.register(
            "AMediaFormat_setFloat",
            eclipse_amediaformat_setfloat as *const () as u64,
        );
        p.register(
            "AMediaFormat_setInt32",
            eclipse_amediaformat_setint32 as *const () as u64,
        );
        p.register(
            "AMediaFormat_setString",
            eclipse_amediaformat_setstring as *const () as u64,
        );
        p.register(
            "AMediaFormat_toString",
            eclipse_amediaformat_tostring as *const () as u64,
        );

        p.register("AMEDIAFORMAT_KEY_BIT_RATE", amediaformat_key_addr(0));
        p.register("AMEDIAFORMAT_KEY_CHANNEL_COUNT", amediaformat_key_addr(1));
        p.register("AMEDIAFORMAT_KEY_COLOR_FORMAT", amediaformat_key_addr(2));
        p.register("AMEDIAFORMAT_KEY_FRAME_RATE", amediaformat_key_addr(3));
        p.register("AMEDIAFORMAT_KEY_HEIGHT", amediaformat_key_addr(4));
        p.register(
            "AMEDIAFORMAT_KEY_I_FRAME_INTERVAL",
            amediaformat_key_addr(5),
        );
        p.register("AMEDIAFORMAT_KEY_MIME", amediaformat_key_addr(6));
        p.register("AMEDIAFORMAT_KEY_SAMPLE_RATE", amediaformat_key_addr(7));
        p.register("AMEDIAFORMAT_KEY_STRIDE", amediaformat_key_addr(8));
        p.register("AMEDIAFORMAT_KEY_WIDTH", amediaformat_key_addr(9));

        p.register(
            "slCreateEngine",
            super::opensl::eclipse_sl_create_engine as *const () as u64,
        );

        p.register("SL_IID_ANDROIDCONFIGURATION", sl_iid_addr(0));
        p.register("SL_IID_ANDROIDSIMPLEBUFFERQUEUE", sl_iid_addr(1));
        p.register("SL_IID_BUFFERQUEUE", sl_iid_addr(2));
        p.register("SL_IID_ENGINE", sl_iid_addr(3));
        p.register("SL_IID_PLAY", sl_iid_addr(4));
        p.register("SL_IID_RECORD", sl_iid_addr(5));
        p.register("SL_IID_VOLUME", sl_iid_addr(6));

        super::bionic_pthread::register_natives(|name, addr| {
            p.register(name, addr);
        });

        super::bionic_sysconf::register_natives(|name, addr| {
            p.register(name, addr);
        });

        p
    }
}

impl SymbolProvider for EclipseNativeProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        self.natives
            .get(name)
            .map(|&addr| ResolvedSym { addr, weak: false })
    }
}

#[must_use]
pub fn anativewindow_from_surface_via_provider() -> Option<*mut c_void> {
    let provider = EclipseNativeProvider::with_bionic_natives();
    let addr = provider.resolve("ANativeWindow_fromSurface")?.addr;

    let func: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute::<u64, _>(addr) };

    Some(unsafe { func(std::ptr::null_mut(), std::ptr::null_mut()) })
}

const ANDROID_LOG_VERBOSE: c_int = 2;
const ANDROID_LOG_DEBUG: c_int = 3;
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_WARN: c_int = 5;
const ANDROID_LOG_ERROR: c_int = 6;
const ANDROID_LOG_FATAL: c_int = 7;

fn emit_log(priority: c_int, tag: &str, msg: &str) {
    #[cfg(test)]
    if tests::capture_emit(priority, tag, msg) {
        return;
    }
    match priority {
        ANDROID_LOG_VERBOSE => tracing::trace!(target: "liblog", tag, "{msg}"),
        ANDROID_LOG_DEBUG => tracing::debug!(target: "liblog", tag, "{msg}"),
        ANDROID_LOG_INFO => tracing::info!(target: "liblog", tag, "{msg}"),
        ANDROID_LOG_WARN => tracing::warn!(target: "liblog", tag, "{msg}"),
        ANDROID_LOG_ERROR | ANDROID_LOG_FATAL => tracing::error!(target: "liblog", tag, "{msg}"),
        _ => tracing::info!(target: "liblog", tag, priority, "{msg}"),
    }
}

unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }

    let s = unsafe { std::ffi::CStr::from_ptr(p) };
    Some(s.to_string_lossy().into_owned())
}

unsafe extern "C" fn eclipse_android_log_write(
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    let tag = unsafe { cstr_opt(tag) }.unwrap_or_default();
    let text = unsafe { cstr_opt(text) }.unwrap_or_default();
    let n = text.len();
    emit_log(prio, &tag, &text);

    c_int::try_from(n).unwrap_or(c_int::MAX).max(1)
}

unsafe extern "C" fn eclipse_android_log_buf_write(
    _buf_id: c_int,
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    unsafe { eclipse_android_log_write(prio, tag, text) }
}

unsafe extern "C" fn eclipse_android_set_abort_message(msg: *const c_char) {
    let msg = unsafe { cstr_opt(msg) }.unwrap_or_default();
    emit_log(ANDROID_LOG_ERROR, "abort", &msg);
}

extern "C" {

    fn __android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;

    fn __android_log_assert(cond: *const c_char, tag: *const c_char, fmt: *const c_char, ...);

    fn __android_log_vprint(
        prio: c_int,
        tag: *const c_char,
        fmt: *const c_char,
        ap: *mut c_void,
    ) -> c_int;
}

#[no_mangle]
pub unsafe extern "C" fn eclipse_liblog_emit(prio: c_int, tag: *const c_char, msg: *const c_char) {
    let tag = unsafe { cstr_opt(tag) }.unwrap_or_default();

    let msg = unsafe { cstr_opt(msg) }.unwrap_or_default();
    emit_log(prio, &tag, &msg);
}

unsafe extern "C" fn eclipse_strlen_chk(s: *const c_char, s_len: usize) -> usize {
    let len = unsafe { libc::strlen(s) };
    if len >= s_len {
        std::process::abort();
    }
    len
}

unsafe extern "C" fn eclipse_strchr_chk(s: *const c_char, c: c_int, s_len: usize) -> *mut c_char {
    let len = unsafe { libc::strlen(s) };
    if len >= s_len {
        std::process::abort();
    }

    unsafe { libc::strchr(s, c) }
}

unsafe extern "C" fn eclipse_strncpy_chk2(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
    _src_len: usize,
) -> *mut c_char {
    if n > dst_len {
        std::process::abort();
    }

    unsafe { libc::strncpy(dst, src, n) }
}

unsafe extern "C" fn eclipse_write_chk(
    fd: c_int,
    buf: *const c_void,
    count: usize,
    buf_size: usize,
) -> isize {
    if count > buf_size {
        std::process::abort();
    }

    unsafe { libc::write(fd, buf, count) }
}

unsafe extern "C" fn eclipse_fwrite_chk(
    buf: *const c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
    buf_size: usize,
) -> usize {
    match size.checked_mul(count) {
        Some(t) if t <= buf_size => {}
        _ => std::process::abort(),
    }

    unsafe { libc::fwrite(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_sendto_chk(
    fd: c_int,
    buf: *const c_void,
    len: usize,
    buf_size: usize,
    flags: c_int,
    dst: *const libc::sockaddr,
    dst_len: libc::socklen_t,
) -> isize {
    if len > buf_size {
        std::process::abort();
    }

    unsafe { libc::sendto(fd, buf, len, flags, dst, dst_len) }
}

unsafe extern "C" fn eclipse_umask_chk(mode: libc::mode_t) -> libc::mode_t {
    if mode & !0o777 != 0 {
        std::process::abort();
    }

    unsafe { libc::umask(mode) }
}

fn fd_in_range(fd: c_int, set_size: usize) -> bool {
    fd >= 0 && (fd as usize) < set_size.saturating_mul(8)
}

unsafe extern "C" fn eclipse_fd_set_chk(fd: c_int, set: *mut libc::fd_set, set_size: usize) {
    if !fd_in_range(fd, set_size) {
        std::process::abort();
    }

    unsafe { libc::FD_SET(fd, set) }
}

unsafe extern "C" fn eclipse_fd_clr_chk(fd: c_int, set: *mut libc::fd_set, set_size: usize) {
    if !fd_in_range(fd, set_size) {
        std::process::abort();
    }

    unsafe { libc::FD_CLR(fd, set) }
}

unsafe extern "C" fn eclipse_fd_isset_chk(
    fd: c_int,
    set: *mut libc::fd_set,
    set_size: usize,
) -> c_int {
    if !fd_in_range(fd, set_size) {
        std::process::abort();
    }

    c_int::from(unsafe { libc::FD_ISSET(fd, set) })
}

extern "C" fn eclipse_errno() -> *mut c_int {
    unsafe { libc::__errno_location() }
}

unsafe extern "C" fn eclipse_assert2(
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    failed_expr: *const c_char,
) -> ! {
    let file = unsafe { cstr_opt(file) }.unwrap_or_default();
    let func = unsafe { cstr_opt(func) }.unwrap_or_default();
    let expr = unsafe { cstr_opt(failed_expr) }.unwrap_or_default();
    emit_log(
        ANDROID_LOG_FATAL,
        "assert",
        &format!("{file}:{line}: {func}: assertion \"{expr}\" failed"),
    );
    std::process::abort();
}

unsafe extern "C" fn eclipse_gnu_strerror_r(
    errnum: c_int,
    buf: *mut c_char,
    buflen: usize,
) -> *mut c_char {
    unsafe { gnu_strerror_r(errnum, buf, buflen) }
}

extern "C" {

    #[link_name = "strerror_r"]
    fn gnu_strerror_r(errnum: c_int, buf: *mut c_char, buflen: usize) -> *mut c_char;
}

unsafe extern "C" fn eclipse_system_property_get(
    _name: *const c_char,
    value: *mut c_char,
) -> c_int {
    if !value.is_null() {
        unsafe { value.write(0) };
    }
    0
}

type BionicSigsetT = u64;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct BionicSigaction {
    sa_flags: c_int,

    handler: usize,

    sa_mask: BionicSigsetT,

    sa_restorer: usize,
}

const SA_RESTORER_FLAG: c_int = 0x0400_0000;

fn glibc_sigset_from_bionic(set: BionicSigsetT) -> libc::sigset_t {
    unsafe {
        let mut g: libc::sigset_t = std::mem::zeroed();
        std::ptr::copy_nonoverlapping(
            (&raw const set).cast::<u8>(),
            (&raw mut g).cast::<u8>(),
            std::mem::size_of::<BionicSigsetT>(),
        );
        g
    }
}

fn bionic_sigset_from_glibc(set: &libc::sigset_t) -> BionicSigsetT {
    let mut b: BionicSigsetT = 0;

    unsafe {
        std::ptr::copy_nonoverlapping(
            (set as *const libc::sigset_t).cast::<u8>(),
            (&raw mut b).cast::<u8>(),
            std::mem::size_of::<BionicSigsetT>(),
        );
    }
    b
}

fn bionic_action_from_glibc(g: &libc::sigaction) -> BionicSigaction {
    BionicSigaction {
        sa_flags: g.sa_flags & !SA_RESTORER_FLAG,
        handler: g.sa_sigaction,
        sa_mask: bionic_sigset_from_glibc(&g.sa_mask),
        sa_restorer: 0,
    }
}

fn einval() -> c_int {
    unsafe { *libc::__errno_location() = libc::EINVAL };
    -1
}

unsafe extern "C" fn eclipse_sigaction(
    signum: c_int,
    act: *const BionicSigaction,
    oldact: *mut BionicSigaction,
) -> c_int {
    let tapped = TAPPED_SIGNAL.load(Ordering::Acquire);
    if tapped != 0 && signum == tapped {
        return unsafe { tap_chain_register(act, oldact) };
    }
    let g_act = if act.is_null() {
        None
    } else {
        let b = unsafe { *act };

        let mut g: libc::sigaction = unsafe { std::mem::zeroed() };
        g.sa_sigaction = b.handler;
        g.sa_mask = glibc_sigset_from_bionic(b.sa_mask);
        g.sa_flags = b.sa_flags & !SA_RESTORER_FLAG;
        Some(g)
    };

    let mut g_old: libc::sigaction = unsafe { std::mem::zeroed() };

    let ret = unsafe {
        libc::sigaction(
            signum,
            g_act
                .as_ref()
                .map_or(std::ptr::null(), |g| g as *const libc::sigaction),
            if oldact.is_null() {
                std::ptr::null_mut()
            } else {
                &mut g_old
            },
        )
    };
    if ret == 0 && !oldact.is_null() {
        unsafe {
            *oldact = bionic_action_from_glibc(&g_old);
        }
    }
    ret
}

unsafe extern "C" fn eclipse_sigemptyset(set: *mut BionicSigsetT) -> c_int {
    if set.is_null() {
        return einval();
    }

    unsafe { *set = 0 };
    0
}

unsafe extern "C" fn eclipse_sigfillset(set: *mut BionicSigsetT) -> c_int {
    if set.is_null() {
        return einval();
    }

    unsafe { *set = !0 };
    0
}

unsafe extern "C" fn eclipse_sigaddset(set: *mut BionicSigsetT, signum: c_int) -> c_int {
    let bit = signum.wrapping_sub(1);
    if set.is_null() || !(0..64).contains(&bit) {
        return einval();
    }

    unsafe { *set |= 1u64 << bit };
    0
}

unsafe extern "C" fn eclipse_sigprocmask(
    how: c_int,
    set: *const BionicSigsetT,
    oldset: *mut BionicSigsetT,
) -> c_int {
    let g_set = if set.is_null() {
        None
    } else {
        Some(glibc_sigset_from_bionic(unsafe { *set }))
    };

    let mut g_old: libc::sigset_t = unsafe { std::mem::zeroed() };

    let ret = unsafe {
        libc::sigprocmask(
            how,
            g_set
                .as_ref()
                .map_or(std::ptr::null(), |g| g as *const libc::sigset_t),
            if oldset.is_null() {
                std::ptr::null_mut()
            } else {
                &mut g_old
            },
        )
    };
    if ret == 0 && !oldset.is_null() {
        unsafe { *oldset = bionic_sigset_from_glibc(&g_old) };
    }
    ret
}

unsafe extern "C" fn eclipse_pthread_sigmask(
    how: c_int,
    set: *const BionicSigsetT,
    oldset: *mut BionicSigsetT,
) -> c_int {
    let g_set = if set.is_null() {
        None
    } else {
        Some(glibc_sigset_from_bionic(unsafe { *set }))
    };

    let mut g_old: libc::sigset_t = unsafe { std::mem::zeroed() };

    let ret = unsafe {
        libc::pthread_sigmask(
            how,
            g_set
                .as_ref()
                .map_or(std::ptr::null(), |g| g as *const libc::sigset_t),
            if oldset.is_null() {
                std::ptr::null_mut()
            } else {
                &mut g_old
            },
        )
    };
    if ret == 0 && !oldset.is_null() {
        unsafe { *oldset = bionic_sigset_from_glibc(&g_old) };
    }
    ret
}

static TAPPED_SIGNAL: AtomicI32 = AtomicI32::new(0);

static TAP_CHAIN: AtomicPtr<BionicSigaction> = AtomicPtr::new(std::ptr::null_mut());

const TAP_CHAIN_POOL_LEN: usize = 8;

struct TapChainPool([UnsafeCell<BionicSigaction>; TAP_CHAIN_POOL_LEN]);

impl TapChainPool {
    const fn new() -> Self {
        Self(
            [const {
                UnsafeCell::new(BionicSigaction {
                    sa_flags: 0,
                    handler: 0,
                    sa_mask: 0,
                    sa_restorer: 0,
                })
            }; TAP_CHAIN_POOL_LEN],
        )
    }
}

unsafe impl Sync for TapChainPool {}

static TAP_CHAIN_POOL: TapChainPool = TapChainPool::new();

static TAP_CHAIN_POOL_NEXT: AtomicUsize = AtomicUsize::new(0);

fn tap_chain_publish(
    pool: &TapChainPool,
    next: &AtomicUsize,
    slot: &AtomicPtr<BionicSigaction>,
    b: BionicSigaction,
) -> bool {
    let idx = next.fetch_add(1, Ordering::Relaxed);
    let Some(cell) = pool.0.get(idx) else {
        const MSG: &[u8] =
            b"eclipse early-fault tap: chain pool exhausted; keeping previous chain occupant\n";

        unsafe { libc::write(2, MSG.as_ptr().cast::<c_void>(), MSG.len()) };
        return false;
    };

    unsafe { cell.get().write(b) };
    slot.store(cell.get(), Ordering::Release);
    true
}

fn tap_chain_store(b: BionicSigaction) -> bool {
    tap_chain_publish(&TAP_CHAIN_POOL, &TAP_CHAIN_POOL_NEXT, &TAP_CHAIN, b)
}

static TAP_HANDLER_TID: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapEntryClaim {
    Latched,

    Unlatched,

    SameThreadReentry,
}

fn tap_entry_claim(latch: &AtomicI64, tid: i64) -> TapEntryClaim {
    match latch.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => TapEntryClaim::Latched,
        Err(owner) if owner == tid => TapEntryClaim::SameThreadReentry,
        Err(_) => TapEntryClaim::Unlatched,
    }
}

static ENGINE_RANGE_BASE: AtomicU64 = AtomicU64::new(0);

static ENGINE_RANGE_SPAN: AtomicU64 = AtomicU64::new(0);

const SEGV_MAPERR: c_int = 1;
const SEGV_ACCERR: c_int = 2;

unsafe fn tap_chain_register(act: *const BionicSigaction, oldact: *mut BionicSigaction) -> c_int {
    if !oldact.is_null() {
        let prev = TAP_CHAIN.load(Ordering::Acquire);
        let out = if prev.is_null() {
            BionicSigaction {
                sa_flags: 0,
                handler: 0,
                sa_mask: 0,
                sa_restorer: 0,
            }
        } else {
            unsafe { *prev }
        };

        unsafe { *oldact = out };
    }
    if !act.is_null() {
        let mut b = unsafe { *act };

        b.sa_flags &= !SA_RESTORER_FLAG;
        b.sa_restorer = 0;

        let _ = tap_chain_store(b);
    }
    0
}

fn tap_restore_default(signo: c_int) {
    unsafe {
        let dfl: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signo, &dfl, std::ptr::null_mut());
    }
}

fn tap_read_u64(addr: u64) -> Option<u64> {
    let mut val: u64 = 0;
    let local = libc::iovec {
        iov_base: (&raw mut val).cast::<c_void>(),
        iov_len: 8,
    };
    let remote = libc::iovec {
        iov_base: addr as *mut c_void,
        iov_len: 8,
    };

    let ret = unsafe {
        libc::syscall(
            libc::SYS_process_vm_readv,
            libc::getpid() as libc::c_long,
            &raw const local,
            1usize,
            &raw const remote,
            1usize,
            0usize,
        )
    };
    (ret == 8).then_some(val)
}

fn tap_stack_walk(rip: u64, rsp: u64, rbp: u64, out: &mut [u64; 32]) -> usize {
    const MAX_FRAME_STEP: u64 = 1 << 20;
    out[0] = rip;
    let mut count = 1usize;
    let mut fp = rbp;
    while count < out.len() {
        if fp == 0 || !fp.is_multiple_of(8) || fp <= rsp {
            break;
        }
        let Some(ret) = tap_read_u64(fp.wrapping_add(8)) else {
            break;
        };
        let Some(next) = tap_read_u64(fp) else {
            break;
        };
        if next <= fp || next.wrapping_sub(fp) >= MAX_FRAME_STEP {
            break;
        }
        out[count] = ret;
        count += 1;
        fp = next;
    }
    count
}

fn tap_write_addr(buf: &mut [u8], n: &mut usize, val: u64, base: u64, span: u64) {
    write_bytes(buf, n, b"0x");
    write_hex(buf, n, val);
    if base != 0 && (base..base.wrapping_add(span)).contains(&val) {
        write_bytes(buf, n, b" (libroblox+0x");
        write_hex(buf, n, val - base);
        write_bytes(buf, n, b")");
    }
}

unsafe extern "C" fn early_fault_tap_handler(
    signo: c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut c_void,
) {
    let saved_errno = unsafe { *libc::__errno_location() };

    let my_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i64;

    let claim = tap_entry_claim(&TAP_HANDLER_TID, my_tid);
    if claim == TapEntryClaim::SameThreadReentry {
        tap_restore_default(signo);

        unsafe { *libc::__errno_location() = saved_errno };
        return;
    }

    let (si_signo, si_code, si_addr) = if info.is_null() {
        (signo, 0, 0u64)
    } else {
        unsafe { ((*info).si_signo, (*info).si_code, (*info).si_addr() as u64) }
    };
    let (rip, rsp, rbp, err) = if ctx.is_null() {
        (0u64, 0u64, 0u64, 0u64)
    } else {
        let uc = unsafe { &*ctx.cast::<libc::ucontext_t>() };
        (
            uc.uc_mcontext.gregs[libc::REG_RIP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_RSP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_RBP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_ERR as usize] as u64,
        )
    };

    let base = ENGINE_RANGE_BASE.load(Ordering::Relaxed);
    let span = ENGINE_RANGE_SPAN.load(Ordering::Relaxed);
    if base == 0 || (base..base.wrapping_add(span)).contains(&rip) {
        let mut frames = [0u64; 32];
        let nframes = tap_stack_walk(rip, rsp, rbp, &mut frames);

        let mut buf = [0u8; 2048];
        let mut n = 0usize;
        write_bytes(&mut buf, &mut n, b"\n*** ECLIPSE EARLY-FAULT TAP: signal ");
        write_dec(&mut buf, &mut n, si_signo as u64);
        write_bytes(&mut buf, &mut n, b" code ");
        if si_code < 0 {
            write_bytes(&mut buf, &mut n, b"-");
            write_dec(&mut buf, &mut n, u64::from(si_code.unsigned_abs()));
        } else {
            write_dec(&mut buf, &mut n, si_code as u64);
        }
        write_bytes(&mut buf, &mut n, b" (");
        let label: &[u8] = if si_code == SEGV_MAPERR {
            b"MAPERR"
        } else if si_code == SEGV_ACCERR {
            b"ACCERR"
        } else if si_code == libc::SI_KERNEL {
            b"SI_KERNEL"
        } else {
            b"?"
        };
        write_bytes(&mut buf, &mut n, label);
        write_bytes(&mut buf, &mut n, b") addr=0x");
        write_hex(&mut buf, &mut n, si_addr);
        write_bytes(&mut buf, &mut n, b" ***\nrip=");
        tap_write_addr(&mut buf, &mut n, rip, base, span);
        write_bytes(&mut buf, &mut n, b" rsp=0x");
        write_hex(&mut buf, &mut n, rsp);
        write_bytes(&mut buf, &mut n, b" rbp=0x");
        write_hex(&mut buf, &mut n, rbp);

        write_bytes(&mut buf, &mut n, b" err=0x");
        write_hex(&mut buf, &mut n, err);
        write_bytes(&mut buf, &mut n, b"\n");
        for (k, &frame) in frames.iter().take(nframes).enumerate() {
            write_bytes(&mut buf, &mut n, b"frame[");
            write_dec(&mut buf, &mut n, k as u64);
            write_bytes(&mut buf, &mut n, b"]=");
            tap_write_addr(&mut buf, &mut n, frame, base, span);
            write_bytes(&mut buf, &mut n, b"\n");
        }

        unsafe { libc::write(2, buf.as_ptr().cast::<c_void>(), n) };
    }

    unsafe { *libc::__errno_location() = saved_errno };

    let p = TAP_CHAIN.load(Ordering::Acquire);
    if p.is_null() {
        tap_restore_default(signo);
    } else {
        let chain = unsafe { *p };
        if chain.handler == libc::SIG_DFL {
            tap_restore_default(signo);
        } else if chain.handler == libc::SIG_IGN {
        } else if chain.sa_flags & libc::SA_SIGINFO != 0 {
            let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                unsafe { std::mem::transmute::<usize, _>(chain.handler) };
            f(signo, info, ctx);
        } else {
            let f: extern "C" fn(c_int) = unsafe { std::mem::transmute::<usize, _>(chain.handler) };
            f(signo);
        }
    }

    if claim == TapEntryClaim::Latched {
        TAP_HANDLER_TID.store(0, Ordering::SeqCst);
    }
}

pub(super) fn install_early_fault_tap(signum: c_int) -> Result<(), String> {
    if TAPPED_SIGNAL.load(Ordering::Acquire) != 0 {
        return Ok(());
    }

    let mut queried: libc::sigaction = unsafe { std::mem::zeroed() };

    if unsafe { libc::sigaction(signum, std::ptr::null(), &mut queried) } != 0 {
        return Err(format!(
            "raw sigaction({signum}) query failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let seed = bionic_action_from_glibc(&queried);
    if !tap_chain_store(seed) {
        return Err("early-fault tap: chain pool exhausted before seeding".to_string());
    }

    let (ret, old) = unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = early_fault_tap_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        let mut old: libc::sigaction = std::mem::zeroed();
        (libc::sigaction(signum, &sa, &mut old), old)
    };
    if ret != 0 {
        return Err(format!(
            "raw sigaction({signum}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    let displaced = bionic_action_from_glibc(&old);
    if displaced != seed && !tap_chain_store(displaced) {
        return Err("early-fault tap: chain pool exhausted on re-seed".to_string());
    }

    TAPPED_SIGNAL.store(signum, Ordering::Release);
    Ok(())
}

pub(super) fn publish_engine_text_range(base: u64, span: u64) {
    ENGINE_RANGE_BASE.store(base, Ordering::Relaxed);
    ENGINE_RANGE_SPAN.store(span, Ordering::Relaxed);
}

extern "C" {

    pub fn eclipse_sigaltstack(ss: *const libc::stack_t, old_ss: *mut libc::stack_t) -> c_int;
}

#[derive(Clone, Debug)]
pub struct AltstackRegistration {
    pub tid: i64,

    pub ss_sp: u64,

    pub ss_size: usize,

    pub ss_flags: c_int,

    pub caller: u64,

    pub caller_module: Option<String>,
}

const ALTSTACK_LOG_CAP: usize = 64;

static ALTSTACK_LOG: std::sync::Mutex<Vec<AltstackRegistration>> =
    std::sync::Mutex::new(Vec::new());
static ALTSTACK_TOTAL: AtomicU64 = AtomicU64::new(0);

#[must_use]
pub fn recent_altstack_registrations() -> Vec<AltstackRegistration> {
    ALTSTACK_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[must_use]
pub fn altstack_registration_total() -> u64 {
    ALTSTACK_TOTAL.load(Ordering::Relaxed)
}

fn describe_code_address(addr: u64) -> Option<String> {
    if let Some(s) = super::module_registry::describe_address(addr) {
        return Some(s);
    }

    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };

    if unsafe { libc::dladdr(addr as *const c_void, &mut info) } != 0 && !info.dli_fname.is_null() {
        let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
        let short = name.rsplit('/').next().unwrap_or(&name);
        return Some(format!(
            "{short}+{:#x}",
            addr.wrapping_sub(info.dli_fbase as u64)
        ));
    }
    None
}

#[no_mangle]
pub unsafe extern "C" fn eclipse_sigaltstack_record(
    ss: *const libc::stack_t,
    old_ss: *mut libc::stack_t,
    caller: *const c_void,
) -> c_int {
    let ret = unsafe { libc::sigaltstack(ss, old_ss) };
    if ss.is_null() {
        return ret;
    }
    if ret != 0 {
        let e = std::io::Error::last_os_error();
        tracing::warn!(
            target: "eclipse.sigaltstack",
            caller = format_args!("{:#x}", caller as u64),
            error = %e,
            "sigaltstack registration REJECTED by the kernel"
        );
        return ret;
    }

    let stack = unsafe { *ss };

    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i64;
    let caller = caller as u64;
    let caller_module = describe_code_address(caller);
    tracing::info!(
        target: "eclipse.sigaltstack",
        tid,
        ss_sp = format_args!("{:#x}", stack.ss_sp as u64),
        ss_size = stack.ss_size,
        ss_flags = stack.ss_flags,
        disable = stack.ss_flags & libc::SS_DISABLE != 0,
        caller = format_args!("{caller:#x}"),
        caller_module = caller_module.as_deref().unwrap_or("?"),
        "altstack registered (core-1223806 attribution)"
    );
    let rec = AltstackRegistration {
        tid,
        ss_sp: stack.ss_sp as u64,
        ss_size: stack.ss_size,
        ss_flags: stack.ss_flags,
        caller,
        caller_module,
    };
    let mut log = ALTSTACK_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if log.len() == ALTSTACK_LOG_CAP {
        log.remove(0);
    }
    log.push(rec);
    ALTSTACK_TOTAL.fetch_add(1, Ordering::Relaxed);
    ret
}

pub const ALTSTACK_CHAIN_BUDGET: usize = 80 * 1024;

pub const ALTSTACK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct GuardedAltstack {
    pub ss_sp: u64,

    pub ss_size: usize,

    pub guard_base: u64,

    pub mapping_len: usize,
}

pub fn install_guarded_altstack() -> Result<GuardedAltstack, String> {
    let page = super::map::host_page_size() as usize;
    let mapping_len = page + ALTSTACK_SIZE;

    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapping_len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!(
            "mmap({mapping_len}) for the alternate signal stack failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let guard_base = base as u64;
    let ss_sp = guard_base + page as u64;

    if unsafe {
        libc::mprotect(
            ss_sp as *mut c_void,
            ALTSTACK_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    } != 0
    {
        let e = std::io::Error::last_os_error();

        unsafe { libc::munmap(base, mapping_len) };
        return Err(format!(
            "mprotect(RW) of the alternate signal stack failed: {e}"
        ));
    }
    let ss = libc::stack_t {
        ss_sp: ss_sp as *mut c_void,
        ss_flags: 0,
        ss_size: ALTSTACK_SIZE,
    };

    if unsafe { libc::sigaltstack(&ss, std::ptr::null_mut()) } != 0 {
        let e = std::io::Error::last_os_error();

        unsafe { libc::munmap(base, mapping_len) };
        return Err(format!("sigaltstack(install) failed: {e}"));
    }
    Ok(GuardedAltstack {
        ss_sp,
        ss_size: ALTSTACK_SIZE,
        guard_base,
        mapping_len,
    })
}

static ECLIPSE_STACK_CHK_GUARD: AtomicUsize = AtomicUsize::new(0);

fn eclipse_stack_chk_guard_addr() -> u64 {
    let _ = ECLIPSE_STACK_CHK_GUARD.compare_exchange(
        0,
        0xff0a_55c3_0000_0000usize,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    std::ptr::addr_of!(ECLIPSE_STACK_CHK_GUARD) as u64
}

const SF_FILE_STRIDE: usize = 152;

const SF_ENTRY_COUNT: usize = 3;

const SF_BACKING_LEN: usize = SF_FILE_STRIDE * SF_ENTRY_COUNT;

#[repr(C, align(8))]
struct SfBacking(UnsafeCell<[u8; SF_BACKING_LEN]>);

unsafe impl Sync for SfBacking {}

static ECLIPSE_SF: SfBacking = SfBacking(UnsafeCell::new([0u8; SF_BACKING_LEN]));

extern "C" {

    static stdin: *mut libc::FILE;
    static stdout: *mut libc::FILE;
    static stderr: *mut libc::FILE;
}

fn eclipse_sf_addr() -> u64 {
    ECLIPSE_SF.0.get() as u64
}

#[no_mangle]
pub extern "C" fn eclipse_sf_translate_stream(stream: *mut libc::FILE) -> *mut libc::FILE {
    let base = ECLIPSE_SF.0.get() as usize;
    let p = stream as usize;
    if p == base {
        unsafe { stdin }
    } else if p == base + SF_FILE_STRIDE {
        unsafe { stdout }
    } else if p == base + 2 * SF_FILE_STRIDE {
        unsafe { stderr }
    } else {
        stream
    }
}

unsafe extern "C" fn eclipse_clearerr(stream: *mut libc::FILE) {
    unsafe { libc::clearerr(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fclose(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fclose(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_feof(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::feof(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_ferror(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::ferror(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fflush(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fflush(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fgets(
    buf: *mut c_char,
    n: c_int,
    stream: *mut libc::FILE,
) -> *mut c_char {
    unsafe { libc::fgets(buf, n, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fileno(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fileno(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fputc(c: c_int, stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fputc(c, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fputs(s: *const c_char, stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fputs(s, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fread(
    buf: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
) -> usize {
    unsafe { libc::fread(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fseek(
    stream: *mut libc::FILE,
    offset: c_long,
    whence: c_int,
) -> c_int {
    unsafe { libc::fseek(eclipse_sf_translate_stream(stream), offset, whence) }
}

unsafe extern "C" fn eclipse_fseeko(
    stream: *mut libc::FILE,
    offset: libc::off_t,
    whence: c_int,
) -> c_int {
    unsafe { libc::fseeko(eclipse_sf_translate_stream(stream), offset, whence) }
}

unsafe extern "C" fn eclipse_ftell(stream: *mut libc::FILE) -> c_long {
    unsafe { libc::ftell(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_ftello(stream: *mut libc::FILE) -> libc::off_t {
    unsafe { libc::ftello(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fwrite(
    buf: *const c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
) -> usize {
    unsafe { libc::fwrite(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_getc(stream: *mut libc::FILE) -> c_int {
    unsafe { libc::fgetc(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_setvbuf(
    stream: *mut libc::FILE,
    buffer: *mut c_char,
    mode: c_int,
    size: usize,
) -> c_int {
    unsafe { libc::setvbuf(eclipse_sf_translate_stream(stream), buffer, mode, size) }
}

unsafe extern "C" fn eclipse_ungetc(c: c_int, stream: *mut libc::FILE) -> c_int {
    unsafe { libc::ungetc(c, eclipse_sf_translate_stream(stream)) }
}

#[allow(non_camel_case_types)]
type wint_t = std::ffi::c_uint;

extern "C" {

    fn fputwc(wc: libc::wchar_t, stream: *mut libc::FILE) -> wint_t;
    fn getwc(stream: *mut libc::FILE) -> wint_t;
    fn ungetwc(wc: wint_t, stream: *mut libc::FILE) -> wint_t;
}

unsafe extern "C" fn eclipse_fputwc(wc: libc::wchar_t, stream: *mut libc::FILE) -> wint_t {
    unsafe { fputwc(wc, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_getwc(stream: *mut libc::FILE) -> wint_t {
    unsafe { getwc(eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_ungetwc(wc: wint_t, stream: *mut libc::FILE) -> wint_t {
    unsafe { ungetwc(wc, eclipse_sf_translate_stream(stream)) }
}

unsafe extern "C" fn eclipse_fread_chk(
    buf: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
    buf_size: usize,
) -> usize {
    match size.checked_mul(count) {
        Some(t) if t <= buf_size => {}
        _ => std::process::abort(),
    }

    unsafe { libc::fread(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

extern "C" {

    fn eclipse_fprintf(stream: *mut libc::FILE, fmt: *const c_char, ...) -> c_int;

    fn eclipse_fscanf(stream: *mut libc::FILE, fmt: *const c_char, ...) -> c_int;

    fn eclipse_vfprintf(stream: *mut libc::FILE, fmt: *const c_char, ap: *mut c_void) -> c_int;
}

#[repr(C)]
struct BionicAddrinfo {
    ai_flags: c_int,

    ai_family: c_int,

    ai_socktype: c_int,

    ai_protocol: c_int,

    ai_addrlen: libc::socklen_t,

    ai_canonname: *mut c_char,

    ai_addr: *mut libc::sockaddr,

    ai_next: *mut BionicAddrinfo,
}

const BIONIC_EAI_ADDRFAMILY: c_int = 1;
const BIONIC_EAI_AGAIN: c_int = 2;
const BIONIC_EAI_BADFLAGS: c_int = 3;
const BIONIC_EAI_FAIL: c_int = 4;
const BIONIC_EAI_FAMILY: c_int = 5;
const BIONIC_EAI_MEMORY: c_int = 6;
const BIONIC_EAI_NODATA: c_int = 7;
const BIONIC_EAI_NONAME: c_int = 8;
const BIONIC_EAI_SERVICE: c_int = 9;
const BIONIC_EAI_SOCKTYPE: c_int = 10;
const BIONIC_EAI_SYSTEM: c_int = 11;
const BIONIC_EAI_OVERFLOW: c_int = 14;

const GLIBC_EAI_ADDRFAMILY: c_int = -9;

const AI_FLAG_PAIRS: &[(c_int, c_int)] = &[
    (0x0001, libc::AI_PASSIVE),
    (0x0002, libc::AI_CANONNAME),
    (0x0004, libc::AI_NUMERICHOST),
    (0x0008, libc::AI_NUMERICSERV),
    (0x0100, libc::AI_ALL),
    (0x0200, libc::AI_V4MAPPED),
    (0x0400, libc::AI_ADDRCONFIG),
    (0x0800, libc::AI_V4MAPPED),
];

const NI_FLAG_PAIRS: &[(c_int, c_int)] = &[
    (0x0001, libc::NI_NOFQDN),
    (0x0002, libc::NI_NUMERICHOST),
    (0x0004, libc::NI_NAMEREQD),
    (0x0008, libc::NI_NUMERICSERV),
    (0x0010, libc::NI_DGRAM),
];

fn translate_flags_by_name(bionic: c_int, pairs: &[(c_int, c_int)]) -> Result<c_int, c_int> {
    let mut rest = bionic;
    let mut glibc = 0;
    for &(b, g) in pairs {
        if rest & b == b {
            glibc |= g;
            rest &= !b;
        }
    }
    if rest != 0 {
        return Err(BIONIC_EAI_BADFLAGS);
    }
    Ok(glibc)
}

fn bionic_eai_from_glibc(rc: c_int) -> c_int {
    match rc {
        0 => 0,
        libc::EAI_BADFLAGS => BIONIC_EAI_BADFLAGS,
        libc::EAI_NONAME => BIONIC_EAI_NONAME,
        libc::EAI_AGAIN => BIONIC_EAI_AGAIN,
        libc::EAI_FAIL => BIONIC_EAI_FAIL,
        libc::EAI_NODATA => BIONIC_EAI_NODATA,
        libc::EAI_FAMILY => BIONIC_EAI_FAMILY,
        libc::EAI_SOCKTYPE => BIONIC_EAI_SOCKTYPE,
        libc::EAI_SERVICE => BIONIC_EAI_SERVICE,
        GLIBC_EAI_ADDRFAMILY => BIONIC_EAI_ADDRFAMILY,
        libc::EAI_MEMORY => BIONIC_EAI_MEMORY,
        libc::EAI_SYSTEM => BIONIC_EAI_SYSTEM,
        libc::EAI_OVERFLOW => BIONIC_EAI_OVERFLOW,
        _ => BIONIC_EAI_FAIL,
    }
}

unsafe fn bionic_node_from_glibc(g: &libc::addrinfo, bionic_flags: c_int) -> *mut BionicAddrinfo {
    let addr_len = if g.ai_addr.is_null() {
        0
    } else {
        g.ai_addrlen as usize
    };
    let canon_len = if g.ai_canonname.is_null() {
        0
    } else {
        unsafe { std::ffi::CStr::from_ptr(g.ai_canonname) }
            .to_bytes_with_nul()
            .len()
    };
    let header = std::mem::size_of::<BionicAddrinfo>();
    let total = header + addr_len + canon_len;

    let block = unsafe { libc::malloc(total) }.cast::<u8>();
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let addr_ptr = if addr_len > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(g.ai_addr.cast::<u8>(), block.add(header), addr_len);
            block.add(header).cast::<libc::sockaddr>()
        }
    } else {
        std::ptr::null_mut()
    };
    let canon_ptr = if canon_len > 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(
                g.ai_canonname.cast::<u8>(),
                block.add(header + addr_len),
                canon_len,
            );
            block.add(header + addr_len).cast::<c_char>()
        }
    } else {
        std::ptr::null_mut()
    };
    let node = block.cast::<BionicAddrinfo>();

    unsafe {
        node.write(BionicAddrinfo {
            ai_flags: bionic_flags,
            ai_family: g.ai_family,
            ai_socktype: g.ai_socktype,
            ai_protocol: g.ai_protocol,
            ai_addrlen: if addr_ptr.is_null() { 0 } else { g.ai_addrlen },
            ai_canonname: canon_ptr,
            ai_addr: addr_ptr,
            ai_next: std::ptr::null_mut(),
        });
    }
    node
}

unsafe extern "C" fn eclipse_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const BionicAddrinfo,
    res: *mut *mut BionicAddrinfo,
) -> c_int {
    let describe = |p: *const c_char| -> String {
        if p.is_null() {
            "<null>".to_owned()
        } else {
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    if res.is_null() {
        unsafe { *libc::__errno_location() = libc::EINVAL };
        return BIONIC_EAI_SYSTEM;
    }
    let (bionic_flags, g_hints) = if hints.is_null() {
        (0, None)
    } else {
        let b = unsafe { &*hints };
        let g_flags = match translate_flags_by_name(b.ai_flags, AI_FLAG_PAIRS) {
            Ok(g) => g,
            Err(eai) => {
                tracing::warn!(
                    target: "eclipse.netdb",
                    node = %describe(node),
                    service = %describe(service),
                    ai_flags = format_args!("0x{:x}", b.ai_flags),
                    "getaddrinfo: undefined bionic AI_* bits -> EAI_BADFLAGS"
                );
                return eai;
            }
        };

        let mut g: libc::addrinfo = unsafe { std::mem::zeroed() };
        g.ai_flags = g_flags;
        g.ai_family = b.ai_family;
        g.ai_socktype = b.ai_socktype;
        g.ai_protocol = b.ai_protocol;
        (b.ai_flags, Some(g))
    };

    let mut g_res: *mut libc::addrinfo = std::ptr::null_mut();

    let rc = unsafe {
        libc::getaddrinfo(
            node,
            service,
            g_hints
                .as_ref()
                .map_or(std::ptr::null(), |g| g as *const libc::addrinfo),
            &mut g_res,
        )
    };
    if rc != 0 {
        let eai = bionic_eai_from_glibc(rc);

        let saved_errno = unsafe { *libc::__errno_location() };
        tracing::info!(
            target: "eclipse.netdb",
            node = %describe(node),
            service = %describe(service),
            bionic_ai_flags = format_args!("0x{bionic_flags:x}"),
            glibc_rc = rc,
            bionic_eai = eai,
            "getaddrinfo: host resolution failed (translated to bionic-positive EAI)"
        );

        unsafe { *libc::__errno_location() = saved_errno };
        return eai;
    }

    let mut head: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut tail: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut count = 0u32;
    let mut cursor = g_res;
    while !cursor.is_null() {
        let g = unsafe { &*cursor };

        let bionic_node = unsafe { bionic_node_from_glibc(g, bionic_flags) };
        if bionic_node.is_null() {
            unsafe { eclipse_freeaddrinfo(head) };

            unsafe { libc::freeaddrinfo(g_res) };
            return BIONIC_EAI_MEMORY;
        }
        if head.is_null() {
            head = bionic_node;
        } else {
            unsafe { (*tail).ai_next = bionic_node };
        }
        tail = bionic_node;
        count += 1;
        cursor = g.ai_next;
    }

    unsafe { libc::freeaddrinfo(g_res) };

    unsafe { *res = head };
    tracing::debug!(
        target: "eclipse.netdb",
        node = %describe(node),
        service = %describe(service),
        bionic_ai_flags = format_args!("0x{bionic_flags:x}"),
        nodes = count,
        "getaddrinfo: resolved via host glibc into bionic-shaped nodes"
    );
    0
}

unsafe extern "C" fn eclipse_freeaddrinfo(head: *mut BionicAddrinfo) {
    let mut cursor = head;
    while !cursor.is_null() {
        let next = unsafe { (*cursor).ai_next };

        unsafe { libc::free(cursor.cast()) };
        cursor = next;
    }
}

unsafe extern "C" fn eclipse_gai_strerror(ecode: c_int) -> *const c_char {
    let msg: &'static [u8] = match ecode {
        0 => b"no error\0",
        BIONIC_EAI_ADDRFAMILY => b"address family for hostname not supported\0",
        BIONIC_EAI_AGAIN => b"temporary failure in name resolution\0",
        BIONIC_EAI_BADFLAGS => b"invalid value for ai_flags\0",
        BIONIC_EAI_FAIL => b"non-recoverable failure in name resolution\0",
        BIONIC_EAI_FAMILY => b"ai_family not supported\0",
        BIONIC_EAI_MEMORY => b"memory allocation failure\0",
        BIONIC_EAI_NODATA => b"no address associated with hostname\0",
        BIONIC_EAI_NONAME => b"hostname nor servname provided, or not known\0",
        BIONIC_EAI_SERVICE => b"servname not supported for ai_socktype\0",
        BIONIC_EAI_SOCKTYPE => b"ai_socktype not supported\0",
        BIONIC_EAI_SYSTEM => b"system error returned in errno\0",
        12 => b"invalid value for hints\0",
        13 => b"resolved protocol is unknown\0",
        BIONIC_EAI_OVERFLOW => b"argument buffer overflow\0",
        _ => b"unknown error\0",
    };
    msg.as_ptr().cast()
}

unsafe extern "C" fn eclipse_getnameinfo(
    sa: *const libc::sockaddr,
    salen: libc::socklen_t,
    host: *mut c_char,
    hostlen: libc::socklen_t,
    serv: *mut c_char,
    servlen: libc::socklen_t,
    flags: c_int,
) -> c_int {
    let g_flags = match translate_flags_by_name(flags, NI_FLAG_PAIRS) {
        Ok(g) => g,
        Err(eai) => {
            tracing::warn!(
                target: "eclipse.netdb",
                ni_flags = format_args!("0x{flags:x}"),
                "getnameinfo: undefined bionic NI_* bits -> EAI_BADFLAGS"
            );
            return eai;
        }
    };

    let rc = unsafe { libc::getnameinfo(sa, salen, host, hostlen, serv, servlen, g_flags) };
    bionic_eai_from_glibc(rc)
}

fn handle_to_ptr<T>(h: ndk_registry::NdkHandle) -> *mut T {
    h as usize as *mut T
}

fn ptr_to_handle<T>(p: *const T) -> ndk_registry::NdkHandle {
    p as usize as ndk_registry::NdkHandle
}

const ACONFIGURATION_DENSITY_BASELINE: i32 = 160;

const ACONFIGURATION_DENSITY_XHIGH: i32 = 320;

const ACONFIGURATION_ORIENTATION_PORT: i32 = 1;

const ACONFIGURATION_SCREENSIZE_NORMAL: i32 = 2;

const ACONFIGURATION_NAVHIDDEN_YES: i32 = 2;

const DEFAULT_DISPLAY_WIDTH_PX: i32 = 1080;

const DEFAULT_DISPLAY_HEIGHT_PX: i32 = 1920;

const WINDOW_FORMAT_RGBA_8888: i32 = 1;

fn default_configuration() -> ConfigurationState {
    let to_dp = |px: i32| px * ACONFIGURATION_DENSITY_BASELINE / ACONFIGURATION_DENSITY_XHIGH;
    ConfigurationState {
        density: ACONFIGURATION_DENSITY_XHIGH,
        screen_width_dp: to_dp(DEFAULT_DISPLAY_WIDTH_PX),
        screen_height_dp: to_dp(DEFAULT_DISPLAY_HEIGHT_PX),
        screen_size: ACONFIGURATION_SCREENSIZE_NORMAL,
        orientation: ACONFIGURATION_ORIENTATION_PORT,
        nav_hidden: ACONFIGURATION_NAVHIDDEN_YES,
        language: *b"en",
        country: *b"US",
    }
}

#[derive(Clone, Copy)]
struct ConfigurationCacheEntry {
    handle: ndk_registry::NdkHandle,
    epoch: u64,
    state: ConfigurationState,
}

static CONFIGURATION_EPOCH: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONFIGURATION_CACHE: std::cell::Cell<Option<ConfigurationCacheEntry>> =
        const { std::cell::Cell::new(None) };
}

fn advance_configuration_epoch() {
    CONFIGURATION_EPOCH.fetch_add(1, Ordering::AcqRel);
}

fn configuration_snapshot(config: *const c_void) -> Option<ConfigurationState> {
    let handle = ptr_to_handle(config);
    let epoch = CONFIGURATION_EPOCH.load(Ordering::Acquire);
    CONFIGURATION_CACHE.with(|cache| {
        if let Some(entry) = cache
            .get()
            .filter(|entry| entry.handle == handle && entry.epoch == epoch)
        {
            return Some(entry.state);
        }
        let state = ndk_registry::configurations()
            .with(handle, |state| *state)
            .ok()?;
        cache.set(Some(ConfigurationCacheEntry {
            handle,
            epoch,
            state,
        }));
        Some(state)
    })
}

fn default_native_window() -> NativeWindowState {
    let (width, height) = ndk_registry::engine_window_geometry()
        .unwrap_or((DEFAULT_DISPLAY_WIDTH_PX, DEFAULT_DISPLAY_HEIGHT_PX));
    NativeWindowState {
        width,
        height,
        format: WINDOW_FORMAT_RGBA_8888,
    }
}

const ASSET_ENTRY_PREFIX: &str = "assets/";

unsafe extern "C" fn eclipse_aassetmanager_fromjava(
    _env: *mut c_void,
    _asset_manager: *mut c_void,
) -> *mut c_void {
    match ndk_registry::apk_path() {
        Some(path) => {
            let state = AssetManagerState {
                apk_path: path.clone(),
            };
            match ndk_registry::asset_managers().insert(state) {
                Ok(h) => handle_to_ptr(h),
                Err(_) => std::ptr::null_mut(),
            }
        }
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn eclipse_aassetmanager_open(
    mgr: *mut c_void,
    filename: *const c_char,
    _mode: c_int,
) -> *mut c_void {
    let Some(name) = (unsafe { cstr_opt(filename) }) else {
        return std::ptr::null_mut();
    };

    let apk_path =
        match ndk_registry::asset_managers().with(ptr_to_handle(mgr), |m| m.apk_path.clone()) {
            Ok(p) => p,
            Err(_) => return std::ptr::null_mut(),
        };

    let entry = format!("{ASSET_ENTRY_PREFIX}{name}");
    let bytes = match crate::apk::Apk::open(&apk_path).and_then(|mut a| a.read_entry(&entry)) {
        Ok(b) => b,
        Err(_) => return std::ptr::null_mut(),
    };
    let state = AssetState {
        bytes: bytes.into_boxed_slice(),
        cursor: 0,
    };
    match ndk_registry::assets().insert(state) {
        Ok(h) => handle_to_ptr(h),
        Err(_) => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn eclipse_aasset_close(asset: *mut c_void) {
    let _ = ndk_registry::assets().remove(ptr_to_handle(asset));
}

unsafe extern "C" fn eclipse_aasset_getbuffer(asset: *mut c_void) -> *const c_void {
    match ndk_registry::assets().with(ptr_to_handle(asset), |a| a.bytes.as_ptr() as *const c_void) {
        Ok(p) => p,
        Err(_) => std::ptr::null(),
    }
}

unsafe extern "C" fn eclipse_aasset_getlength(asset: *mut c_void) -> libc::off_t {
    match ndk_registry::assets().with(ptr_to_handle(asset), |a| a.bytes.len()) {
        Ok(n) => libc::off_t::try_from(n).unwrap_or(libc::off_t::MAX),
        Err(_) => 0,
    }
}

unsafe extern "C" fn eclipse_aasset_openfiledescriptor(
    asset: *mut c_void,
    out_start: *mut libc::off_t,
    out_length: *mut libc::off_t,
) -> c_int {
    let bytes = match ndk_registry::assets().with(ptr_to_handle(asset), |a| a.bytes.clone()) {
        Ok(b) => b,
        Err(_) => return -1,
    };
    let len = bytes.len();

    unsafe {
        let fd = libc::memfd_create(c"eclipse-asset".as_ptr(), 0);
        if fd < 0 {
            return -1;
        }
        if len > 0 {
            if libc::ftruncate(fd, len as libc::off_t) < 0 {
                libc::close(fd);
                return -1;
            }
            let mut off = 0usize;
            while off < len {
                let n = libc::write(fd, bytes.as_ptr().add(off) as *const c_void, len - off);
                if n <= 0 {
                    libc::close(fd);
                    return -1;
                }
                off += n as usize;
            }
            libc::lseek(fd, 0, libc::SEEK_SET);
        }
        if !out_start.is_null() {
            *out_start = 0;
        }
        if !out_length.is_null() {
            *out_length = len as libc::off_t;
        }
        fd
    }
}

extern "C" fn eclipse_aconfiguration_new() -> *mut c_void {
    match ndk_registry::configurations().insert(default_configuration()) {
        Ok(h) => {
            advance_configuration_epoch();
            handle_to_ptr(h)
        }
        Err(_) => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn eclipse_aconfiguration_delete(config: *mut c_void) {
    if ndk_registry::configurations()
        .remove(ptr_to_handle(config))
        .is_ok()
    {
        advance_configuration_epoch();
    }
}

unsafe extern "C" fn eclipse_aconfiguration_fromassetmanager(out: *mut c_void, _am: *mut c_void) {
    if ndk_registry::configurations()
        .with(ptr_to_handle(out), |c| *c = default_configuration())
        .is_ok()
    {
        advance_configuration_epoch();
    }
}

unsafe extern "C" fn eclipse_aconfiguration_getcountry(
    config: *mut c_void,
    out_country: *mut c_char,
) {
    if out_country.is_null() {
        return;
    }
    if let Some(country) = configuration_snapshot(config).map(|c| c.country) {
        unsafe {
            out_country.write(country[0] as c_char);
            out_country.add(1).write(country[1] as c_char);
        }
    }
}

unsafe extern "C" fn eclipse_aconfiguration_getlanguage(
    config: *mut c_void,
    out_language: *mut c_char,
) {
    if out_language.is_null() {
        return;
    }
    if let Some(language) = configuration_snapshot(config).map(|c| c.language) {
        unsafe {
            out_language.write(language[0] as c_char);
            out_language.add(1).write(language[1] as c_char);
        }
    }
}

unsafe extern "C" fn eclipse_aconfiguration_getnavhidden(config: *mut c_void) -> i32 {
    configuration_snapshot(config).map_or(0, |c| c.nav_hidden)
}

unsafe extern "C" fn eclipse_aconfiguration_getscreenheightdp(config: *mut c_void) -> i32 {
    configuration_snapshot(config).map_or(0, |c| c.screen_height_dp)
}

unsafe extern "C" fn eclipse_aconfiguration_getscreensize(config: *mut c_void) -> i32 {
    configuration_snapshot(config).map_or(0, |c| c.screen_size)
}

unsafe extern "C" fn eclipse_aconfiguration_getscreenwidthdp(config: *mut c_void) -> i32 {
    configuration_snapshot(config).map_or(0, |c| c.screen_width_dp)
}

use crate::loader::looper::{
    PollResult, ALOOPER_EVENT_INPUT, ALOOPER_POLL_ERROR, ALOOPER_POLL_TIMEOUT, ALOOPER_POLL_WAKE,
};

thread_local! {

    static THREAD_LOOPER: std::cell::Cell<Option<ndk_registry::NdkHandle>> =
        const { std::cell::Cell::new(None) };
}

extern "C" fn eclipse_alooper_prepare(_opts: c_int) -> *mut c_void {
    THREAD_LOOPER.with(|tl| {
        if let Some(h) = tl.get() {
            return handle_to_ptr(h);
        }

        let Some(looper) = crate::loader::looper::Looper::new() else {
            return std::ptr::null_mut();
        };

        ndk_registry::register_looper_waker(looper.waker());
        match ndk_registry::loopers().insert(LooperState { looper }) {
            Ok(h) => {
                tl.set(Some(h));
                handle_to_ptr(h)
            }
            Err(_) => std::ptr::null_mut(),
        }
    })
}

extern "C" fn eclipse_alooper_forthread() -> *mut c_void {
    THREAD_LOOPER.with(|tl| match tl.get() {
        Some(h) => handle_to_ptr(h),
        None => std::ptr::null_mut(),
    })
}

unsafe extern "C" fn eclipse_alooper_acquire(_looper: *mut c_void) {}

unsafe extern "C" fn eclipse_alooper_release(_looper: *mut c_void) {}

unsafe extern "C" fn eclipse_alooper_pollonce(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    let Some(handle) = THREAD_LOOPER.with(std::cell::Cell::get) else {
        return ALOOPER_POLL_ERROR;
    };

    let snapshot = match ndk_registry::loopers().with(handle, |l| l.looper.snapshot()) {
        Ok(s) => s,
        Err(_) => return ALOOPER_POLL_ERROR,
    };
    let result = snapshot.poll_once(timeout_millis);

    let (ret, fd, events) = match result {
        PollResult::Fd { ident, fd, events } => (ident, fd, events),
        PollResult::Wake => (ALOOPER_POLL_WAKE, 0, 0),
        PollResult::Timeout => (ALOOPER_POLL_TIMEOUT, 0, 0),
        PollResult::Error => (ALOOPER_POLL_ERROR, 0, 0),
    };
    if !out_fd.is_null() {
        unsafe { out_fd.write(fd) };
    }
    if !out_events.is_null() {
        unsafe { out_events.write(events) };
    }
    if !out_data.is_null() {
        unsafe { out_data.write(std::ptr::null_mut()) };
    }
    ret
}

unsafe extern "C" fn eclipse_alooper_addfd(
    looper: *mut c_void,
    fd: c_int,
    ident: c_int,
    events: c_int,
    callback: *mut c_void,
    _data: *mut c_void,
) -> c_int {
    if !callback.is_null() || ident < 0 {
        return -1;
    }
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| {
        l.looper.add_fd(fd, ident, events)
    }) {
        Ok(()) => 1,
        Err(_) => -1,
    }
}

unsafe extern "C" fn eclipse_alooper_removefd(looper: *mut c_void, fd: c_int) -> c_int {
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| {
        let _ = l.looper.remove_fd(fd);
    }) {
        Ok(()) => 1,
        Err(_) => -1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostInputKind {
    Pointer,

    MouseButton,

    Scroll,

    Touch,

    Key,
}

pub fn classify_winit_event(event: &winit::event::WindowEvent) -> Option<HostInputKind> {
    use winit::event::WindowEvent as W;
    match event {
        W::CursorMoved { .. } | W::CursorEntered { .. } | W::CursorLeft { .. } => {
            Some(HostInputKind::Pointer)
        }
        W::MouseInput { .. } => Some(HostInputKind::MouseButton),
        W::MouseWheel { .. } => Some(HostInputKind::Scroll),
        W::Touch(_) => Some(HostInputKind::Touch),
        W::KeyboardInput { .. } => Some(HostInputKind::Key),
        _ => None,
    }
}

pub fn host_input_should_wake(kind: Option<HostInputKind>) -> bool {
    kind.is_some()
}

pub fn feed_winit_input_to_loopers(event: &winit::event::WindowEvent) -> usize {
    if host_input_should_wake(classify_winit_event(event)) {
        ndk_registry::wake_all_loopers()
    } else {
        0
    }
}

pub fn run_input_test() -> Result<String, String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::mpsc;
    use std::time::Duration;

    let mut fds = [0i32; 2];

    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err("pipe2 failed (fd exhaustion?)".into());
    }

    let read: OwnedFd = unsafe { OwnedFd::from_raw_fd(fds[0]) };

    let mut write: std::fs::File = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let read_fd = read.as_raw_fd();

    const ENGINE_INPUT_IDENT: c_int = 11;
    let (registered_tx, registered_rx) = mpsc::channel::<bool>();
    let (fd_result_tx, fd_result_rx) = mpsc::channel::<(c_int, c_int, c_int)>();
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (wake_result_tx, wake_result_rx) = mpsc::channel::<c_int>();

    let worker = std::thread::spawn(move || {
        let looper = eclipse_alooper_prepare(0);
        if looper.is_null() {
            let _ = registered_tx.send(false);
            return;
        }

        let added = unsafe {
            eclipse_alooper_addfd(
                looper,
                read_fd,
                ENGINE_INPUT_IDENT,
                ALOOPER_EVENT_INPUT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let _ = registered_tx.send(added == 1);
        let mut out_fd: c_int = -1;
        let mut out_events: c_int = -1;

        let rc = unsafe {
            eclipse_alooper_pollonce(5000, &mut out_fd, &mut out_events, std::ptr::null_mut())
        };
        let _ = fd_result_tx.send((rc, out_fd, out_events));

        let _ = unsafe { eclipse_alooper_removefd(looper, read_fd) };
        let _ = parked_tx.send(());

        let wrc = unsafe {
            eclipse_alooper_pollonce(
                -1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let _ = wake_result_tx.send(wrc);
    });

    match registered_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => return Err("ALooper_prepare/addFd failed in the worker".into()),
        Err(_) => return Err("worker did not register its fd (timeout)".into()),
    }

    write
        .write_all(b"DOWN")
        .map_err(|e| format!("write to engine input source: {e}"))?;
    let (rc, out_fd, out_events) = fd_result_rx
        .recv_timeout(Duration::from_secs(6))
        .map_err(|_| "pollOnce did not wake on the fd (timeout)".to_string())?;
    if rc != ENGINE_INPUT_IDENT {
        return Err(format!(
            "pollOnce returned {rc}, expected the registered ident {ENGINE_INPUT_IDENT}"
        ));
    }
    if out_fd != read_fd {
        return Err(format!(
            "pollOnce out_fd {out_fd} != the firing fd {read_fd}"
        ));
    }
    if out_events & ALOOPER_EVENT_INPUT == 0 {
        return Err(format!("pollOnce out_events {out_events} missing POLLIN"));
    }

    parked_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "worker did not re-park for the wake stage (timeout)".to_string())?;

    std::thread::sleep(Duration::from_millis(50));
    let woken = ndk_registry::wake_all_loopers();
    if woken == 0 {
        return Err("wake_all_loopers woke 0 loopers (no looper registered its waker?)".into());
    }
    let wrc = wake_result_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "parked pollOnce did not return after the wake (timeout)".to_string())?;
    if wrc != ALOOPER_POLL_WAKE {
        return Err(format!(
            "post-wake pollOnce returned {wrc}, expected ALOOPER_POLL_WAKE ({ALOOPER_POLL_WAKE})"
        ));
    }

    worker
        .join()
        .map_err(|_| "worker thread panicked".to_string())?;
    Ok(format!(
        "input path OK: registered fd → pollOnce returned ident {ENGINE_INPUT_IDENT} (fd {read_fd}, POLLIN); \
         host-input wake → parked pollOnce returned ALOOPER_POLL_WAKE; {woken} looper(s) woken"
    ))
}

fn resolve_egl_display_target(display_id: usize, wsi: Option<usize>) -> usize {
    if display_id == 0 {
        wsi.unwrap_or(0)
    } else {
        display_id
    }
}

fn host_egl_get_display() -> Option<usize> {
    static HOST_EGL_GET_DISPLAY: OnceLock<Option<usize>> = OnceLock::new();
    *HOST_EGL_GET_DISPLAY.get_or_init(|| {
        let handle =
            unsafe { libc::dlopen(c"libEGL.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return None;
        }
        let sym = unsafe { libc::dlsym(handle, c"eglGetDisplay".as_ptr()) };
        if sym.is_null() {
            None
        } else {
            Some(sym as usize)
        }
    })
}

unsafe extern "C" fn eclipse_egl_get_display(display_id: *mut c_void) -> *mut c_void {
    let Some(host) = host_egl_get_display() else {
        return std::ptr::null_mut();
    };
    let target = resolve_egl_display_target(display_id as usize, ndk_registry::wsi_display());

    let host_fn: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(host) };
    unsafe { host_fn(target as *mut c_void) }
}

unsafe extern "C" fn eclipse_anativewindow_fromsurface(
    _env: *mut c_void,
    _surface: *mut c_void,
) -> *mut c_void {
    if let Some(p) = ndk_registry::current_wsi_window() {
        ndk_registry::set_engine_claimed_surface(true);
        return p as *mut c_void;
    }

    ndk_registry::register_fallback_native_window(default_native_window()) as *mut c_void
}

unsafe extern "C" fn eclipse_anativewindow_getwidth(window: *mut c_void) -> i32 {
    if let Some((w, _)) = ndk_registry::wsi_window_geometry(window as usize) {
        return w;
    }
    ndk_registry::fallback_native_window_state(window as usize).map_or(-1, |w| w.width)
}

unsafe extern "C" fn eclipse_anativewindow_getheight(window: *mut c_void) -> i32 {
    if let Some((_, h)) = ndk_registry::wsi_window_geometry(window as usize) {
        return h;
    }
    ndk_registry::fallback_native_window_state(window as usize).map_or(-1, |w| w.height)
}

unsafe extern "C" fn eclipse_anativewindow_getformat(window: *mut c_void) -> i32 {
    if ndk_registry::wsi_window_geometry(window as usize).is_some() {
        return WINDOW_FORMAT_RGBA_8888;
    }
    ndk_registry::fallback_native_window_state(window as usize).map_or(-1, |w| w.format)
}

unsafe extern "C" fn eclipse_anativewindow_acquire(_window: *mut c_void) {}

unsafe extern "C" fn eclipse_anativewindow_release(_window: *mut c_void) {}

type MediaStatus = c_int;

const AMEDIA_ERROR_BASE: MediaStatus = -10000;

const AMEDIA_ERROR_UNSUPPORTED: MediaStatus = AMEDIA_ERROR_BASE - 9;

unsafe extern "C" fn eclipse_amediacodec_configure(
    _codec: *mut c_void,
    _format: *const c_void,
    _surface: *mut c_void,
    _crypto: *mut c_void,
    _flags: u32,
) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_createdecoderbytype(
    _mime_type: *const c_char,
) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediacodec_createencoderbytype(
    _mime_type: *const c_char,
) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediacodec_delete(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_dequeueinputbuffer(
    _codec: *mut c_void,
    _timeout_us: i64,
) -> isize {
    AMEDIA_ERROR_UNSUPPORTED as isize
}

unsafe extern "C" fn eclipse_amediacodec_dequeueoutputbuffer(
    _codec: *mut c_void,
    _info: *mut c_void,
    _timeout_us: i64,
) -> isize {
    AMEDIA_ERROR_UNSUPPORTED as isize
}

unsafe extern "C" fn eclipse_amediacodec_flush(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_getinputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediacodec_getoutputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediacodec_getoutputformat(_codec: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediacodec_queueinputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _offset: libc::off_t,
    _size: usize,
    _time: u64,
    _flags: u32,
) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_releaseoutputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _render: bool,
) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_start(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediacodec_stop(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

extern "C" fn eclipse_amediaformat_new() -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn eclipse_amediaformat_delete(_format: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

unsafe extern "C" fn eclipse_amediaformat_getint32(
    _format: *mut c_void,
    _name: *const c_char,
    _out: *mut i32,
) -> bool {
    false
}

unsafe extern "C" fn eclipse_amediaformat_getbuffer(
    _format: *mut c_void,
    _name: *const c_char,
    _data: *mut *mut c_void,
    _size: *mut usize,
) -> bool {
    false
}

unsafe extern "C" fn eclipse_amediaformat_setint32(
    _format: *mut c_void,
    _name: *const c_char,
    _value: i32,
) {
}

unsafe extern "C" fn eclipse_amediaformat_setfloat(
    _format: *mut c_void,
    _name: *const c_char,
    _value: f32,
) {
}

unsafe extern "C" fn eclipse_amediaformat_setstring(
    _format: *mut c_void,
    _name: *const c_char,
    _value: *const c_char,
) {
}

unsafe extern "C" fn eclipse_amediaformat_setbuffer(
    _format: *mut c_void,
    _name: *const c_char,
    _data: *const c_void,
    _size: usize,
) {
}

static EMPTY_CSTR: [u8; 1] = [0];

unsafe extern "C" fn eclipse_amediaformat_tostring(_format: *mut c_void) -> *const c_char {
    EMPTY_CSTR.as_ptr() as *const c_char
}

static AMEDIAFORMAT_KEY_STRINGS: [&[u8]; 10] = [
    b"bitrate\0",
    b"channel-count\0",
    b"color-format\0",
    b"frame-rate\0",
    b"height\0",
    b"i-frame-interval\0",
    b"mime\0",
    b"sample-rate\0",
    b"stride\0",
    b"width\0",
];

struct KeyPtrTable([*const c_char; 10]);

unsafe impl Sync for KeyPtrTable {}

unsafe impl Send for KeyPtrTable {}

static AMEDIAFORMAT_KEY_PTRS: OnceLock<KeyPtrTable> = OnceLock::new();

fn amediaformat_key_addr(idx: usize) -> u64 {
    let t = AMEDIAFORMAT_KEY_PTRS.get_or_init(|| {
        let mut ptrs = [std::ptr::null::<c_char>(); 10];
        for (slot, s) in ptrs.iter_mut().zip(AMEDIAFORMAT_KEY_STRINGS.iter()) {
            *slot = s.as_ptr() as *const c_char;
        }
        KeyPtrTable(ptrs)
    });
    std::ptr::addr_of!(t.0[idx]) as u64
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SlInterfaceId {
    time_low: u32,
    time_mid: u16,
    time_hi_and_version: u16,
    clock_seq: u16,
    node: [u8; 6],
}

struct SlIidStructs([SlInterfaceId; 7]);

unsafe impl Sync for SlIidStructs {}

unsafe impl Send for SlIidStructs {}

struct SlIidPtrs([*const SlInterfaceId; 7]);

unsafe impl Sync for SlIidPtrs {}

unsafe impl Send for SlIidPtrs {}

static SL_IID_STRUCTS: OnceLock<SlIidStructs> = OnceLock::new();
static SL_IID_PTRS: OnceLock<SlIidPtrs> = OnceLock::new();

fn sl_iid_addr(idx: usize) -> u64 {
    let structs = SL_IID_STRUCTS.get_or_init(|| {
        let mut ids = [SlInterfaceId {
            time_low: 0,
            time_mid: 0,
            time_hi_and_version: 0,
            clock_seq: 0,
            node: [0; 6],
        }; 7];
        for (i, id) in ids.iter_mut().enumerate() {
            id.time_low = i as u32;
        }
        SlIidStructs(ids)
    });

    let ptrs = SL_IID_PTRS
        .get_or_init(|| SlIidPtrs(std::array::from_fn(|i| std::ptr::addr_of!(structs.0[i]))));
    std::ptr::addr_of!(ptrs.0[idx]) as u64
}

pub(crate) fn sl_iid_addr_for_test(idx: usize) -> u64 {
    sl_iid_addr(idx)
}

pub(crate) fn sl_iid_index(iid_value: usize) -> Option<usize> {
    let _ = sl_iid_addr(0);
    let structs = SL_IID_STRUCTS.get()?;
    (0..7).find(|&i| std::ptr::addr_of!(structs.0[i]) as usize == iid_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::reloc::{apply_one, Rela, SliceImage, SymbolResolver, R_X86_64_GLOB_DAT};
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    static ANW_TEST_LOCK: Mutex<()> = Mutex::new(());

    thread_local! {
        static EMIT_CAPTURE: RefCell<Option<Vec<(c_int, String, String)>>> = const { RefCell::new(None) };
    }

    pub(super) fn capture_emit(priority: c_int, tag: &str, msg: &str) -> bool {
        EMIT_CAPTURE.with(|c| {
            let mut slot = c.borrow_mut();
            match slot.as_mut() {
                Some(buf) => {
                    buf.push((priority, tag.to_owned(), msg.to_owned()));
                    true
                }
                None => false,
            }
        })
    }

    fn with_capture(body: impl FnOnce()) -> Vec<(c_int, String, String)> {
        EMIT_CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        body();
        EMIT_CAPTURE.with(|c| c.borrow_mut().take().unwrap_or_default())
    }

    #[test]
    fn provider_resolves_registered_and_rejects_unregistered() {
        let p = EclipseNativeProvider::with_bionic_natives();

        let got = p.resolve("__android_log_write");
        assert!(got.is_some(), "__android_log_write must be registered");
        let got = got.unwrap();
        assert!(got.addr != 0, "registered native address must be non-null");
        assert!(!got.weak, "Eclipse natives are strong definitions");

        assert!(p.resolve("__memcpy_chk").is_none());
        assert!(p.resolve("__strlen_chk").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__errno").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__stack_chk_guard").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__sF").is_some_and(|r| r.addr != 0));

        assert!(p
            .resolve("__android_log_print")
            .is_some_and(|r| r.addr != 0));
        assert!(p
            .resolve("__android_log_assert")
            .is_some_and(|r| r.addr != 0));

        assert!(p
            .resolve("__android_log_vprint")
            .is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__umask_chk").is_some_and(|r| r.addr != 0));

        assert_eq!(p.resolve("memcpy"), None);
        assert_eq!(p.resolve("__eclipse_no_such_native__"), None);
    }

    #[test]
    fn with_bionic_natives_registers_the_three_implemented_categories() {
        let p = EclipseNativeProvider::with_bionic_natives();

        assert_eq!(
            p.len(),
            134 + super::super::bionic_pthread::PTHREAD_NATIVE_COUNT
                + super::super::bionic_sysconf::SYSQ_NATIVE_COUNT,
            "6 liblog + 16 bionic-libc + 25 bionic-stdio + 7 bionic-signal + 2 link-map \
             introspection + 4 netdb resolver-ABI + 1 EGL display interception + 4 Vulkan WSI \
             interception + 28 ndk-android + 33 media-ndk + 8 audio + 53 pthread + 5 sysconf \
             system-query natives registered"
        );
        for name in [
            "__android_log_write",
            "__android_log_buf_write",
            "android_set_abort_message",
            "__android_log_print",
            "__android_log_assert",
            "__android_log_vprint",
            "__strlen_chk",
            "__strchr_chk",
            "__strncpy_chk2",
            "__write_chk",
            "__fwrite_chk",
            "__sendto_chk",
            "__FD_SET_chk",
            "__FD_CLR_chk",
            "__FD_ISSET_chk",
            "__umask_chk",
            "__errno",
            "__assert2",
            "__gnu_strerror_r",
            "__system_property_get",
            "__stack_chk_guard",
            "__sF",
            "clearerr",
            "fclose",
            "feof",
            "ferror",
            "fflush",
            "fgets",
            "fileno",
            "fputc",
            "fputs",
            "fputwc",
            "fread",
            "__fread_chk",
            "fseek",
            "fseeko",
            "ftell",
            "ftello",
            "fwrite",
            "getc",
            "getwc",
            "setvbuf",
            "ungetc",
            "ungetwc",
            "fprintf",
            "fscanf",
            "vfprintf",
            "sigaction",
            "sigemptyset",
            "sigaddset",
            "sigfillset",
            "sigprocmask",
            "pthread_sigmask",
            "sigaltstack",
            "dl_iterate_phdr",
            "dladdr",
            "getaddrinfo",
            "freeaddrinfo",
            "gai_strerror",
            "getnameinfo",
            "eglGetDisplay",
            "vkGetInstanceProcAddr",
            "vkCreateInstance",
            "vkCreateAndroidSurfaceKHR",
            "AAssetManager_fromJava",
            "AAssetManager_open",
            "AAsset_close",
            "AAsset_getBuffer",
            "AAsset_getLength",
            "AAsset_openFileDescriptor",
            "AConfiguration_new",
            "AConfiguration_delete",
            "AConfiguration_fromAssetManager",
            "AConfiguration_getCountry",
            "AConfiguration_getLanguage",
            "AConfiguration_getNavHidden",
            "AConfiguration_getScreenHeightDp",
            "AConfiguration_getScreenSize",
            "AConfiguration_getScreenWidthDp",
            "ALooper_prepare",
            "ALooper_forThread",
            "ALooper_acquire",
            "ALooper_release",
            "ALooper_pollOnce",
            "ALooper_addFd",
            "ALooper_removeFd",
            "ANativeWindow_fromSurface",
            "ANativeWindow_getWidth",
            "ANativeWindow_getHeight",
            "ANativeWindow_getFormat",
            "ANativeWindow_acquire",
            "ANativeWindow_release",
            "AMediaCodec_configure",
            "AMediaCodec_createDecoderByType",
            "AMediaCodec_createEncoderByType",
            "AMediaCodec_delete",
            "AMediaCodec_dequeueInputBuffer",
            "AMediaCodec_dequeueOutputBuffer",
            "AMediaCodec_flush",
            "AMediaCodec_getInputBuffer",
            "AMediaCodec_getOutputBuffer",
            "AMediaCodec_getOutputFormat",
            "AMediaCodec_queueInputBuffer",
            "AMediaCodec_releaseOutputBuffer",
            "AMediaCodec_start",
            "AMediaCodec_stop",
            "AMediaFormat_delete",
            "AMediaFormat_getBuffer",
            "AMediaFormat_getInt32",
            "AMediaFormat_new",
            "AMediaFormat_setBuffer",
            "AMediaFormat_setFloat",
            "AMediaFormat_setInt32",
            "AMediaFormat_setString",
            "AMediaFormat_toString",
            "AMEDIAFORMAT_KEY_BIT_RATE",
            "AMEDIAFORMAT_KEY_CHANNEL_COUNT",
            "AMEDIAFORMAT_KEY_COLOR_FORMAT",
            "AMEDIAFORMAT_KEY_FRAME_RATE",
            "AMEDIAFORMAT_KEY_HEIGHT",
            "AMEDIAFORMAT_KEY_I_FRAME_INTERVAL",
            "AMEDIAFORMAT_KEY_MIME",
            "AMEDIAFORMAT_KEY_SAMPLE_RATE",
            "AMEDIAFORMAT_KEY_STRIDE",
            "AMEDIAFORMAT_KEY_WIDTH",
            "slCreateEngine",
            "SL_IID_ANDROIDCONFIGURATION",
            "SL_IID_ANDROIDSIMPLEBUFFERQUEUE",
            "SL_IID_BUFFERQUEUE",
            "SL_IID_ENGINE",
            "SL_IID_PLAY",
            "SL_IID_RECORD",
            "SL_IID_VOLUME",
            "pthread_mutex_lock",
            "pthread_mutex_unlock",
            "pthread_once",
            "pthread_key_create",
            "pthread_getspecific",
            "pthread_setspecific",
            "pthread_self",
            "pthread_cond_wait",
            "pthread_rwlock_rdlock",
            "sem_wait",
            "gettid",
            "syscall",
            "__cxa_thread_atexit_impl",
            "pthread_atfork",
            "sysconf",
            "getauxval",
            "sched_getcpu",
            "getpagesize",
            "sysinfo",
        ] {
            assert!(p.resolve(name).is_some(), "{name} must be registered");
        }
    }

    extern "C" {

        fn __android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;
    }

    #[test]
    fn variadic_shim_formats_and_forwards_to_eclipse_sink() {
        use std::ffi::CString;

        let tag = CString::new("EclipseTag").unwrap();
        let fmt = CString::new("n=%d s=%s hex=0x%x").unwrap();
        let s_arg = CString::new("hi").unwrap();

        let mut ret = 0;
        let emits = with_capture(|| {
            ret = unsafe {
                __android_log_print(
                    ANDROID_LOG_INFO,
                    tag.as_ptr(),
                    fmt.as_ptr(),
                    42_i32,
                    s_arg.as_ptr(),
                    0xbeef_u32,
                )
            };
        });

        assert_eq!(emits.len(), 1, "shim forwards exactly one line per call");
        let (prio, got_tag, got_msg) = &emits[0];
        assert_eq!(*prio, ANDROID_LOG_INFO, "priority passes through unchanged");
        assert_eq!(got_tag, "EclipseTag", "tag passes through unchanged");
        assert_eq!(
            got_msg, "n=42 s=hi hex=0xbeef",
            "the C shim's vsnprintf formatted the varargs correctly"
        );

        assert!(ret > 0, "__android_log_print returns the byte count (> 0)");
        assert_eq!(
            ret as usize,
            "n=42 s=hi hex=0xbeef".len(),
            "the returned byte count matches the formatted message length"
        );
    }

    #[test]
    fn variadic_shim_handles_null_tag_and_empty_format() {
        use std::ffi::CString;

        let fmt = CString::new("plain").unwrap();
        let emits = with_capture(|| {
            let _ =
                unsafe { __android_log_print(ANDROID_LOG_WARN, std::ptr::null(), fmt.as_ptr()) };
        });
        assert_eq!(emits.len(), 1);
        let (prio, got_tag, got_msg) = &emits[0];
        assert_eq!(*prio, ANDROID_LOG_WARN);
        assert_eq!(got_tag, "", "a null tag becomes an empty string");
        assert_eq!(got_msg, "plain");
    }

    #[test]
    fn eclipse_provider_beats_host_in_scope_order() {
        use crate::loader::resolve::{HostDlsymProvider, Scope};

        let mut eclipse = EclipseNativeProvider::empty();
        eclipse.register("memcpy", 0xdead_beef);
        let mut scope = Scope::new();
        scope.push(Box::new(eclipse));
        scope.push(Box::new(HostDlsymProvider));

        assert_eq!(scope.resolve("memcpy").map(|r| r.addr), Some(0xdead_beef));

        assert!(scope.resolve("malloc").is_some_and(|r| r.addr != 0));
    }

    #[test]
    fn strlen_chk_returns_length_within_bound() {
        let s = b"hello\0";

        let len = unsafe { eclipse_strlen_chk(s.as_ptr().cast(), 6) };
        assert_eq!(len, 5);
    }

    #[test]
    fn strchr_chk_finds_char_within_bound() {
        let s = b"abcde\0";

        let p = unsafe { eclipse_strchr_chk(s.as_ptr().cast(), b'c' as c_int, 6) };
        assert!(!p.is_null());

        assert_eq!(unsafe { *p } as u8, b'c');
    }

    #[test]
    fn umask_chk_forwards_a_valid_mode_and_round_trips() {
        unsafe {
            let saved = libc::umask(0o022);
            let prev = eclipse_umask_chk(0o077);
            assert_eq!(prev, 0o022, "__umask_chk returns the previous mask");
            let now = eclipse_umask_chk(0o022);
            assert_eq!(now, 0o077, "__umask_chk installed the requested mask");
            libc::umask(saved);
        }
    }

    #[test]
    fn errno_returns_thread_errno_location() {
        let p = eclipse_errno();
        assert!(
            !p.is_null(),
            "__errno must return a non-null errno location"
        );

        let host = unsafe { libc::__errno_location() };
        assert_eq!(p, host, "__errno forwards to the glibc per-thread errno");
    }

    #[test]
    fn system_property_get_reports_unset() {
        let mut buf = [0xAAu8; 92];
        let name = b"ro.build.version.sdk\0";

        let n =
            unsafe { eclipse_system_property_get(name.as_ptr().cast(), buf.as_mut_ptr().cast()) };
        assert_eq!(n, 0, "an unset property reports length 0");
        assert_eq!(
            buf[0], 0,
            "the value buffer must be an empty NUL-terminated string"
        );
    }

    #[test]
    fn bionic_sigaction_layout_matches_lp64() {
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_flags), 0);
        assert_eq!(std::mem::offset_of!(BionicSigaction, handler), 8);
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_mask), 16);
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_restorer), 24);
        assert_eq!(std::mem::size_of::<BionicSigaction>(), 32);

        assert_eq!(std::mem::size_of::<BionicSigsetT>(), 8);
        assert_eq!(std::mem::size_of::<libc::sigset_t>(), 128);
    }

    #[test]
    fn bionic_sigset_ops_match_the_bionic_contract() {
        let mut set: BionicSigsetT = 0xdead_beef;

        unsafe {
            assert_eq!(eclipse_sigemptyset(&mut set), 0);
            assert_eq!(set, 0, "sigemptyset clears exactly the 64-bit word");
            assert_eq!(eclipse_sigaddset(&mut set, libc::SIGURG), 0);
            assert_eq!(
                set,
                1u64 << (libc::SIGURG - 1),
                "sigaddset sets bit signum-1"
            );
            assert_eq!(eclipse_sigfillset(&mut set), 0);
            assert_eq!(set, !0u64, "sigfillset fills exactly the 64-bit word");

            assert_eq!(eclipse_sigaddset(&mut set, 0), -1);
            assert_eq!(eclipse_sigaddset(&mut set, 65), -1);
            assert_eq!(*libc::__errno_location(), libc::EINVAL);

            assert_eq!(eclipse_sigemptyset(std::ptr::null_mut()), -1);
            assert_eq!(eclipse_sigfillset(std::ptr::null_mut()), -1);
        }
    }

    #[test]
    fn bionic_sigset_translation_round_trips() {
        let bionic: BionicSigsetT = (1 << (libc::SIGURG - 1)) | (1 << (libc::SIGUSR2 - 1));
        let glibc = glibc_sigset_from_bionic(bionic);

        unsafe {
            assert_eq!(libc::sigismember(&glibc, libc::SIGURG), 1);
            assert_eq!(libc::sigismember(&glibc, libc::SIGUSR2), 1);
            assert_eq!(libc::sigismember(&glibc, libc::SIGUSR1), 0);
        }

        assert_eq!(bionic_sigset_from_glibc(&glibc), bionic);
    }

    static SIGNAL_TEST_RECEIVED: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn signal_test_handler(
        signum: c_int,
        _info: *mut libc::siginfo_t,
        _ctx: *mut c_void,
    ) {
        SIGNAL_TEST_RECEIVED.store(signum as usize, Ordering::SeqCst);
    }

    #[test]
    fn bionic_sigaction_registers_a_live_handler_and_round_trips_oldact() {
        let act = BionicSigaction {
            sa_flags: libc::SA_SIGINFO,
            handler: signal_test_handler as *const () as usize,
            sa_mask: 0,
            sa_restorer: 0,
        };
        let mut old = BionicSigaction {
            sa_flags: 0,
            handler: usize::MAX,
            sa_mask: !0,
            sa_restorer: usize::MAX,
        };

        unsafe {
            assert_eq!(eclipse_sigaction(libc::SIGURG, &act, &mut old), 0);
            assert_eq!(
                old.handler,
                libc::SIG_DFL,
                "oldact reports the prior (default) disposition"
            );
            assert_eq!(old.sa_restorer, 0, "glibc's restorer is never leaked back");
            libc::raise(libc::SIGURG);
        }
        assert_eq!(
            SIGNAL_TEST_RECEIVED.load(Ordering::SeqCst),
            libc::SIGURG as usize,
            "the kernel delivered SIGURG to the bionic-registered handler"
        );

        let mut requeried = old;

        unsafe {
            assert_eq!(
                eclipse_sigaction(libc::SIGURG, &old, std::ptr::null_mut()),
                0
            );
            assert_eq!(
                eclipse_sigaction(libc::SIGURG, std::ptr::null(), &mut requeried),
                0
            );
        }
        assert_eq!(requeried.handler, old.handler, "restore round-trips");
    }

    #[test]
    fn bionic_sigprocmask_translates_both_directions() {
        let mut block: BionicSigsetT = 0;
        let mut prev: BionicSigsetT = !0;

        unsafe {
            assert_eq!(eclipse_sigaddset(&mut block, libc::SIGURG), 0);
            assert_eq!(eclipse_sigprocmask(libc::SIG_BLOCK, &block, &mut prev), 0);

            let mut host_mask: libc::sigset_t = std::mem::zeroed();
            assert_eq!(
                libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut host_mask),
                0
            );
            assert_eq!(libc::sigismember(&host_mask, libc::SIGURG), 1);

            assert_eq!(
                eclipse_sigprocmask(libc::SIG_SETMASK, &prev, std::ptr::null_mut()),
                0
            );
        }
    }

    #[test]
    fn bionic_addrinfo_layout_is_bsd_order_and_differs_from_glibc() {
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_flags), 0);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_family), 4);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_socktype), 8);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_protocol), 12);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addrlen), 16);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_canonname), 24);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addr), 32);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_next), 40);
        assert_eq!(std::mem::size_of::<BionicAddrinfo>(), 48);

        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_addr), 24);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_canonname), 32);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_next), 40);
        assert_eq!(std::mem::size_of::<libc::addrinfo>(), 48);
    }

    #[test]
    fn bionic_ai_ni_eai_translation_tables_match_both_headers() {
        assert_eq!(translate_flags_by_name(0, AI_FLAG_PAIRS), Ok(0));
        assert_eq!(
            translate_flags_by_name(0x0001, AI_FLAG_PAIRS),
            Ok(libc::AI_PASSIVE)
        );
        assert_eq!(
            translate_flags_by_name(0x0002, AI_FLAG_PAIRS),
            Ok(libc::AI_CANONNAME)
        );
        assert_eq!(
            translate_flags_by_name(0x0004, AI_FLAG_PAIRS),
            Ok(libc::AI_NUMERICHOST)
        );
        assert_eq!(
            translate_flags_by_name(0x0008, AI_FLAG_PAIRS),
            Ok(libc::AI_NUMERICSERV)
        );
        assert_eq!(
            translate_flags_by_name(0x0100, AI_FLAG_PAIRS),
            Ok(libc::AI_ALL)
        );
        assert_eq!(
            translate_flags_by_name(0x0400, AI_FLAG_PAIRS),
            Ok(libc::AI_ADDRCONFIG)
        );
        assert_eq!(
            translate_flags_by_name(0x0800, AI_FLAG_PAIRS),
            Ok(libc::AI_V4MAPPED)
        );

        assert_eq!(libc::AI_NUMERICSERV, 0x0400);
        assert_ne!(libc::AI_ADDRCONFIG, 0x0400);
        assert_eq!(
            translate_flags_by_name(0x4000, AI_FLAG_PAIRS),
            Err(BIONIC_EAI_BADFLAGS)
        );

        assert_eq!(
            translate_flags_by_name(0x1, NI_FLAG_PAIRS),
            Ok(libc::NI_NOFQDN)
        );
        assert_eq!(
            translate_flags_by_name(0x2, NI_FLAG_PAIRS),
            Ok(libc::NI_NUMERICHOST)
        );
        assert_eq!(
            translate_flags_by_name(0x4, NI_FLAG_PAIRS),
            Ok(libc::NI_NAMEREQD)
        );
        assert_eq!(
            translate_flags_by_name(0x8, NI_FLAG_PAIRS),
            Ok(libc::NI_NUMERICSERV)
        );
        assert_eq!(
            translate_flags_by_name(0x10, NI_FLAG_PAIRS),
            Ok(libc::NI_DGRAM)
        );
        assert_eq!(
            translate_flags_by_name(0x100, NI_FLAG_PAIRS),
            Err(BIONIC_EAI_BADFLAGS)
        );

        assert_eq!(bionic_eai_from_glibc(0), 0);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_BADFLAGS), 3);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_NONAME), 8);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_AGAIN), 2);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_FAIL), 4);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_NODATA), 7);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_FAMILY), 5);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_SOCKTYPE), 10);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_SERVICE), 9);
        assert_eq!(bionic_eai_from_glibc(GLIBC_EAI_ADDRFAMILY), 1);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_MEMORY), 6);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_SYSTEM), 11);
        assert_eq!(bionic_eai_from_glibc(libc::EAI_OVERFLOW), 14);

        assert_eq!(bionic_eai_from_glibc(-100), BIONIC_EAI_FAIL);
    }

    #[test]
    fn bionic_getaddrinfo_returns_bionic_shaped_nodes_and_positive_eai() {
        let node = std::ffi::CString::new("127.0.0.1").expect("cstring");

        let mut hints: BionicAddrinfo = unsafe { std::mem::zeroed() };
        hints.ai_flags = 0x0004 | 0x0002;
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut BionicAddrinfo = std::ptr::null_mut();

        let rc = unsafe { eclipse_getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut res) };
        assert_eq!(rc, 0, "numeric-host lookup must succeed offline");
        assert!(!res.is_null(), "success must produce a chain");

        let first = unsafe { &*res };
        assert_eq!(first.ai_family, libc::AF_INET);
        assert!(
            !first.ai_addr.is_null(),
            "ai_addr (the BIONIC @32 slot) must be populated"
        );
        assert_eq!(
            first.ai_addrlen as usize,
            std::mem::size_of::<libc::sockaddr_in>()
        );

        let sin = unsafe { &*(first.ai_addr.cast::<libc::sockaddr_in>()) };
        assert_eq!(sin.sin_family, libc::AF_INET as libc::sa_family_t);
        assert_eq!(u32::from_be(sin.sin_addr.s_addr), 0x7f00_0001);

        assert!(!first.ai_canonname.is_null(), "AI_CANONNAME requested");

        let canon = unsafe { std::ffi::CStr::from_ptr(first.ai_canonname) };
        assert_eq!(canon.to_str().expect("utf-8"), "127.0.0.1");

        unsafe { eclipse_freeaddrinfo(res) };

        let bad = std::ffi::CString::new("not-an-ip.invalid").expect("cstring");
        let mut res2: *mut BionicAddrinfo = std::ptr::null_mut();

        let rc = unsafe { eclipse_getaddrinfo(bad.as_ptr(), std::ptr::null(), &hints, &mut res2) };
        assert_eq!(rc, BIONIC_EAI_NONAME);
        assert!(rc > 0, "bionic EAI codes are POSITIVE");
        assert!(res2.is_null(), "failure must not hand out a chain");

        let msg = unsafe { eclipse_gai_strerror(rc) };
        assert!(!msg.is_null());

        let s = unsafe { std::ffi::CStr::from_ptr(msg) }
            .to_str()
            .expect("ascii");
        assert!(s.contains("not known"), "the NONAME message, got: {s}");
    }

    #[test]
    fn bionic_getnameinfo_translates_flags_and_returns_numeric_host() {
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = 80u16.to_be();
        sin.sin_addr.s_addr = 0x7f00_0001u32.to_be();
        let mut host = [0 as c_char; 64];
        let mut serv = [0 as c_char; 16];

        let rc = unsafe {
            eclipse_getnameinfo(
                (&raw const sin).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                host.as_mut_ptr(),
                host.len() as libc::socklen_t,
                serv.as_mut_ptr(),
                serv.len() as libc::socklen_t,
                0x2 | 0x8,
            )
        };
        assert_eq!(rc, 0);

        let h = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }
            .to_str()
            .expect("ascii");

        let s = unsafe { std::ffi::CStr::from_ptr(serv.as_ptr()) }
            .to_str()
            .expect("ascii");
        assert_eq!(h, "127.0.0.1");
        assert_eq!(s, "80");

        let rc = unsafe {
            eclipse_getnameinfo(
                (&raw const sin).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                host.as_mut_ptr(),
                host.len() as libc::socklen_t,
                serv.as_mut_ptr(),
                serv.len() as libc::socklen_t,
                0x100,
            )
        };
        assert_eq!(rc, BIONIC_EAI_BADFLAGS);
    }

    static TAP_TEST_RECEIVED: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn tap_test_chain_handler(
        signum: c_int,
        _info: *mut libc::siginfo_t,
        _ctx: *mut c_void,
    ) {
        TAP_TEST_RECEIVED.store(signum as usize, Ordering::SeqCst);
    }

    #[test]
    fn early_fault_tap_intercepts_registration_and_chains() {
        let sig = libc::SIGWINCH;

        let mut snapshot: libc::sigaction = unsafe { std::mem::zeroed() };

        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut snapshot), 0);
        }

        let cursor_before = TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst);
        assert!(
            install_early_fault_tap(libc::SIGKILL).is_err(),
            "the kernel must reject installing over SIGKILL"
        );
        assert_eq!(
            TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst),
            cursor_before + 1,
            "the chain seed is claimed BEFORE the kernel install"
        );
        assert!(
            !TAP_CHAIN.load(Ordering::Acquire).is_null(),
            "the chain slot is published before the (failed) install"
        );
        assert_eq!(
            TAPPED_SIGNAL.load(Ordering::SeqCst),
            0,
            "a failed install never opens the seam gate"
        );

        let cursor_pre_install = TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst);
        install_early_fault_tap(sig).expect("tap install");

        assert_eq!(
            TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst),
            cursor_pre_install + 1,
            "a quiescent install claims exactly one pool cell (no spurious re-seed)"
        );

        let mut kernel: libc::sigaction = unsafe { std::mem::zeroed() };

        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut kernel), 0);
        }
        assert_eq!(
            kernel.sa_sigaction, early_fault_tap_handler as *const () as usize,
            "the tap is kernel-registered"
        );
        assert_ne!(kernel.sa_flags & libc::SA_SIGINFO, 0, "SA_SIGINFO is set");

        let mut old = BionicSigaction {
            sa_flags: 0,
            handler: usize::MAX,
            sa_mask: !0,
            sa_restorer: usize::MAX,
        };

        unsafe {
            assert_eq!(eclipse_sigaction(sig, std::ptr::null(), &mut old), 0);
        }
        assert_eq!(
            old.handler, snapshot.sa_sigaction,
            "the chain slot holds the pre-tap disposition"
        );
        assert_eq!(
            old.sa_restorer, 0,
            "glibc's restorer never crosses the seam"
        );

        let act = BionicSigaction {
            sa_flags: libc::SA_SIGINFO,
            handler: tap_test_chain_handler as *const () as usize,
            sa_mask: 0,
            sa_restorer: 0,
        };
        let mut old2 = old;

        unsafe {
            assert_eq!(eclipse_sigaction(sig, &act, &mut old2), 0);
        }
        assert_eq!(old2.handler, old.handler, "previous occupant round-trips");

        let mut kernel2: libc::sigaction = unsafe { std::mem::zeroed() };

        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut kernel2), 0);
        }
        assert_eq!(
            kernel2.sa_sigaction, early_fault_tap_handler as *const () as usize,
            "the kernel slot is STILL the tap — the engine registration never reached the kernel"
        );

        let chain_ptr = TAP_CHAIN.load(Ordering::Acquire) as usize;
        let pool_start = TAP_CHAIN_POOL.0.as_ptr() as usize;
        let pool_end = pool_start + std::mem::size_of_val(&TAP_CHAIN_POOL.0);
        assert!(
            (pool_start..pool_end).contains(&chain_ptr),
            "the chain slot points into the static pool, never the heap"
        );

        unsafe {
            libc::raise(sig);
        }
        assert_eq!(
            TAP_TEST_RECEIVED.load(Ordering::SeqCst),
            sig as usize,
            "kernel → tap → chained engine handler delivered end-to-end"
        );
        assert_eq!(
            TAP_HANDLER_TID.load(Ordering::SeqCst),
            0,
            "the re-entry latch is cleared after a normal pass"
        );

        static TAP_TEST_PARK_RELEASED: AtomicBool = AtomicBool::new(false);
        static TAP_TEST_CHAIN_ENTRIES: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn tap_test_parking_chain_handler(
            _signum: c_int,
            _info: *mut libc::siginfo_t,
            _ctx: *mut c_void,
        ) {
            if TAP_TEST_CHAIN_ENTRIES.fetch_add(1, Ordering::SeqCst) == 0 {
                while !TAP_TEST_PARK_RELEASED.load(Ordering::SeqCst) {
                    std::hint::spin_loop();
                }
            }
        }
        let park_act = BionicSigaction {
            sa_flags: libc::SA_SIGINFO,
            handler: tap_test_parking_chain_handler as *const () as usize,
            sa_mask: 0,
            sa_restorer: 0,
        };

        unsafe {
            assert_eq!(eclipse_sigaction(sig, &park_act, std::ptr::null_mut()), 0);
        }
        let parker = std::thread::spawn(move || {
            unsafe { libc::raise(sig) };
        });

        while TAP_TEST_CHAIN_ENTRIES.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let owner_while_parked = TAP_HANDLER_TID.load(Ordering::SeqCst);

        unsafe {
            libc::raise(sig);
        }

        let entries_while_parked = TAP_TEST_CHAIN_ENTRIES.load(Ordering::SeqCst);

        let mut kernel3: libc::sigaction = unsafe { std::mem::zeroed() };

        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut kernel3), 0);
        }
        TAP_TEST_PARK_RELEASED.store(true, Ordering::SeqCst);
        parker.join().expect("parker thread");
        assert_ne!(
            owner_while_parked, 0,
            "the parked thread's tid holds the latch"
        );
        assert_eq!(
            entries_while_parked, 2,
            "a concurrent different-tid delivery chains instead of dying to SIG_DFL"
        );
        assert_eq!(
            kernel3.sa_sigaction, early_fault_tap_handler as *const () as usize,
            "the kernel slot survives a concurrent delivery (never restored to SIG_DFL)"
        );
        assert_eq!(
            TAP_HANDLER_TID.load(Ordering::SeqCst),
            0,
            "the owner released the latch after the parked run"
        );

        unsafe {
            assert_eq!(eclipse_sigaction(sig, &old, std::ptr::null_mut()), 0);
        }
        let mut requeried = act;

        unsafe {
            assert_eq!(eclipse_sigaction(sig, std::ptr::null(), &mut requeried), 0);
        }
        assert_eq!(requeried.handler, old.handler, "the chain slot reverts");

        TAPPED_SIGNAL.store(0, Ordering::SeqCst);
        TAP_CHAIN.store(std::ptr::null_mut(), Ordering::SeqCst);

        unsafe {
            assert_eq!(libc::sigaction(sig, &snapshot, std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn tap_chain_pool_publishes_in_place_and_keeps_last_occupant_on_exhaustion() {
        let pool = TapChainPool::new();
        let next = AtomicUsize::new(0);
        let slot: AtomicPtr<BionicSigaction> = AtomicPtr::new(std::ptr::null_mut());
        let pool_start = pool.0.as_ptr() as usize;
        let pool_end = pool_start + std::mem::size_of_val(&pool.0);
        let mk = |handler: usize| BionicSigaction {
            sa_flags: libc::SA_SIGINFO,
            handler,
            sa_mask: 0,
            sa_restorer: 0,
        };

        let mut published = Vec::new();
        for k in 0..TAP_CHAIN_POOL_LEN {
            assert!(tap_chain_publish(&pool, &next, &slot, mk(0x1000 + k)));
            let p = slot.load(Ordering::Acquire);
            assert!(
                (pool_start..pool_end).contains(&(p as usize)),
                "published pointer must be a pool cell, never a heap allocation"
            );
            assert!(!published.contains(&(p as usize)), "cells are claim-once");
            published.push(p as usize);

            assert_eq!(unsafe { (*p).handler }, 0x1000 + k);
        }

        let last = slot.load(Ordering::Acquire);
        assert!(!tap_chain_publish(&pool, &next, &slot, mk(0xdead)));
        assert_eq!(
            slot.load(Ordering::Acquire),
            last,
            "exhaustion keeps the last occupant"
        );

        assert_eq!(
            unsafe { (*last).handler },
            0x1000 + (TAP_CHAIN_POOL_LEN - 1)
        );
    }

    #[test]
    fn tap_entry_claim_is_tid_scoped_not_process_global() {
        let latch = AtomicI64::new(0);

        assert_eq!(tap_entry_claim(&latch, 101), TapEntryClaim::Latched);
        assert_eq!(latch.load(Ordering::SeqCst), 101);

        assert_eq!(
            tap_entry_claim(&latch, 101),
            TapEntryClaim::SameThreadReentry
        );

        assert_eq!(tap_entry_claim(&latch, 202), TapEntryClaim::Unlatched);
        assert_eq!(
            latch.load(Ordering::SeqCst),
            101,
            "an Unlatched entry never disturbs the owner's claim"
        );

        latch.store(0, Ordering::SeqCst);
        assert_eq!(tap_entry_claim(&latch, 202), TapEntryClaim::Latched);
        assert_eq!(latch.load(Ordering::SeqCst), 202);
    }

    #[test]
    fn tap_stack_walk_bounds_and_validates() {
        let mut mem = Box::new([0u64; 64]);
        let base = mem.as_ptr() as u64;
        for k in 0..5usize {
            mem[k * 8] = if k < 4 {
                base + ((k + 1) * 64) as u64
            } else {
                0
            };
            mem[k * 8 + 1] = 0x1000_0000 + k as u64;
        }
        let rip = 0xdead_0000u64;
        let rsp = base.wrapping_sub(64);
        let mut out = [0u64; 32];

        let n = tap_stack_walk(rip, rsp, base, &mut out);
        assert_eq!(n, 5);
        assert_eq!(out[0], rip, "frame 0 is RIP itself");
        for k in 0..4u64 {
            assert_eq!(out[(k + 1) as usize], 0x1000_0000 + k);
        }

        assert_eq!(tap_stack_walk(rip, rsp, base + 1, &mut out), 1);

        assert_eq!(tap_stack_walk(rip, base, base, &mut out), 1);

        mem[0] = base;
        assert_eq!(tap_stack_walk(rip, rsp, base, &mut out), 1);

        mem[0] = base + (1 << 20);
        assert_eq!(tap_stack_walk(rip, rsp, base, &mut out), 1);

        let mut long = Box::new([0u64; 128]);
        let lbase = long.as_ptr() as u64;
        for k in 0..63usize {
            long[k * 2] = lbase + ((k + 1) * 16) as u64;
            long[k * 2 + 1] = 0x2000_0000 + k as u64;
        }
        assert_eq!(
            tap_stack_walk(rip, lbase.wrapping_sub(64), lbase, &mut out),
            32,
            "the walk caps at the 32-entry buffer"
        );

        let local = 0xfeed_face_cafe_beefu64;
        assert_eq!(
            tap_read_u64(&raw const local as u64),
            Some(local),
            "process_vm_readv reads a mapped local"
        );
        assert_eq!(
            tap_read_u64(0x10),
            None,
            "an unmapped address yields None, never a fault"
        );
    }

    extern "C" fn tap_test_dummy_restorer() {}

    #[test]
    fn tap_si_code_consts_match_kernel_uapi() {
        assert_eq!(SEGV_MAPERR, 1);
        assert_eq!(SEGV_ACCERR, 2);

        let mut g: libc::sigaction = unsafe { std::mem::zeroed() };
        g.sa_sigaction = 0x1234;
        g.sa_flags = libc::SA_SIGINFO | SA_RESTORER_FLAG;
        g.sa_mask = glibc_sigset_from_bionic(1 << (libc::SIGURG - 1));
        g.sa_restorer = Some(tap_test_dummy_restorer);
        let b = bionic_action_from_glibc(&g);
        assert_eq!(b.sa_flags, libc::SA_SIGINFO, "SA_RESTORER stripped");
        assert_eq!(b.handler, 0x1234, "the handler carries over");
        assert_eq!(b.sa_mask, 1 << (libc::SIGURG - 1), "the mask narrows");
        assert_eq!(b.sa_restorer, 0, "the restorer pointer is never carried");
    }

    #[test]
    fn guarded_altstack_installs_eclipse_region_with_a_prot_none_guard_page() {
        let mut saved: libc::stack_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaltstack(std::ptr::null(), &mut saved) },
            0,
            "query the pre-test altstack state"
        );

        let st = install_guarded_altstack().expect("install the Eclipse guard-paged altstack");

        let mut q: libc::stack_t = unsafe { std::mem::zeroed() };

        assert_eq!(unsafe { libc::sigaltstack(std::ptr::null(), &mut q) }, 0);
        assert_eq!(
            q.ss_sp as u64, st.ss_sp,
            "the active altstack must be Eclipse's mmap'd region"
        );
        assert_eq!(q.ss_size, st.ss_size, "the active size is Eclipse's");
        assert_eq!(q.ss_flags & libc::SS_DISABLE, 0, "the stack is enabled");

        assert!(
            st.ss_size >= 2 * ALTSTACK_CHAIN_BUDGET,
            "ss_size {} must dominate the measured ~79.2 KiB fatal-chain budget",
            st.ss_size
        );

        let page = crate::loader::map::host_page_size();
        assert_eq!(st.guard_base + page, st.ss_sp, "one guard page below ss_sp");
        assert!(
            tap_read_u64(st.ss_sp).is_some(),
            "the stack region is mapped + readable"
        );
        assert!(
            tap_read_u64(st.ss_sp + st.ss_size as u64 - 8).is_some(),
            "the top of the stack region is mapped"
        );
        assert!(
            tap_read_u64(st.guard_base).is_none(),
            "the guard page is PROT_NONE (unreadable)"
        );
        assert!(
            tap_read_u64(st.ss_sp - 8).is_none(),
            "the bytes immediately below ss_sp fall in the guard page"
        );

        assert_eq!(
            unsafe { libc::sigaltstack(&saved, std::ptr::null_mut()) },
            0
        );

        unsafe { libc::munmap(st.guard_base as *mut c_void, st.mapping_len) };
    }

    #[test]
    fn sigaltstack_native_forwards_and_records_caller_attribution() {
        let mut saved: libc::stack_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaltstack(std::ptr::null(), &mut saved) },
            0
        );

        let before_total = altstack_registration_total();
        let mut q: libc::stack_t = unsafe { std::mem::zeroed() };

        assert_eq!(unsafe { eclipse_sigaltstack(std::ptr::null(), &mut q) }, 0);
        assert_eq!(
            altstack_registration_total(),
            before_total,
            "a pure query records nothing"
        );

        let mut stack = vec![0u8; libc::SIGSTKSZ];
        let ss = libc::stack_t {
            ss_sp: stack.as_mut_ptr() as *mut c_void,
            ss_flags: 0,
            ss_size: stack.len(),
        };

        assert_eq!(
            unsafe { eclipse_sigaltstack(&ss, std::ptr::null_mut()) },
            0,
            "the forward must reach the kernel and succeed"
        );

        let mut active: libc::stack_t = unsafe { std::mem::zeroed() };

        assert_eq!(
            unsafe { libc::sigaltstack(std::ptr::null(), &mut active) },
            0
        );
        assert_eq!(active.ss_sp as u64, ss.ss_sp as u64);
        assert_eq!(active.ss_size, ss.ss_size);

        let my_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i64;
        let recs = recent_altstack_registrations();
        let rec = recs
            .iter()
            .rev()
            .find(|r| r.ss_sp == ss.ss_sp as u64)
            .expect("the registration must be recorded");
        assert_eq!(rec.tid, my_tid, "the record names the registering thread");
        assert_eq!(rec.ss_size, ss.ss_size);
        assert_eq!(rec.ss_flags, 0);
        assert_ne!(rec.caller, 0, "the shim captured a return address");
        assert!(
            rec.caller_module.is_some(),
            "the caller resolves to a module (host-dladdr fallback names the test binary)"
        );

        assert_eq!(
            unsafe { libc::sigaltstack(&saved, std::ptr::null_mut()) },
            0
        );
    }

    #[test]
    fn stack_chk_guard_is_stable_nonzero_with_zero_low_byte() {
        let a = eclipse_stack_chk_guard_addr();
        let b = eclipse_stack_chk_guard_addr();
        assert_eq!(a, b, "the guard address is stable");
        let val = ECLIPSE_STACK_CHK_GUARD.load(Ordering::SeqCst);
        assert_ne!(val, 0, "the guard word is initialized non-zero");
        assert_eq!(val & 0xff, 0, "SSP convention: the guard's low byte is 0");
    }

    #[test]
    fn sf_backing_is_bionic_shaped_three_structs() {
        assert_eq!(SF_FILE_STRIDE, 152, "LP64 sizeof(struct __sFILE)");
        assert_eq!(SF_FILE_STRIDE, 0x98, "bionic &__sF[1] offset");
        assert_eq!(2 * SF_FILE_STRIDE, 0x130, "bionic &__sF[2] offset");
        assert_eq!(SF_BACKING_LEN, 456, "3 x 152-byte entries");
        assert_eq!(std::mem::size_of::<SfBacking>(), SF_BACKING_LEN);
        assert_eq!(
            std::mem::align_of::<SfBacking>(),
            8,
            "aligned(sizeof(void*))"
        );

        let p = EclipseNativeProvider::with_bionic_natives();
        let registered = p.resolve("__sF").expect("__sF registered").addr;
        assert_eq!(registered, eclipse_sf_addr());
        assert_eq!(registered % 8, 0, "the backing honors the ABI alignment");
        for i in 0..SF_ENTRY_COUNT as u64 {
            let entry = registered + i * SF_FILE_STRIDE as u64;
            assert!(
                entry + SF_FILE_STRIDE as u64 <= registered + SF_BACKING_LEN as u64,
                "&__sF[{i}] + sizeof(struct __sFILE) stays inside the Eclipse-owned backing"
            );
        }
    }

    #[test]
    fn sf_sentinels_translate_to_host_streams() {
        let base = eclipse_sf_addr() as usize;
        let s0 = eclipse_sf_translate_stream(base as *mut libc::FILE);
        let s1 = eclipse_sf_translate_stream((base + SF_FILE_STRIDE) as *mut libc::FILE);
        let s2 = eclipse_sf_translate_stream((base + 2 * SF_FILE_STRIDE) as *mut libc::FILE);

        let (g0, g1, g2) = unsafe { (stdin, stdout, stderr) };
        assert_eq!(s0, g0, "&__sF[0] -> glibc stdin");
        assert_eq!(s1, g1, "&__sF[1] -> glibc stdout");
        assert_eq!(s2, g2, "&__sF[2] -> glibc stderr");

        unsafe {
            assert_eq!(eclipse_fileno(base as *mut libc::FILE), 0);
            assert_eq!(
                eclipse_fileno((base + SF_FILE_STRIDE) as *mut libc::FILE),
                1
            );
            assert_eq!(
                eclipse_fileno((base + 2 * SF_FILE_STRIDE) as *mut libc::FILE),
                2
            );
        }

        let interior = (base + 8) as *mut libc::FILE;
        assert_eq!(eclipse_sf_translate_stream(interior), interior);
        let null: *mut libc::FILE = std::ptr::null_mut();
        assert_eq!(eclipse_sf_translate_stream(null), null);

        let msg = std::ffi::CString::new(
            "eclipse __sF regression pin: fputs(&__sF[2]) reaches host stderr\n",
        )
        .unwrap();

        let ret =
            unsafe { eclipse_fputs(msg.as_ptr(), (base + 2 * SF_FILE_STRIDE) as *mut libc::FILE) };
        assert!(ret >= 0, "fputs through the stderr sentinel succeeds");
    }

    #[test]
    fn sf_stdio_natives_round_trip_a_real_stream() {
        use std::ffi::CString;

        let f = unsafe { libc::tmpfile() };
        assert!(!f.is_null(), "tmpfile available");

        let line = CString::new("num 42\n").unwrap();
        let fmt_out = CString::new("%s %d\n").unwrap();
        let word = CString::new("val").unwrap();
        let fmt_in = CString::new("num %d").unwrap();

        unsafe {
            assert!(eclipse_fputs(line.as_ptr(), f) >= 0);
            assert_eq!(
                eclipse_fprintf(f, fmt_out.as_ptr(), word.as_ptr(), 7_i32),
                6,
                "C-shim fprintf formats and writes through the pass-through stream"
            );
            assert_eq!(eclipse_fwrite(b"bytes".as_ptr().cast(), 1, 5, f), 5);
            assert_eq!(eclipse_fputc(c_int::from(b'\n'), f), c_int::from(b'\n'));
            assert_eq!(eclipse_fflush(f), 0);

            assert_eq!(eclipse_ftell(f), 19);
            assert_eq!(eclipse_ftello(f), 19);
            assert_eq!(eclipse_fseek(f, 0, libc::SEEK_SET), 0);

            let mut n: c_int = 0;
            assert_eq!(
                eclipse_fscanf(f, fmt_in.as_ptr(), &raw mut n),
                1,
                "C-shim fscanf converts through the pass-through stream"
            );
            assert_eq!(n, 42);

            assert_eq!(eclipse_getc(f), c_int::from(b'\n'));

            let mut buf = [0u8; 32];
            assert!(!eclipse_fgets(buf.as_mut_ptr().cast(), buf.len() as c_int, f).is_null());
            let got = std::ffi::CStr::from_ptr(buf.as_ptr().cast());
            assert_eq!(got.to_bytes(), b"val 7\n");

            let mut tail = [0u8; 6];
            assert_eq!(eclipse_fread(tail.as_mut_ptr().cast(), 1, 6, f), 6);
            assert_eq!(&tail, b"bytes\n");

            assert_eq!(eclipse_getc(f), libc::EOF);
            assert_ne!(
                eclipse_feof(f),
                0,
                "EOF flag set after the read past the end"
            );
            eclipse_clearerr(f);
            assert_eq!(eclipse_feof(f), 0, "clearerr resets the EOF flag");
            assert_eq!(eclipse_ferror(f), 0);
            assert_eq!(eclipse_ungetc(c_int::from(b'Z'), f), c_int::from(b'Z'));
            assert_eq!(eclipse_getc(f), c_int::from(b'Z'));

            assert!(eclipse_fileno(f) > 2, "a real stream keeps its own fd");
            assert_eq!(eclipse_fseeko(f, 0, libc::SEEK_SET), 0);
            assert_eq!(eclipse_fclose(f), 0);
        }
    }

    #[test]
    fn fread_chk_uses_the_bionic_argument_order_and_honors_the_bound() {
        let f = unsafe { libc::tmpfile() };
        assert!(!f.is_null(), "tmpfile available");

        unsafe {
            assert_eq!(eclipse_fwrite(b"abcdef".as_ptr().cast(), 1, 6, f), 6);
            assert_eq!(eclipse_fseek(f, 0, libc::SEEK_SET), 0);
            let mut buf = [0u8; 8];

            assert_eq!(
                eclipse_fread_chk(buf.as_mut_ptr().cast(), 1, 6, f, buf.len()),
                6
            );
            assert_eq!(&buf[..6], b"abcdef");
            assert_eq!(eclipse_fclose(f), 0);
        }
    }

    #[test]
    fn registered_native_fills_a_got_slot_via_reloc_core() {
        use crate::loader::elf::DynSym;
        use crate::loader::resolve::{Scope, ScopedResolver};

        let dynsyms = vec![DynSym {
            name: "__android_log_write".to_string(),
            value: 0,
            size: 0,
            bind: 1,
            sym_type: 2,
            shndx: 0,
        }];
        let mut scope = Scope::new();
        scope.push(Box::new(EclipseNativeProvider::with_bionic_natives()));
        let resolver = ScopedResolver::new(&scope, &dynsyms);

        let eclipse_addr = resolver.resolve_symbol(0).expect("Eclipse native resolves");
        assert!(eclipse_addr != 0);

        let mut got = vec![0u8; 8];
        let mut image = SliceImage::new(0, 0, &mut got);
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            r_type: R_X86_64_GLOB_DAT,
            addend: 0,
        };
        apply_one(&mut image, &resolver, &rela).expect("apply GLOB_DAT");
        let slot = u64::from_le_bytes(got.try_into().unwrap());
        assert_eq!(
            slot, eclipse_addr,
            "the GOT slot holds the Eclipse native address"
        );
    }

    fn write_test_apk(tag: &str, entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        use std::io::{Cursor, Write};
        use zip::write::SimpleFileOptions;
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file(*name, opts).expect("start_file");
            w.write_all(bytes).expect("write_all");
        }
        let bytes = w.finish().expect("finish").into_inner();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-ndk-asset-{tag}-{:?}.apk",
            std::thread::current().id()
        ));
        std::fs::write(&path, &bytes).expect("write temp apk");
        path
    }

    #[test]
    fn aasset_open_getbuffer_getlength_round_trips_real_apk_bytes() {
        let payload: &[u8] = b"ECLIPSE-ASSET-CONTENTS-1234567890";
        let apk = write_test_apk("rt", &[("assets/config/app.txt", payload)]);

        let mgr_h = ndk_registry::asset_managers()
            .insert(AssetManagerState {
                apk_path: apk.clone(),
            })
            .expect("insert asset manager");
        let mgr = handle_to_ptr::<c_void>(mgr_h);

        let name = std::ffi::CString::new("config/app.txt").unwrap();

        let asset = unsafe { eclipse_aassetmanager_open(mgr, name.as_ptr(), 0) };
        assert!(!asset.is_null(), "opening a present asset must succeed");

        let len = unsafe { eclipse_aasset_getlength(asset) };
        assert_eq!(len as usize, payload.len(), "getLength == real byte count");

        let buf = unsafe { eclipse_aasset_getbuffer(asset) };
        assert!(!buf.is_null(), "getBuffer must return the asset bytes");

        let got = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
        assert_eq!(got, payload, "getBuffer returns the exact APK entry bytes");

        let missing = std::ffi::CString::new("does/not/exist").unwrap();

        let none = unsafe { eclipse_aassetmanager_open(mgr, missing.as_ptr(), 0) };
        assert!(none.is_null(), "a missing asset must open to NULL");

        unsafe { eclipse_aasset_close(asset) };

        unsafe { eclipse_aasset_close(asset) };

        assert!(unsafe { eclipse_aasset_getbuffer(asset) }.is_null());

        ndk_registry::asset_managers().remove(mgr_h).ok();
        std::fs::remove_file(&apk).ok();
    }

    #[test]
    fn aasset_open_with_stale_manager_returns_null() {
        let stale_mgr = handle_to_ptr::<c_void>(0xDEAD_BEEF_0000_0001);
        let name = std::ffi::CString::new("anything").unwrap();

        let asset = unsafe { eclipse_aassetmanager_open(stale_mgr, name.as_ptr(), 0) };
        assert!(asset.is_null(), "a stale manager handle must open to NULL");
    }

    #[test]
    fn aasset_openfiledescriptor_serves_a_real_fd_with_exact_bytes() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let s = ndk_registry::assets()
            .insert(AssetState {
                bytes: payload.clone().into_boxed_slice(),
                cursor: 0,
            })
            .expect("insert asset");
        let asset = handle_to_ptr::<c_void>(s);
        let mut start: libc::off_t = -1;
        let mut length: libc::off_t = -1;

        let fd = unsafe { eclipse_aasset_openfiledescriptor(asset, &mut start, &mut length) };
        assert!(fd >= 0, "a real fd must back the in-memory asset");
        assert_eq!(start, 0, "asset begins at offset 0 in the backing memfd");
        assert_eq!(
            length,
            payload.len() as libc::off_t,
            "length is the asset len"
        );

        let mut got = vec![0u8; payload.len()];
        let mut off = 0usize;
        while off < got.len() {
            let n = unsafe {
                libc::read(
                    fd,
                    got.as_mut_ptr().add(off) as *mut c_void,
                    got.len() - off,
                )
            };
            assert!(n > 0, "fd must read back the full asset");
            off += n as usize;
        }
        assert_eq!(
            got, payload,
            "fd contents must be byte-exact with the asset"
        );

        unsafe { libc::close(fd) };
        ndk_registry::assets().remove(s).ok();
    }

    #[test]
    fn aconfiguration_getters_return_device_values() {
        let cfg = eclipse_aconfiguration_new();
        assert!(!cfg.is_null(), "AConfiguration_new must allocate");
        let def = default_configuration();

        unsafe {
            assert_eq!(
                eclipse_aconfiguration_getscreenwidthdp(cfg),
                def.screen_width_dp
            );
            assert_eq!(
                eclipse_aconfiguration_getscreenheightdp(cfg),
                def.screen_height_dp
            );
            assert_eq!(eclipse_aconfiguration_getscreensize(cfg), def.screen_size);
            assert_eq!(eclipse_aconfiguration_getnavhidden(cfg), def.nav_hidden);

            let mut country = [0u8; 2];
            eclipse_aconfiguration_getcountry(cfg, country.as_mut_ptr().cast());
            assert_eq!(&country, &def.country);
            let mut language = [0u8; 2];
            eclipse_aconfiguration_getlanguage(cfg, language.as_mut_ptr().cast());
            assert_eq!(&language, &def.language);

            eclipse_aconfiguration_fromassetmanager(cfg, std::ptr::null_mut());
            assert_eq!(
                eclipse_aconfiguration_getscreenwidthdp(cfg),
                def.screen_width_dp
            );

            eclipse_aconfiguration_delete(cfg);
        }

        assert_eq!(unsafe { eclipse_aconfiguration_getscreenwidthdp(cfg) }, 0);
    }

    #[test]
    fn alooper_prepare_is_idempotent_per_thread_and_pollonce_returns_documented_sentinels() {
        let l1 = eclipse_alooper_prepare(0);
        assert!(!l1.is_null(), "ALooper_prepare must return a looper");
        let l2 = eclipse_alooper_prepare(0);
        assert_eq!(l1, l2, "prepare is idempotent for the calling thread");
        assert_eq!(
            eclipse_alooper_forthread(),
            l1,
            "forThread == the prepared looper"
        );

        let added = unsafe {
            eclipse_alooper_addfd(l1, 7, 1, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(added, 1, "addFd on a valid looper returns 1");

        let removed = unsafe { eclipse_alooper_removefd(l1, 7) };
        assert_eq!(removed, 1, "removeFd on a valid looper returns 1");

        let finite = unsafe {
            eclipse_alooper_pollonce(
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            finite, ALOOPER_POLL_TIMEOUT,
            "finite-timeout pollOnce with no ready source → TIMEOUT"
        );

        let stale = handle_to_ptr::<c_void>(0xCAFE_0000_0000_0001);

        let bad_add = unsafe {
            eclipse_alooper_addfd(stale, 1, 1, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(bad_add, -1, "addFd on a stale looper returns -1");

        unsafe {
            eclipse_alooper_acquire(l1);
            eclipse_alooper_release(l1);
        }
    }

    #[test]
    fn anativewindow_getters_return_real_geometry_and_stale_is_negative() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert!(!win.is_null(), "fromSurface must mint a window handle");
        let def = default_native_window();

        unsafe {
            assert_eq!(eclipse_anativewindow_getwidth(win), def.width);
            assert_eq!(eclipse_anativewindow_getheight(win), def.height);

            assert_eq!(eclipse_anativewindow_getformat(win), def.format);

            eclipse_anativewindow_acquire(win);
            eclipse_anativewindow_release(win);
        }

        let stale = handle_to_ptr::<c_void>(0xBEEF_0000_0000_0001);

        assert_eq!(unsafe { eclipse_anativewindow_getwidth(stale) }, -1);

        assert_eq!(unsafe { eclipse_anativewindow_getheight(stale) }, -1);

        assert_eq!(unsafe { eclipse_anativewindow_getformat(stale) }, -1);
    }

    #[test]
    fn anativewindow_fromsurface_reports_published_live_window_geometry() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        ndk_registry::set_engine_window_geometry(1600, 900);

        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert!(!win.is_null());

        unsafe {
            assert_eq!(eclipse_anativewindow_getwidth(win), 1600, "live width");
            assert_eq!(eclipse_anativewindow_getheight(win), 900, "live height");
        }
    }

    #[test]
    fn anativewindow_fromsurface_returns_the_real_wsi_handle_when_registered() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let fake_wsi: usize = 0x7F00_1234_5670;
        ndk_registry::register_wsi_window(fake_wsi, 1280, 720);

        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(
            win as usize, fake_wsi,
            "fromSurface must return the real WSI handle the engine passes to host eglCreateWindowSurface"
        );

        unsafe {
            assert_eq!(
                eclipse_anativewindow_getwidth(win),
                1280,
                "WSI width via the map"
            );
            assert_eq!(
                eclipse_anativewindow_getheight(win),
                720,
                "WSI height via the map"
            );

            assert_eq!(
                eclipse_anativewindow_getformat(win),
                WINDOW_FORMAT_RGBA_8888,
                "WSI format is Eclipse's RGBA_8888 surface format"
            );

            eclipse_anativewindow_acquire(win);
            eclipse_anativewindow_release(win);
        }

        ndk_registry::unregister_wsi_window(fake_wsi);
        assert_eq!(
            ndk_registry::wsi_window_geometry(fake_wsi),
            None,
            "an unregistered WSI pointer is unknown (the getters then return the NDK -1 sentinel)"
        );
    }

    #[test]
    fn resolve_egl_display_target_maps_default_display_to_winit_wayland_only() {
        let winit_wl_display: usize = 0x5000_1000;

        assert_eq!(
            resolve_egl_display_target(0, Some(winit_wl_display)),
            winit_wl_display,
            "EGL_DEFAULT_DISPLAY on Wayland remaps to the registered winit wl_display"
        );

        assert_eq!(
            resolve_egl_display_target(0, None),
            0,
            "EGL_DEFAULT_DISPLAY on X11/other passes through unchanged"
        );

        assert_eq!(
            resolve_egl_display_target(0xABCD, Some(winit_wl_display)),
            0xABCD,
            "a non-default display_id is never rewritten (Wayland)"
        );

        assert_eq!(
            resolve_egl_display_target(0xABCD, None),
            0xABCD,
            "a non-default display_id is never rewritten (X11/other)"
        );
    }

    #[test]
    fn media_ndk_natives_return_unavailable_sentinels() {
        assert!(unsafe { eclipse_amediacodec_createdecoderbytype(std::ptr::null()) }.is_null());

        assert!(unsafe { eclipse_amediacodec_createencoderbytype(std::ptr::null()) }.is_null());
        assert!(eclipse_amediaformat_new().is_null());

        assert!(unsafe { eclipse_amediacodec_getoutputformat(std::ptr::null_mut()) }.is_null());

        unsafe {
            assert_eq!(
                eclipse_amediacodec_start(std::ptr::null_mut()),
                AMEDIA_ERROR_UNSUPPORTED
            );
            assert_eq!(
                eclipse_amediacodec_stop(std::ptr::null_mut()),
                AMEDIA_ERROR_UNSUPPORTED
            );
            assert_eq!(
                eclipse_amediacodec_flush(std::ptr::null_mut()),
                AMEDIA_ERROR_UNSUPPORTED
            );
            assert_eq!(
                eclipse_amediacodec_configure(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0
                ),
                AMEDIA_ERROR_UNSUPPORTED
            );
        }

        assert_eq!(AMEDIA_ERROR_UNSUPPORTED, -10009);

        unsafe {
            assert!(eclipse_amediacodec_dequeueinputbuffer(std::ptr::null_mut(), 0) < 0);
            assert!(
                eclipse_amediacodec_dequeueoutputbuffer(
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0
                ) < 0
            );
        }

        unsafe {
            assert!(!eclipse_amediaformat_getint32(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut()
            ));
            assert!(!eclipse_amediaformat_getbuffer(
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ));
        }

        let s = unsafe { eclipse_amediaformat_tostring(std::ptr::null_mut()) };
        assert!(!s.is_null(), "toString must never return NULL");

        assert_eq!(unsafe { *s }, 0, "toString returns an empty string");
    }

    #[test]
    fn amediaformat_key_data_objects_hold_the_public_key_strings() {
        let cases = [
            ("AMEDIAFORMAT_KEY_MIME", "mime"),
            ("AMEDIAFORMAT_KEY_WIDTH", "width"),
            ("AMEDIAFORMAT_KEY_HEIGHT", "height"),
            ("AMEDIAFORMAT_KEY_BIT_RATE", "bitrate"),
            ("AMEDIAFORMAT_KEY_SAMPLE_RATE", "sample-rate"),
            ("AMEDIAFORMAT_KEY_I_FRAME_INTERVAL", "i-frame-interval"),
        ];
        let p = EclipseNativeProvider::with_bionic_natives();
        for (name, want) in cases {
            let addr = p.resolve(name).expect("key registered").addr;
            assert!(addr != 0, "{name} data symbol must be non-null");

            let strp = unsafe { *(addr as *const *const c_char) };
            assert!(!strp.is_null(), "{name} value (the char*) must be non-null");

            let got = unsafe { std::ffi::CStr::from_ptr(strp) };
            assert_eq!(got.to_str().unwrap(), want, "{name} == \"{want}\"");
        }
    }

    #[test]
    fn sl_create_engine_via_provider_produces_a_real_engine() {
        let p = EclipseNativeProvider::with_bionic_natives();
        let addr = p.resolve("slCreateEngine").expect("registered").addr;
        assert!(
            addr != 0,
            "slCreateEngine must resolve to an Eclipse address"
        );

        let create: unsafe extern "C" fn(
            *mut c_void,
            u32,
            *const c_void,
            u32,
            *const c_void,
            *const c_void,
        ) -> u32 = unsafe { std::mem::transmute::<u64, _>(addr) };
        let mut engine: *mut c_void = std::ptr::null_mut();

        let r = unsafe {
            create(
                std::ptr::addr_of_mut!(engine).cast(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(r, super::super::opensl::SL_RESULT_SUCCESS);
        assert!(!engine.is_null(), "a real engine object must be produced");

        super::super::opensl::destroy_object_for_test(engine);
    }

    #[test]
    fn sl_iid_data_objects_are_stable_distinct_nonnull_pointers() {
        let names = [
            "SL_IID_ANDROIDCONFIGURATION",
            "SL_IID_ANDROIDSIMPLEBUFFERQUEUE",
            "SL_IID_BUFFERQUEUE",
            "SL_IID_ENGINE",
            "SL_IID_PLAY",
            "SL_IID_RECORD",
            "SL_IID_VOLUME",
        ];
        let p = EclipseNativeProvider::with_bionic_natives();
        let mut iface_ptrs = std::collections::BTreeSet::new();
        for name in names {
            let addr = p.resolve(name).expect("iid registered").addr;
            assert!(addr != 0, "{name} data symbol must be non-null");

            let iid = unsafe { *(addr as *const *const SlInterfaceId) };
            assert!(
                !iid.is_null(),
                "{name} interface-id pointer must be non-null"
            );

            assert!(
                iface_ptrs.insert(iid as usize),
                "{name} must be a distinct IID"
            );
        }

        assert_eq!(
            p.resolve("SL_IID_ENGINE").unwrap().addr,
            EclipseNativeProvider::with_bionic_natives()
                .resolve("SL_IID_ENGINE")
                .unwrap()
                .addr,
            "SL_IID_ENGINE address is stable across providers"
        );
    }

    struct NativeTestPipe {
        read: std::os::fd::OwnedFd,
        write: std::fs::File,
    }
    impl NativeTestPipe {
        fn new() -> Self {
            use std::os::fd::FromRawFd;
            let mut fds = [0i32; 2];

            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert_eq!(rc, 0, "pipe2");

            let read = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };

            let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
            Self { read, write }
        }
        fn read_fd(&self) -> i32 {
            use std::os::fd::AsRawFd;
            self.read.as_raw_fd()
        }
        fn signal(&mut self) {
            use std::io::Write;
            self.write.write_all(b"x").expect("signal pipe");
        }
    }

    #[test]
    fn alooper_prepare_then_pollonce_no_source_times_out() {
        std::thread::spawn(|| {
            let looper = eclipse_alooper_prepare(0);
            assert!(!looper.is_null(), "prepare returns a real looper handle");

            assert_eq!(eclipse_alooper_forthread(), looper, "forThread == prepared");

            let rc = unsafe {
                eclipse_alooper_pollonce(
                    10,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ALOOPER_POLL_TIMEOUT);
        })
        .join()
        .expect("looper thread");
    }

    #[test]
    fn alooper_pollonce_returns_ident_when_registered_fd_fires() {
        let mut pipe = NativeTestPipe::new();
        let fd = pipe.read_fd();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<i32>();
        let h = std::thread::spawn(move || {
            let looper = eclipse_alooper_prepare(0);
            assert!(!looper.is_null());
            const IDENT: c_int = 42;

            let added = unsafe {
                eclipse_alooper_addfd(
                    looper,
                    fd,
                    IDENT,
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(added, 1, "addFd succeeds");
            ready_tx.send(()).expect("signal ready");
            let mut out_fd: c_int = -1;
            let mut out_events: c_int = -1;

            let rc = unsafe {
                eclipse_alooper_pollonce(1000, &mut out_fd, &mut out_events, std::ptr::null_mut())
            };
            assert_eq!(rc, IDENT, "pollOnce returns the registered ident");
            assert_eq!(out_fd, fd, "out_fd is the fd that fired");
            assert!(out_events & 1 != 0, "out_events reports POLLIN");
            done_tx.send(rc).expect("signal done");
        });
        ready_rx.recv().expect("thread registered the fd");

        pipe.signal();
        let rc = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("pollOnce woke");
        assert_eq!(rc, 42);
        h.join().expect("looper thread");
    }

    #[test]
    fn winit_feed_wakes_a_parked_native_pollonce() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<i32>();
        let h = std::thread::spawn(move || {
            let looper = eclipse_alooper_prepare(0);
            assert!(!looper.is_null());
            ready_tx.send(()).expect("ready");

            let rc = unsafe {
                eclipse_alooper_pollonce(
                    -1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            done_tx.send(rc).expect("done");
        });
        ready_rx.recv().expect("thread prepared its looper");
        std::thread::sleep(std::time::Duration::from_millis(50));
        let woken = ndk_registry::wake_all_loopers();
        assert!(
            woken >= 1,
            "at least the parked looper was registered + woken"
        );
        let rc = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("pollOnce woke");
        assert_eq!(
            rc, ALOOPER_POLL_WAKE,
            "the winit feed woke the parked pollOnce"
        );
        h.join().expect("looper thread");
    }

    #[test]
    fn alooper_addfd_rejects_callback_and_negative_ident() {
        std::thread::spawn(|| {
            let looper = eclipse_alooper_prepare(0);

            let mut sentinel: u8 = 0;
            let cb = std::ptr::addr_of_mut!(sentinel).cast::<c_void>();

            let r1 = unsafe { eclipse_alooper_addfd(looper, 3, 1, 1, cb, std::ptr::null_mut()) };
            assert_eq!(r1, -1, "callback form rejected");

            let r2 = unsafe {
                eclipse_alooper_addfd(looper, 3, -1, 1, std::ptr::null_mut(), std::ptr::null_mut())
            };
            assert_eq!(r2, -1, "negative ident rejected");
        })
        .join()
        .expect("looper thread");
    }

    #[test]
    fn alooper_pollonce_without_prepare_is_error_not_panic() {
        std::thread::spawn(|| {
            let rc = unsafe {
                eclipse_alooper_pollonce(
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(rc, ALOOPER_POLL_ERROR);
        })
        .join()
        .expect("thread");
    }

    #[test]
    fn alooper_addfd_removefd_on_stale_handle_return_minus_one() {
        let fabricated = handle_to_ptr::<c_void>(0xDEAD_0000_0001);

        let add = unsafe {
            eclipse_alooper_addfd(
                fabricated,
                5,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(add, -1, "addFd on a fabricated handle is -1");

        let rem = unsafe { eclipse_alooper_removefd(fabricated, 5) };
        assert_eq!(rem, -1, "removeFd on a fabricated handle is -1");
    }

    #[test]
    fn every_host_input_kind_wakes_the_loopers() {
        for kind in [
            HostInputKind::Pointer,
            HostInputKind::MouseButton,
            HostInputKind::Scroll,
            HostInputKind::Touch,
            HostInputKind::Key,
        ] {
            assert!(
                host_input_should_wake(Some(kind)),
                "{kind:?} (a Roblox player input) must wake the engine input loop"
            );
        }
    }

    #[test]
    fn non_input_events_do_not_wake() {
        assert!(
            !host_input_should_wake(None),
            "non-input events must not wake the engine input loop"
        );
    }

    #[test]
    fn full_input_path_run_input_test_succeeds() {
        match run_input_test() {
            Ok(report) => assert!(report.contains("ALOOPER_POLL_WAKE"), "report: {report}"),
            Err(e) => panic!("run_input_test failed: {e}"),
        }
    }
}
