//! Eclipse-owned **bionic-ABI-correct native provider** — the tier that beats the host baseline.
//!
//! 2026-06-05: [`bionic_env`](super::bionic_env) resolved 490/584 of `libroblox.so`'s UND imports
//! against a **host glibc / host-GL baseline** (relocation lands, but calling a glibc routine with
//! bionic-shaped arguments is **not** correct — see that module's honest caveat) and enumerated an
//! 88-import Eclipse-bionic-native work-list. This module supplies the first **Eclipse-owned**
//! provider tier: an [`EclipseNativeProvider`] — a registry mapping a bionic C-ABI symbol *name* to
//! the address of an Eclipse-owned `extern "C"` Rust function (or data object). It is **prepended**
//! before the host tier in the [`super::bionic_env::BionicEnv`] scope, so Eclipse's own
//! implementations win over the host-glibc baseline for the names it registers (the System V gABI
//! first-match rule applied by [`super::resolve::Scope`]).
//!
//! ## Clean-room provenance
//! Every native here is implemented from the **public** bionic / NDK C-ABI symbol contract — the
//! documented signatures and semantics of `__android_log_write`, `__errno`, `__system_property_get`,
//! the `_FORTIFY` `_chk` family, `__stack_chk_guard`, `__sF`, etc. (general knowledge of these
//! public APIs) — plus Eclipse's own `src/` (the [`tracing`] log sink, the loader cores). **No**
//! bionic / NDK / dynamic-linker *source* was read. `libroblox.so` is parsed as data only; nothing
//! in it is executed.
//!
//! ## Forward / minimal-correct / documented-stub — the honesty rule (AGENTS.md core principle)
//! Each registered symbol is labelled at its definition:
//! - **forward** — bionic and glibc share an *identical* C ABI for this routine, so the native
//!   forwards to the glibc equivalent (honoring/ignoring the bionic `_chk` bound per the public
//!   `_FORTIFY` contract). The behavior is real and correct.
//! - **minimal-correct** — glibc has no equivalent, so the native is a correct minimal Eclipse-owned
//!   implementation (e.g. an Eclipse property store that returns 0/empty with the value written
//!   safely; an Eclipse SSP guard value). Callers that depend on a return value get a correct one.
//! - **documented-stub** — there are deliberately **none** in the registered set.
//!
//! ## Deferred: the two VARIADIC liblog natives (honest, dated 2026-06-05)
//! `__android_log_print(int, const char*, const char*, ...)` and
//! `__android_log_assert(const char*, const char*, const char*, ...)` are **C-variadic**. Defining a
//! variadic `extern "C"` function requires Rust's unstable `c_variadic` feature (nightly only);
//! Eclipse builds on **stable** Rust (no `rust-toolchain.toml`, must build from a clean checkout per
//! AGENTS.md §2.11 portability). A *non-variadic* Rust fn registered under a variadic symbol would be
//! an **ABI landmine** (the caller passes varargs the callee cannot pop), so per the task rule —
//! "leave it on the work-list, do not register a landmine" — these **two** stay on the work-list.
//! The other **3** liblog natives (`__android_log_write`/`__android_log_buf_write`/
//! `android_set_abort_message`) are fixed-arity and implemented here; `__assert2` (fixed 4-arg) is
//! NOT variadic and is implemented. When Eclipse adopts a nightly toolchain (or a tiny clean-room C
//! shim is justified), the two variadic natives route to the same [`emit_log`] sink.
//!
//! ## Safety
//! Taking the address of an Eclipse `extern "C"` fn/data symbol (`f as usize`) is **safe** Rust; the
//! provider/registry itself needs **no** `unsafe`. The `unsafe` is confined to the native *bodies*
//! that cross the C ABI (raw-pointer args, forwarding to glibc), each with a dated `// SAFETY:` note.
//! [`super::reloc`]/[`super::elf`]/[`super::resolve`] stay `#![forbid(unsafe_code)]`.
//!
//! ## ndk-android (libandroid) tier — the 28 NDK natives (added 2026-06-05)
//! The second Eclipse-native category: the 27 `libandroid` C-ABI imports from
//! `docs/bionic-env-worklist.md` (libroblox's own set), plus `ANativeWindow_getFormat`
//! (2026-06-12 — `libsurface_util_jni.so`'s sole unresolved pre-load import). Each is labelled at
//! its definition:
//! - **AAsset / AAssetManager (6) — real:** route to Eclipse's own [`crate::apk`] reader.
//!   `AAssetManager_open` reads the named `assets/<name>` zip entry's real bytes; `AAsset_getBuffer`
//!   /`AAsset_getLength` hand them back; `AAsset_close` frees the owned-handle slot. Handles are
//!   Eclipse-owned generational [`super::ndk_registry`] indices cast to the opaque NDK pointers, so a
//!   stale/fabricated `AAsset*`/`AAssetManager*` is a typed `Err` → NDK sentinel, never UB.
//! - **AConfiguration (9) — minimal-correct:** an Eclipse `AConfiguration` holding sane device values
//!   (mdpi/160, the window geometry in dp, portrait); the getters read them back.
//! - **ALooper (7) — minimal-correct:** a small Eclipse per-thread looper (an fd registry); `pollOnce`
//!   returns the documented `ALOOPER_POLL_*` sentinel a caller must handle (NOT a fake-success
//!   landmine).
//! - **ANativeWindow (6) — sound-stub:** the getters return the real window geometry/format; the
//!   surface/buffer bits whose real behavior is the upcoming GLES2/EGL render integration return
//!   documented sound sentinels (valid-but-empty handle / negative error per the NDK contract) so
//!   resolution + early init proceed WITHOUT pretending a frame was presented. Deferred-to-render.
//!
//! ## media-ndk (libmediandk, 33) — sound-stubs (added 2026-06-05) + audio (OpenSL ES, 8) — REAL
//! - **media-ndk (33) — sound-stub: media playback deferred (gameplay-time):** video playback is NOT
//!   needed to start/render, so each native returns its public-ABI failure/unavailable sentinel so a
//!   caller cleanly detects "no media" and never acts on a fabricated success. `AMediaCodec_*` /
//!   `AMediaFormat_*` pointer-returning fns → `NULL`; [`media_status_t`](MEDIA_STATUS)-returning fns
//!   → `AMEDIA_ERROR_UNSUPPORTED`; the `ssize_t` dequeue fns → that error (negative); `bool` getters
//!   → `false`; `delete`/setters → safe no-ops; `AMediaFormat_toString` → a stable empty C string.
//!   The 10 `AMEDIAFORMAT_KEY_*` are real `const char*` data objects holding the documented public
//!   key strings (minimal-correct data, not a stub).
//! - **audio (8) — REAL OpenSL ES → host audio (2026-06-05):** `slCreateEngine` is implemented by
//!   [`super::opensl`] — it returns a **working** `SLObjectItf` whose vtables drive
//!   `Realize`/`GetInterface`/`CreateOutputMix`/`CreateAudioPlayer`/`SetPlayState` and whose
//!   `SLAndroidSimpleBufferQueueItf::Enqueue` feeds a **cpal** host output stream (real PCM → real
//!   sound). The 7 `SL_IID_*` stay real, stable, distinct `SLInterfaceID` data objects — now
//!   **consumed** by `GetInterface` (matched via [`sl_iid_index`]). On a host with no audio device the
//!   engine still constructs and accepts Enqueues (no sound) — a clean "no device" posture, never a
//!   fake. Only `slCreateEngine` + the 7 IIDs are imported by libroblox (everything else flows through
//!   the vtables), so no other audio symbol is registered (no dead natives, §2.5).
//!
//! ## What this is NOT (honest scope, dated 2026-06-05)
//! Registering a correct address makes the relocation land *and* (for the forward/minimal/real
//! natives) makes a **call** to that symbol behave per its public contract. It does **not** by itself
//! make `libroblox.so` runnable — that needs binding the image to execution and running the
//! `DT_INIT_ARRAY` constructors
//! (the runtime tail, main-loop / dev-host only). The ANativeWindow surface/buffer natives are
//! explicitly **deferred to the render integration** (documented sound sentinels until then).

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

// =================================================================================================
// The provider: a name -> Eclipse-owned address registry, prepended before the host tier.
// =================================================================================================

/// An Eclipse-owned [`SymbolProvider`]: a registry mapping a bionic C-ABI symbol **name** to the
/// run-time address of an Eclipse-owned `extern "C"` function or data object. [`Self::resolve`]
/// returns the registered address as a **strong** definition (these are real, exported Eclipse
/// definitions), or `None` for an unregistered name (so the scope falls through to the host tier).
///
/// Built with [`EclipseNativeProvider::with_bionic_natives`], which registers the liblog (3
/// fixed-arity), bionic-specific libc (15), bionic stdio FILE\*-translation (25, 2026-06-12), and
/// bionic signal-ABI (6, 2026-06-11) natives implemented in this module. Prepended before the host
/// baseline in [`super::bionic_env::BionicEnv`] so Eclipse's bionic-correct impls win.
pub struct EclipseNativeProvider {
    /// name → run-time address of the Eclipse-owned `extern "C"` symbol.
    natives: HashMap<&'static str, u64>,
}

impl EclipseNativeProvider {
    /// An empty provider (registers nothing; resolves nothing). Useful for tests and composing a
    /// custom registration set.
    pub fn empty() -> Self {
        Self {
            natives: HashMap::new(),
        }
    }

    /// Register a single bionic C-ABI `name` → the address of an Eclipse-owned symbol. `addr` is a
    /// function-item or data address obtained safely (`some_extern_c_fn as usize as u64`).
    pub fn register(&mut self, name: &'static str, addr: u64) -> &mut Self {
        self.natives.insert(name, addr);
        self
    }

    /// Number of registered natives (for reporting / tests).
    pub fn len(&self) -> usize {
        self.natives.len()
    }

    /// Whether the provider has no registered natives.
    pub fn is_empty(&self) -> bool {
        self.natives.is_empty()
    }

    /// Build the provider with the **fixed-arity liblog (3)** + **bionic-specific libc (16)** +
    /// **bionic stdio FILE\* translation (25)** + **bionic signal-ABI (6)** + **ndk-android
    /// libandroid (28)** natives this module implements registered. Taking each native's address is
    /// safe Rust (a function/data item coerced to a pointer then to `u64`).
    ///
    /// The names are the real work-list from `loader::link::tests::real_libroblox_bionic_env_*`
    /// (`docs/bionic-env-worklist.md`): liblog's full 6 (the 3 fixed-arity Rust natives plus the 2
    /// **variadic** ones — `__android_log_print`/`__android_log_assert` — DEFINED by the
    /// clean-room C shim, 2026-06-05, plus the `va_list` `__android_log_vprint`, 2026-06-12 —
    /// `libbacktrace-native.so`'s pre-load needs it); bionic-libc's 16 (`__umask_chk` added
    /// 2026-06-12, the other libbacktrace-native unresolved import); the stdio FILE\* translation 25
    /// (2026-06-12 — bionic `&__sF[i]` stream sentinels remapped to host glibc streams; see the
    /// `__sF` section); the signal-ABI 7 (6 translating, 2026-06-11 — these resolved to host glibc
    /// before, whose sigset_t/sigaction LAYOUT is incompatible; + the sigaltstack attribution
    /// forward, 2026-06-12); link-map introspection's 2 (dl_iterate_phdr/dladdr, 2026-06-12); the
    /// netdb resolver-ABI 4 (getaddrinfo/freeaddrinfo/gai_strerror/getnameinfo, 2026-06-12 — the
    /// engine DnsResolve root cause; see the netdb section); EGL display interception's 1
    /// (eglGetDisplay, 2026-06-13 — the EGL_BAD_ALLOC 3003 connection-match); Vulkan WSI
    /// interception's 3 (vkGetInstanceProcAddr/vkCreateInstance/vkCreateAndroidSurfaceKHR, 2026-06-13 —
    /// the Android→Wayland Vulkan WSI translation; see [`super::vulkan_wsi`]); ndk-android's 28
    /// (AAsset* real via `src/apk`, AConfiguration/ALooper minimal-correct, ANativeWindow
    /// sound-stub — `ANativeWindow_getFormat` added 2026-06-12 for `libsurface_util_jni.so`'s
    /// pre-load); media-ndk's 33 sound-stubs and audio's 8 (REAL OpenSL ES → host audio via
    /// [`super::opensl`]). **134** base symbols (the pthread + sysconf groups register on top) —
    /// includes the tier-0 `dlsym` interposer that hands the engine the Vulkan WSI shims it `dlsym`s
    /// from its runtime-`dlopen`ed libvulkan (see [`super::vulkan_wsi::eclipse_dlsym`]).
    pub fn with_bionic_natives() -> Self {
        let mut p = Self::empty();

        // ---- liblog — the 3 FIXED-ARITY natives (the 2 variadic stay on the work-list) ----------
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
        // The 2 VARIADIC liblog natives — DEFINED by the clean-room C shim (src/loader/liblog_shim.c,
        // compiled by build.rs via the `cc` crate) because Rust stable cannot define a C-variadic
        // `extern "C"` fn. Rust DECLARES them as variadic externs (stable) and takes their addresses
        // here; the shim forwards the formatted line to `eclipse_liblog_emit` → `emit_log`. 2026-06-05.
        p.register(
            "__android_log_print",
            __android_log_print as *const () as u64,
        );
        p.register(
            "__android_log_assert",
            __android_log_assert as *const () as u64,
        );
        // The va_list liblog native — also DEFINED by the C shim (no stable Rust spelling for
        // va_list). 2026-06-12: libbacktrace-native.so imports it; without it the pre-load failed
        // and System.loadLibrary("backtrace-native") fell through to the apkenv shim linker
        // (fatal NULL _r_debug_ptr write — core 866509).
        p.register(
            "__android_log_vprint",
            __android_log_vprint as *const () as u64,
        );

        // ---- bionic-specific libc (16) — glibc lacks these exact names --------------------------
        // FORTIFY `_chk` family — forward to the plain glibc routine (ABI-identical), honoring the
        // bound per the public `_FORTIFY_SOURCE` contract (abort if the access would overflow).
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
        // bionic internal / assert / strerror / property entry points.
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
        // bionic data OBJECTs (not functions): the SSP guard word and the stdio FILE table.
        p.register("__stack_chk_guard", eclipse_stack_chk_guard_addr());
        p.register("__sF", eclipse_sf_addr());

        // ---- bionic stdio FILE* translation (25) — &__sF[i] sentinels → host glibc streams ------
        // 2026-06-12: bionic's public ABI computes stdin/stdout/stderr as `&__sF[i]` (an
        // array-of-STRUCTS interior address, stride 152 on LP64) — those sentinel addresses are
        // NOT glibc FILE*s, so every FILE*-consuming stdio import of the five __sF-importing
        // engine libs (readelf enumeration — see the `__sF` section) is intercepted to remap the
        // three sentinels and forward to glibc. Root cause: core 782252 — crashpad's
        // `fputs(msg, &__sF[2])` handed glibc a pointer 280 bytes past Eclipse's old 24-byte
        // pointer table and the crash handler's own logging SIGSEGV'd.
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
        // The 2 VARIADIC + 1 va_list stdio natives — DEFINED by the clean-room C shim
        // (src/loader/stdio_shim.c), the liblog_shim.c pattern. 2026-06-12.
        p.register("fprintf", eclipse_fprintf as *const () as u64);
        p.register("fscanf", eclipse_fscanf as *const () as u64);
        p.register("vfprintf", eclipse_vfprintf as *const () as u64);

        // ---- bionic signal ABI (6) — glibc HAS these names but an incompatible layout -----------
        // 2026-06-11: bionic sigset_t = 8 bytes vs glibc's 128; bionic sigaction = flags@0/
        // handler@8/mask@16 vs glibc handler@0/mask@8(128B)/flags@136. Falling through to glibc
        // scrambled crashpad's SIGSEGV handler registration (core dump 455287 — the kernel-invoked
        // handler was a bionic sa_flags value read as a pointer) and glibc's sigfillset/
        // *_sigmask(oldset) write 128 bytes through 8-byte bionic sets. See the signal-ABI section.
        p.register("sigaction", eclipse_sigaction as *const () as u64);
        p.register("sigemptyset", eclipse_sigemptyset as *const () as u64);
        p.register("sigaddset", eclipse_sigaddset as *const () as u64);
        p.register("sigfillset", eclipse_sigfillset as *const () as u64);
        p.register("sigprocmask", eclipse_sigprocmask as *const () as u64);
        p.register(
            "pthread_sigmask",
            eclipse_pthread_sigmask as *const () as u64,
        );
        // sigaltstack (the 7th signal native) — bionic/glibc `stack_t` ARE layout-identical on
        // x86-64, so this is a PURE forward; it exists for OBSERVABILITY (2026-06-12,
        // core 1223806): the kernel force_sigsegv()'d writing a signal frame to a
        // registered-but-unwritable SA_ONSTACK altstack, and Eclipse had ZERO attribution for
        // engine altstack registrations. The C shim captures the caller's return address; the
        // Rust callee forwards, then logs+records tid/ss_sp/ss_size/ss_flags + caller module.
        p.register("sigaltstack", eclipse_sigaltstack as *const () as u64);

        // ---- bionic link-map introspection (2) — dl_iterate_phdr + dladdr ----------------------
        // 2026-06-12 (core 1223806): libroblox's statically-linked libc++abi unwinder resolves
        // FDEs via `dl_iterate_phdr@LIBC`; falling through to HOST glibc walked only glibc's own
        // link map (the Eclipse-mapped engine images are invisible to it), so the boot's first
        // C++ throw found no FDE → std::terminate re-raise loop, 61,497 iterations / 12.2 MB of
        // stack. `dladdr` is the same-class companion (engine backtrace symbolization). Both walk
        // Eclipse's `module_registry` first, then delegate to host glibc for host modules.
        p.register(
            "dl_iterate_phdr",
            super::module_registry::eclipse_dl_iterate_phdr as *const () as u64,
        );
        p.register(
            "dladdr",
            super::module_registry::eclipse_dladdr as *const () as u64,
        );

        // ---- bionic netdb resolver ABI (4) — addrinfo tail order + AI_/EAI_/NI_ values diverge --
        // 2026-06-12 (the engine HttpError:DnsResolve root cause — see the netdb section): the
        // host-glibc fall-through handed bionic walkers glibc-shaped addrinfo nodes (canonname/
        // addr tail SWAPPED) → zero usable addresses on every engine curl lookup. gethostbyname
        // stays host-baseline (hostent field order identical — record-only).
        p.register("getaddrinfo", eclipse_getaddrinfo as *const () as u64);
        p.register("freeaddrinfo", eclipse_freeaddrinfo as *const () as u64);
        p.register("gai_strerror", eclipse_gai_strerror as *const () as u64);
        p.register("getnameinfo", eclipse_getnameinfo as *const () as u64);

        // ---- ndk-android (libandroid) — the 28 NDK natives -------------------------------------
        // AAsset / AAssetManager (6) — REAL, routed to Eclipse's own `src/apk` reader.
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
        // AConfiguration (9) — MINIMAL-CORRECT, real getters over Eclipse device values.
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
        // ALooper (7) — MINIMAL-CORRECT Eclipse per-thread looper; pollOnce returns ALOOPER_POLL_*.
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
        // EGL display interception (1) — 2026-06-13: tier-0 `eglGetDisplay` that connection-matches the
        // engine's EGLDisplay to Eclipse's winit `wl_display` (remaps EGL_DEFAULT_DISPLAY on Wayland,
        // pass-through otherwise/X11), fixing the engine's `eglCreateWindowSurface` EGL_BAD_ALLOC 3003
        // cross-connection. Wins over host libEGL by `resolve`'s first-strong-match (tier 0 before
        // tier 1). See `eclipse_egl_get_display`.
        p.register("eglGetDisplay", eclipse_egl_get_display as *const () as u64);

        // Vulkan WSI interception (3) — 2026-06-13: tier-0 `vk*` shims that translate the engine's
        // Android Vulkan WSI to the host Linux Wayland WSI. The engine (API 28 ⇒ Mode 6) requests the
        // Android-only `VK_KHR_android_surface` instance extension + `vkCreateAndroidSurfaceKHR`, absent
        // from the host ICD → `Mode 6 failed: Unable to create Vulkan instance`. `vkCreateInstance` swaps
        // `VK_KHR_android_surface`→`VK_KHR_wayland_surface`; `vkCreateAndroidSurfaceKHR` builds the surface
        // on Eclipse's winit `wl_display`+`wl_surface` via the host `vkCreateWaylandSurfaceKHR`;
        // `vkGetInstanceProcAddr` routes the two shims by name (proc-addr path) and forwards the rest. Win
        // over host libvulkan by `resolve`'s first-strong-match (tier 0 before tier 1), exactly like
        // `eglGetDisplay`. See `super::vulkan_wsi`.
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
        // 2026-06-13: the engine `dlopen`s libvulkan at runtime and `dlsym`s the Vulkan loader commands by
        // name (they are NOT UND imports, so the three `vk*` registrations above are never consulted for
        // it). `dlsym` IS a UND import the engine resolves through Eclipse's scope, so a tier-0 `dlsym`
        // interposer hands back the WSI-translating shims for the loader entry points and forwards every
        // other symbol unchanged to the host `dlsym`. See `super::vulkan_wsi::eclipse_dlsym`.
        p.register(
            "dlsym",
            super::vulkan_wsi::eclipse_dlsym as *const () as u64,
        );

        // ANativeWindow (6) — WSI-bound: fromSurface returns the REAL host-EGL native window Eclipse
        // owns, getters return real geometry/format, refcount ops are no-ops (the engine render WSI
        // bind). getFormat added 2026-06-12 — libsurface_util_jni.so's sole unresolved pre-load
        // import (the apkenv-delegation class, core 866509).
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

        // ---- media-ndk (libmediandk) — the 33 NDK media natives (sound-stub: gameplay-time) -------
        // AMediaCodec (14) — sound-stub: pointer fns → NULL, media_status_t → AMEDIA_ERROR_UNSUPPORTED,
        // ssize_t dequeue → negative AMEDIA_ERROR_UNSUPPORTED, delete → no-op.
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
        // AMediaFormat (9) — sound-stub: new → NULL, getters → false, setters/delete → no-op,
        // toString → stable empty string.
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
        // AMEDIAFORMAT_KEY_* (10) — DATA objects: `const char*` holding the documented key string.
        // These public key constants are real values (minimal-correct data, not a stub) — a caller
        // that reads/passes them gets the canonical MediaFormat key string.
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

        // ---- audio (OpenSL ES) — the 8 audio natives (REAL OpenSL ES → host audio) ---------------
        // 2026-06-05: slCreateEngine now returns a WORKING Eclipse-owned `SLObjectItf` engine
        // (`super::opensl`): Realize/GetInterface yields a real SLEngineItf; CreateOutputMix +
        // CreateAudioPlayer (AndroidSimpleBufferQueue source + PCM format → output-mix sink) build a
        // player whose SLAndroidSimpleBufferQueueItf::Enqueue feeds a cpal host output stream. On a
        // host with no audio device the engine still constructs (Enqueues accepted, no sound) — a
        // clean "no device" posture, never a fake. See `src/loader/opensl.rs`.
        p.register(
            "slCreateEngine",
            super::opensl::eclipse_sl_create_engine as *const () as u64,
        );
        // SL_IID_* (7) — DATA objects of type `SLInterfaceID` (a pointer to a 128-bit interface UUID
        // struct). Each resolves to a stable, valid, distinct Eclipse-owned `SLInterfaceID_` object;
        // `GetInterface` matches the engine's requested interface by these pointers (see
        // `sl_iid_index`).
        p.register("SL_IID_ANDROIDCONFIGURATION", sl_iid_addr(0));
        p.register("SL_IID_ANDROIDSIMPLEBUFFERQUEUE", sl_iid_addr(1));
        p.register("SL_IID_BUFFERQUEUE", sl_iid_addr(2));
        p.register("SL_IID_ENGINE", sl_iid_addr(3));
        p.register("SL_IID_PLAY", sl_iid_addr(4));
        p.register("SL_IID_RECORD", sl_iid_addr(5));
        p.register("SL_IID_VOLUME", sl_iid_addr(6));

        // ---- bionic pthread + TLS + sem + syscall shim (the threading runtime) ------------------
        // 2026-06-05: the engine's `pthread_*` / `sem_*` / `gettid` / `syscall` imports previously
        // resolved to the HOST glibc baseline, whose pthread/key/once LAYOUTS differ from bionic's —
        // which aborted `init[1]` (a libc++/protobuf static-init guard misread its bionic-layout
        // per-thread state; see docs/libroblox-init-run.md). The Eclipse-owned bionic-ABI shim
        // (`super::bionic_pthread`) operates on the BIONIC memory layouts; prepended before host, it
        // displaces glibc so those objects are interpreted correctly. See that module for the layout
        // encoding + the futex/gettid primitives.
        super::bionic_pthread::register_natives(|name, addr| {
            p.register(name, addr);
        });

        // ---- bionic system-query natives (the allocator-bootstrap fix) --------------------------
        // 2026-06-05: the engine's `sysconf` / `getauxval` / `sched_getcpu` / `getpagesize` /
        // `sysinfo` imports previously resolved to the HOST glibc baseline, whose `sysconf`
        // mis-answers the BIONIC `_SC_*` constant numbering (bionic `_SC_NPROCESSORS_ONLN` = 97,
        // which glibc's `sysconf(97)` answers as **-1**; bionic `_SC_PAGESIZE` = 39 → glibc 1000).
        // libroblox's own per-thread allocator sized its arena table from those bad values and its
        // first central refill returned NULL → `init[1]` `abort()` (docs/libroblox-init-run.md §7).
        // The Eclipse-owned bionic-correct natives (`super::bionic_sysconf`), prepended before host,
        // answer with the bionic constant meaning. See that module for the constant mapping + trace.
        super::bionic_sysconf::register_natives(|name, addr| {
            p.register(name, addr);
        });

        p
    }
}

impl SymbolProvider for EclipseNativeProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        // Eclipse-owned natives are real, exported, strong definitions.
        self.natives
            .get(name)
            .map(|&addr| ResolvedSym { addr, weak: false })
    }
}

/// Resolve `ANativeWindow_fromSurface` through the Eclipse native provider (exactly as the engine's
/// relocation would) and call it with `(env=null, surface=null)`, returning the `ANativeWindow*` the
/// engine would receive. This is the **engine-style** entry the `eclipse __gl-test-anw` validation
/// uses to obtain its `ANativeWindow*` — going through the bound native, not a direct internal call —
/// before driving host `eglCreateWindowSurface` over it. Returns `None` if the name is not bound.
///
/// # Safety
/// The returned pointer is an `ANativeWindow*` Eclipse owns (the real WSI handle when a window exists,
/// else a sound geometry-only slab handle); the caller passes it to host EGL / the geometry getters.
#[must_use]
pub fn anativewindow_from_surface_via_provider() -> Option<*mut c_void> {
    let provider = EclipseNativeProvider::with_bionic_natives();
    let addr = provider.resolve("ANativeWindow_fromSurface")?.addr;
    // SAFETY: `addr` is the address `with_bionic_natives` registered for
    // `eclipse_anativewindow_fromsurface` (an `unsafe extern "C" fn(*mut c_void, *mut c_void) ->
    // *mut c_void`), transmuted to that exact signature. The native ignores both args, so null is a
    // valid call. This mirrors the engine resolving + calling the bound native. 2026-06-05.
    let func: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute::<u64, _>(addr) };
    // SAFETY: see above — the native accepts any (env, surface) and does not dereference them.
    Some(unsafe { func(std::ptr::null_mut(), std::ptr::null_mut()) })
}

// =================================================================================================
// liblog — route to Eclipse's existing `tracing` log sink (the diagnostics module).
// =================================================================================================
//
// 2026-06-05: bionic's liblog has no glibc equivalent; Eclipse OWNS logging via `tracing`
// (src/diagnostics.rs) — the same sink `framework.rs`'s `Log.println_native` JNI native uses. These
// C-ABI natives are the loader-visible (`dlsym`-shaped) entry points the engine's relocations bind
// to. Each emits the message (real behavior, not a no-op) at the priority-mapped level and returns
// the public-contract value.

/// bionic `android_LogPriority` values (public `<android/log.h>`): the `priority` arg meaning. Mapped
/// to a `tracing` level; an unknown value falls through to INFO (bionic does not validate priority).
const ANDROID_LOG_VERBOSE: c_int = 2;
const ANDROID_LOG_DEBUG: c_int = 3;
const ANDROID_LOG_INFO: c_int = 4;
const ANDROID_LOG_WARN: c_int = 5;
const ANDROID_LOG_ERROR: c_int = 6;
const ANDROID_LOG_FATAL: c_int = 7;

/// Emit a `[tag] msg` line to Eclipse's `tracing` sink at the priority-mapped level (the host
/// equivalent of bionic writing to the log buffer). `tag`/`msg` are already owned Rust strings.
fn emit_log(priority: c_int, tag: &str, msg: &str) {
    // 2026-06-05: in tests, route through the per-thread capture (if armed) so the variadic-shim
    // unit test can observe the exact message the C shim formatted, without changing the production
    // path. Production builds compile only the `tracing` arm below.
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

/// Read a possibly-null C string into an owned `String` (lossy UTF-8). A null pointer → `None`.
///
/// # Safety
/// `p` must be either null or a valid pointer to a NUL-terminated C string that stays valid for the
/// duration of the call.
unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: 2026-06-05 — caller guarantees `p` is a valid NUL-terminated C string (the liblog ABI
    // passes C string literals / heap buffers). `CStr::from_ptr` reads up to the NUL; we copy to an
    // owned `String` immediately so no borrow outlives the pointer.
    let s = unsafe { std::ffi::CStr::from_ptr(p) };
    Some(s.to_string_lossy().into_owned())
}

/// `int __android_log_write(int prio, const char* tag, const char* text)` — emit `text` under `tag`.
/// Returns > 0 on success (bionic returns the liblog write result, > 0). **minimal-correct.**
///
/// # Safety
/// `tag`/`text` must be null or valid NUL-terminated C strings (the public liblog ABI).
unsafe extern "C" fn eclipse_android_log_write(
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    // SAFETY: 2026-06-05 — `tag`/`text` are the liblog ABI's C-string args; `cstr_opt`'s contract
    // (null-or-valid NUL-terminated) is exactly the liblog caller contract.
    let tag = unsafe { cstr_opt(tag) }.unwrap_or_default();
    let text = unsafe { cstr_opt(text) }.unwrap_or_default();
    let n = text.len();
    emit_log(prio, &tag, &text);
    // bionic returns the number of bytes written (> 0) on success; report the message length (≥ 1).
    c_int::try_from(n).unwrap_or(c_int::MAX).max(1)
}

/// `int __android_log_buf_write(int bufID, int prio, const char* tag, const char* text)` — like
/// [`eclipse_android_log_write`] but with an explicit log-buffer id, which Eclipse's single sink
/// ignores (it has no separate Android log buffers). **minimal-correct.**
///
/// # Safety
/// `tag`/`text` must be null or valid NUL-terminated C strings.
unsafe extern "C" fn eclipse_android_log_buf_write(
    _buf_id: c_int,
    prio: c_int,
    tag: *const c_char,
    text: *const c_char,
) -> c_int {
    // SAFETY: 2026-06-05 — `tag`/`text` are the liblog C-string args this fn received under the same
    // null-or-valid-NUL-terminated contract; forwarding them to `eclipse_android_log_write` is sound.
    unsafe { eclipse_android_log_write(prio, tag, text) }
}

/// `void android_set_abort_message(const char* msg)` — record the message bionic would attach to a
/// subsequent `abort()` (visible in a tombstone/crash dump). **minimal-correct:** Eclipse has no
/// tombstone mechanism, so it emits the message to the log sink at ERROR (the closest real,
/// observable behavior) and returns. No caller depends on a return value (the function is `void`).
///
/// # Safety
/// `msg` must be null or a valid NUL-terminated C string.
unsafe extern "C" fn eclipse_android_set_abort_message(msg: *const c_char) {
    // SAFETY: 2026-06-05 — `msg` is the public C-string arg (null-or-valid NUL-terminated).
    let msg = unsafe { cstr_opt(msg) }.unwrap_or_default();
    emit_log(ANDROID_LOG_ERROR, "abort", &msg);
}

// =================================================================================================
// liblog — the 2 VARIADIC + 1 va_list natives: clean-room C shim → Eclipse sink (2026-06-05;
// `__android_log_vprint` added 2026-06-12).
// =================================================================================================
//
// `__android_log_print` / `__android_log_assert` are C-variadic; Rust stable cannot DEFINE a
// variadic `extern "C"` fn (`c_variadic` is nightly-only) but it CAN declare one and take its
// address. `__android_log_vprint` takes a `va_list` (no stable Rust spelling — the
// `eclipse_vfprintf` precedent). The definitions live in the clean-room C shim
// `src/loader/liblog_shim.c` (compiled by build.rs via the `cc` crate); each formats its
// varargs/va_list with `vsnprintf` into a bounded stack buffer and forwards the finished line to
// the Eclipse-owned non-variadic sink below.

extern "C" {
    /// `int __android_log_print(int prio, const char* tag, const char* fmt, ...)` — DEFINED in the
    /// C shim (`src/loader/liblog_shim.c`). Variadic externs are stable to declare; the address is
    /// taken in [`EclipseNativeProvider::with_bionic_natives`] to bind the engine's relocation.
    fn __android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;

    /// `void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...)` —
    /// DEFINED in the C shim (noreturn: emits FATAL then `abort()`). Address-only use here.
    fn __android_log_assert(cond: *const c_char, tag: *const c_char, fmt: *const c_char, ...);

    /// `int __android_log_vprint(int prio, const char* tag, const char* fmt, va_list ap)` —
    /// DEFINED in the C shim. 2026-06-12: one of `libbacktrace-native.so`'s 2 unresolved strong
    /// imports (the failed pre-load that sent its `System.loadLibrary` into the apkenv shim
    /// linker's fatal NULL `_r_debug_ptr` write — core 866509). In the x86-64 SysV ABI a `va_list`
    /// parameter is a pointer (`__va_list_tag*`), so the declaration is ABI-accurate — and it is
    /// ADDRESS-ONLY here (never called from Rust).
    fn __android_log_vprint(
        prio: c_int,
        tag: *const c_char,
        fmt: *const c_char,
        ap: *mut c_void,
    ) -> c_int;
}

/// Eclipse-owned **non-variadic** liblog sink, called by the C variadic shim
/// (`src/loader/liblog_shim.c`) after it formats the varargs into a NUL-terminated message. Routes
/// to [`emit_log`] (Eclipse's `tracing`), the same sink the fixed-arity liblog natives use. The
/// shim guarantees `tag`/`msg` are non-null NUL-terminated C strings (it substitutes `""` for a
/// null tag/fmt). 2026-06-05.
///
/// # Safety
/// `tag`/`msg` must be valid NUL-terminated C strings for the duration of the call (the C shim
/// passes its bounded stack buffer + the caller's tag, satisfying this).
#[no_mangle]
pub unsafe extern "C" fn eclipse_liblog_emit(prio: c_int, tag: *const c_char, msg: *const c_char) {
    // SAFETY: 2026-06-05 — `tag`/`msg` are the shim's NUL-terminated C strings (non-null per the
    // shim's contract); `cstr_opt`'s null-or-valid-NUL-terminated contract covers them.
    let tag = unsafe { cstr_opt(tag) }.unwrap_or_default();
    // SAFETY: 2026-06-05 — same contract as `tag` above.
    let msg = unsafe { cstr_opt(msg) }.unwrap_or_default();
    emit_log(prio, &tag, &msg);
}

// =================================================================================================
// bionic-specific libc (16) — names glibc does not export under these exact identifiers.
// =================================================================================================

// ---- FORTIFY `_chk` family — forward to the ABI-identical glibc routine, honoring the bound ------
//
// 2026-06-05: bionic's `_FORTIFY_SOURCE` `__*_chk` wrappers take an extra trailing
// "destination/known object size" argument and abort if the operation would overflow it; otherwise
// they perform the plain operation. glibc does not export these exact bionic names, but the
// *underlying* operation (`strlen`/`strchr`/`strncpy`/`write`/`fwrite`/`sendto`/FD ops) is
// ABI-identical. So each native checks the bionic bound per the public `_FORTIFY` contract (abort on
// overflow — never a silent wrong result) and forwards to the plain glibc routine. **forward.**

/// `size_t __strlen_chk(const char* s, size_t s_len)` — bionic FORTIFY strlen. Returns `strlen(s)`;
/// aborts if the computed length would reach/exceed the known object size `s_len`. **forward.**
///
/// # Safety
/// `s` must be a valid NUL-terminated C string within an object of at least `s_len` bytes.
unsafe extern "C" fn eclipse_strlen_chk(s: *const c_char, s_len: usize) -> usize {
    // SAFETY: 2026-06-05 — `s` is a valid NUL-terminated C string (caller contract); glibc `strlen`
    // reads up to the NUL. The bionic FORTIFY contract requires that NUL fall within `s_len` bytes.
    let len = unsafe { libc::strlen(s) };
    if len >= s_len {
        // bionic `__strlen_chk` calls `__fortify_fatal` (abort) on overflow — match it.
        std::process::abort();
    }
    len
}

/// `char* __strchr_chk(const char* s, int c, size_t s_len)` — bionic FORTIFY strchr. **forward.**
///
/// # Safety
/// `s` must be a valid NUL-terminated C string within an object of at least `s_len` bytes.
unsafe extern "C" fn eclipse_strchr_chk(s: *const c_char, c: c_int, s_len: usize) -> *mut c_char {
    // SAFETY: 2026-06-05 — `s` valid NUL-terminated within `s_len` bytes (caller contract). bionic
    // FORTIFY requires the scanned NUL to fall within `s_len`; verify, then forward to glibc strchr.
    let len = unsafe { libc::strlen(s) };
    if len >= s_len {
        std::process::abort();
    }
    // SAFETY: 2026-06-05 — `s` is valid for the call; glibc `strchr` scans to the NUL (within bounds
    // per the check above) and returns a pointer into `s` or null.
    unsafe { libc::strchr(s, c) }
}

/// `char* __strncpy_chk2(char* dst, const char* src, size_t n, size_t dst_len, size_t src_len)` —
/// bionic FORTIFY strncpy (the `2` variant also bounds the *source* read). **forward.**
///
/// # Safety
/// `dst` must point to at least `dst_len` writable bytes; `src` to a valid NUL-terminated source.
unsafe extern "C" fn eclipse_strncpy_chk2(
    dst: *mut c_char,
    src: *const c_char,
    n: usize,
    dst_len: usize,
    _src_len: usize,
) -> *mut c_char {
    // bionic aborts if `n > dst_len` (the copy would overflow the destination object).
    if n > dst_len {
        std::process::abort();
    }
    // SAFETY: 2026-06-05 — after the bound check, `dst` has ≥ `n` (≤ `dst_len`) writable bytes and
    // `src` is a valid NUL-terminated source; glibc `strncpy` writes exactly `n` bytes (NUL-padded),
    // ABI-identical to bionic's underlying copy.
    unsafe { libc::strncpy(dst, src, n) }
}

/// `ssize_t __write_chk(int fd, const void* buf, size_t count, size_t buf_size)` — bionic FORTIFY
/// write. Aborts if `count > buf_size` (read would overflow the source object). **forward.**
///
/// # Safety
/// `buf` must point to at least `buf_size` readable bytes; `count` ≤ `buf_size`.
unsafe extern "C" fn eclipse_write_chk(
    fd: c_int,
    buf: *const c_void,
    count: usize,
    buf_size: usize,
) -> isize {
    if count > buf_size {
        std::process::abort();
    }
    // SAFETY: 2026-06-05 — after the check, `buf` has ≥ `count` readable bytes; glibc `write` reads
    // `count` bytes from `buf` and writes them to `fd`, ABI-identical to bionic's underlying write.
    unsafe { libc::write(fd, buf, count) }
}

/// `size_t __fwrite_chk(const void* buf, size_t size, size_t count, FILE* stream, size_t buf_size)`
/// — bionic FORTIFY fwrite. Aborts if `size * count > buf_size`. **forward (stream-translated).**
///
/// # Safety
/// `buf` must point to at least `buf_size` readable bytes; `stream` must be a bionic `&__sF[i]`
/// sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fwrite_chk(
    buf: *const c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
    buf_size: usize,
) -> usize {
    // bionic aborts if the total bytes (`size * count`) would over-read the source object.
    match size.checked_mul(count) {
        Some(t) if t <= buf_size => {}
        _ => std::process::abort(),
    }
    // SAFETY: 2026-06-05 — after the check, `buf` has ≥ `size*count` readable bytes; glibc `fwrite`
    // reads that many bytes and writes them, ABI-identical. 2026-06-12: the stream may be a bionic
    // `&__sF[i]` sentinel — remapped to the host glibc stream first (see the `__sF` section).
    unsafe { libc::fwrite(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

/// `ssize_t __sendto_chk(int fd, const void* buf, size_t len, size_t buf_size, int flags,
/// const struct sockaddr* dst, socklen_t dst_len)` — bionic FORTIFY sendto. Aborts if
/// `len > buf_size`. **forward.**
///
/// # Safety
/// `buf` must point to at least `buf_size` readable bytes (`len` ≤ `buf_size`); `dst`/`dst_len`
/// describe a valid (or null) socket address per the `sendto(2)` contract.
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
    // SAFETY: 2026-06-05 — after the check, `buf` has ≥ `len` readable bytes; `dst`/`dst_len` are
    // the caller's socket-address args (null-or-valid per sendto). glibc `sendto` is ABI-identical
    // to bionic's underlying send.
    unsafe { libc::sendto(fd, buf, len, flags, dst, dst_len) }
}

/// `mode_t __umask_chk(mode_t mode)` — bionic FORTIFY umask. Aborts if `mode` carries bits outside
/// the permission mask `0777` (the public bionic FORTIFY contract — `bits/fortify/stat.h` routes
/// `umask(mode)` here and documents "called with invalid mode" as the fortify failure), else
/// forwards to glibc `umask` (ABI-identical: `mode_t` is `u32` on LP64). **forward.**
///
/// 2026-06-12: one of `libbacktrace-native.so`'s 2 unresolved strong imports — absent from glibc
/// (glibc has no `__umask_chk`) and from every Eclipse provider, the pre-load failure that sent
/// its `System.loadLibrary` into the apkenv shim linker (fatal NULL `_r_debug_ptr` write,
/// core 866509).
unsafe extern "C" fn eclipse_umask_chk(mode: libc::mode_t) -> libc::mode_t {
    if mode & !0o777 != 0 {
        // bionic `__umask_chk` calls `__fortify_fatal` (abort) on an invalid mode — match it.
        std::process::abort();
    }
    // SAFETY: 2026-06-12 — `umask(2)` takes any mode_t and cannot fail; after the bound check the
    // mode is a valid permission mask. glibc `umask` is ABI-identical to bionic's underlying umask.
    unsafe { libc::umask(mode) }
}

// FD_SET / FD_CLR / FD_ISSET FORTIFY helpers. bionic's `__FD_*_chk(int fd, fd_set*, size_t set_size)`
// abort if `fd` is out of range for an `fd_set` of `set_size` bytes, then perform the bit op. glibc
// implements the bit ops as macros (no exported function), so these are an Eclipse-owned
// **minimal-correct** implementation of the documented bit operation + the bionic bound check.

/// Whether `fd` is in range for an `fd_set` of `set_size` bytes (`set_size * 8` addressable bits).
fn fd_in_range(fd: c_int, set_size: usize) -> bool {
    fd >= 0 && (fd as usize) < set_size.saturating_mul(8)
}

/// `void __FD_SET_chk(int fd, fd_set* set, size_t set_size)` — set bit `fd`. **minimal-correct.**
///
/// # Safety
/// `set` must point to a valid `fd_set` of at least `set_size` bytes.
unsafe extern "C" fn eclipse_fd_set_chk(fd: c_int, set: *mut libc::fd_set, set_size: usize) {
    if !fd_in_range(fd, set_size) {
        std::process::abort(); // bionic aborts on an out-of-range fd for the set size.
    }
    // SAFETY: 2026-06-05 — `set` is a valid `fd_set` ≥ `set_size` bytes and `fd` is in range (check
    // above); `FD_SET` sets the one bit for `fd` within bounds.
    unsafe { libc::FD_SET(fd, set) }
}

/// `void __FD_CLR_chk(int fd, fd_set* set, size_t set_size)` — clear bit `fd`. **minimal-correct.**
///
/// # Safety
/// `set` must point to a valid `fd_set` of at least `set_size` bytes.
unsafe extern "C" fn eclipse_fd_clr_chk(fd: c_int, set: *mut libc::fd_set, set_size: usize) {
    if !fd_in_range(fd, set_size) {
        std::process::abort();
    }
    // SAFETY: 2026-06-05 — see `eclipse_fd_set_chk`; `FD_CLR` clears the one in-range bit.
    unsafe { libc::FD_CLR(fd, set) }
}

/// `int __FD_ISSET_chk(int fd, const fd_set* set, size_t set_size)` — test bit `fd`.
/// **minimal-correct.**
///
/// # Safety
/// `set` must point to a valid `fd_set` of at least `set_size` bytes.
unsafe extern "C" fn eclipse_fd_isset_chk(
    fd: c_int,
    set: *mut libc::fd_set,
    set_size: usize,
) -> c_int {
    if !fd_in_range(fd, set_size) {
        std::process::abort();
    }
    // SAFETY: 2026-06-05 — see `eclipse_fd_set_chk`; `FD_ISSET` tests the one in-range bit.
    c_int::from(unsafe { libc::FD_ISSET(fd, set) })
}

// ---- bionic internal / assert / strerror / property entry points --------------------------------

/// `int* __errno(void)` — bionic's errno accessor (glibc exports `__errno_location`, not `__errno`).
/// **forward:** returns the address of the thread-local errno via glibc's `__errno_location` (the C
/// contract is identical — a pointer to the calling thread's `int errno`).
///
/// 2026-06-05 — HONEST: this returns glibc's per-thread errno location, not a bionic one. For a
/// routine that ITSELF forwards to glibc (the `_chk` family, `write`, …) this is *consistent* —
/// glibc set the error, glibc's errno location reports it. A future fully-bionic libc would own its
/// own errno; documented as forward-to-glibc here.
extern "C" fn eclipse_errno() -> *mut c_int {
    // SAFETY: 2026-06-05 — `__errno_location()` returns a valid pointer to the calling thread's
    // `int errno`, valid for the thread's lifetime; we return it unchanged (identical C contract).
    unsafe { libc::__errno_location() }
}

/// `void __assert2(const char* file, int line, const char* func, const char* failed_expr)` —
/// bionic's 4-arg assertion failure handler (fixed arity, NOT variadic). Aborts (does not return).
/// **minimal-correct:** emits the location + failed expression to the log sink at FATAL, then aborts
/// — matching bionic's observable `__assert2` behavior (a return would be a landmine; `noreturn`).
///
/// # Safety
/// `file`/`func`/`failed_expr` must be null or valid NUL-terminated C strings.
unsafe extern "C" fn eclipse_assert2(
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    failed_expr: *const c_char,
) -> ! {
    // SAFETY: 2026-06-05 — the three pointers are the public `__assert2` C-string args (null-or-valid
    // NUL-terminated); `cstr_opt` copies each to an owned String.
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

/// `char* __gnu_strerror_r(int errnum, char* buf, size_t buflen)` — bionic's alias for the GNU
/// (char*-returning) `strerror_r`. **forward:** the GNU `strerror_r` ABI is identical; forward to
/// glibc's GNU `strerror_r` (returns a pointer to the message, in `buf` or a static string).
///
/// # Safety
/// `buf` must point to at least `buflen` writable bytes.
unsafe extern "C" fn eclipse_gnu_strerror_r(
    errnum: c_int,
    buf: *mut c_char,
    buflen: usize,
) -> *mut c_char {
    // SAFETY: 2026-06-05 — `buf`/`buflen` describe a writable buffer (caller contract). glibc's
    // default `strerror_r` is the GNU char*-returning variant — exactly the bionic `__gnu_strerror_r`
    // ABI. `gnu_strerror_r` binds that GNU symbol with the char*-returning signature.
    unsafe { gnu_strerror_r(errnum, buf, buflen) }
}

extern "C" {
    // glibc's GNU (char*-returning) `strerror_r` — bound explicitly so the return type is `char*`
    // (the `libc` crate exposes the XSI int-returning variant). This is the symbol bionic's
    // `__gnu_strerror_r` is an alias of (identical GNU ABI). 2026-06-05.
    #[link_name = "strerror_r"]
    fn gnu_strerror_r(errnum: c_int, buf: *mut c_char, buflen: usize) -> *mut c_char;
}

/// `int __system_property_get(const char* name, char* value)` — read an Android system property
/// into `value` (a `PROP_VALUE_MAX` = 92-byte buffer), returning the value's length (0 if unset).
/// glibc has no equivalent. **minimal-correct:** Eclipse has no Android property store yet, so every
/// property is treated as **unset** — the value buffer is written to an empty NUL-terminated string
/// and `0` is returned, which is exactly the bionic contract for an absent property (a real,
/// contract-correct answer, not a fake non-zero result a caller would misuse).
///
/// 2026-06-05 — documented as a minimal *empty* store: it returns the correct "property not set"
/// answer. When Eclipse grows a real property store, populate it here; callers already handle 0/empty.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; `value` must point to at least
/// `PROP_VALUE_MAX` (92) writable bytes (the bionic caller contract).
unsafe extern "C" fn eclipse_system_property_get(
    _name: *const c_char,
    value: *mut c_char,
) -> c_int {
    // Write an empty (just-NUL) value: the bionic contract for an unset property.
    if !value.is_null() {
        // SAFETY: 2026-06-05 — the bionic ABI guarantees `value` points to ≥ PROP_VALUE_MAX (92)
        // writable bytes; writing a single NUL byte at offset 0 is well within that.
        unsafe { value.write(0) };
    }
    0 // length of the (empty) value — bionic returns 0 for an unset property.
}

// =================================================================================================
// bionic SIGNAL ABI — glibc HAS these names but with an INCOMPATIBLE layout. **translate.**
// =================================================================================================
//
// 2026-06-11: bionic LP64 `sigset_t` is ONE 64-bit word (the kernel's 8-byte rt_sigset); glibc's
// `sigset_t` is 128 bytes (1024 bits). bionic LP64 `struct sigaction` is
// `{ int sa_flags; union handler; sigset_t sa_mask; void (*sa_restorer)(); }` (32 bytes:
// flags@0, handler@8, mask@16, restorer@24); glibc x86-64's is
// `{ union handler; 128-byte sa_mask; int sa_flags; sa_restorer }` (152 bytes: handler@0,
// mask@8, flags@136). Letting the engine's signal imports fall through to host glibc therefore
// SCRAMBLES registration and CORRUPTS memory:
//   - glibc `sigaction` reads its handler from offset 0 of the bionic struct — where bionic keeps
//     `sa_flags`. PROVEN on the dev host (core dump 455287, gdb): crashpad registered its
//     first-chance SIGSEGV handler and the kernel-invoked handler address was
//     0x00007fbc_08000804 = [4 bytes stack-garbage padding | SA_ONSTACK|SA_EXPOSE_TAGBITS|
//     SA_SIGINFO] — exactly a bionic `sa_flags` value read as a pointer → the handler delivery
//     itself faulted (double-SIGSEGV death with no crash report).
//   - glibc `sigfillset`, and `sigprocmask`/`pthread_sigmask` with a non-null `oldset`, WRITE 128
//     bytes through the caller's 8-byte bionic set — silent stack corruption.
// The engine work-list (readelf, libroblox.so + libbacktrace-native.so, 2026-06-11): `sigaction`,
// `sigemptyset`, `sigaddset`, `sigfillset`, `sigprocmask`, `pthread_sigmask` — these six are
// translating natives. `sigaltstack` is the seventh: bionic/glibc `stack_t` ARE layout-identical
// on x86-64 (`{void* ss_sp; int ss_flags; size_t ss_size}`), so it forwarded on the host baseline
// until 2026-06-12 (core 1223806) — a silent SI_KERNEL/addr=0 force_sigsegv kill proved the
// kernel's signal-frame write targeted a registered-but-UNWRITABLE SA_ONSTACK altstack, and the
// host-baseline pass-through left Eclipse with zero attribution for WHO registered it. It is now
// an Eclipse-owned pure forward that logs+records every registration with caller attribution
// (see the sigaltstack section below).

/// Bionic LP64 `sigset_t` — one 64-bit word (signals 1–64, bit `signum-1`).
type BionicSigsetT = u64;

/// Bionic LP64 x86-64 `struct sigaction` (AOSP `bits/signal_types.h`), 32 bytes. The
/// `#[repr(C)]` field order yields flags@0 (+4 padding), handler@8, mask@16, restorer@24 —
/// pinned by `bionic_sigaction_layout_matches_lp64` below.
// 2026-06-12: PartialEq/Eq derived (field-wise, padding ignored) for the install's
// re-seed-only-if-changed comparison in `install_early_fault_tap`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct BionicSigaction {
    /// `int sa_flags` — kernel flag bits (bionic passes them through unchanged).
    sa_flags: c_int,
    /// The `sa_handler`/`sa_sigaction` union member (one pointer; which one is live is
    /// `SA_SIGINFO`-determined, kernel-side — opaque here).
    handler: usize,
    /// `sigset_t sa_mask` — the 64-bit bionic set.
    sa_mask: BionicSigsetT,
    /// `void (*sa_restorer)(void)` — bionic/glibc each install their own; never forwarded.
    sa_restorer: usize,
}

/// `SA_RESTORER` (0x04000000) — stripped both ways: glibc supplies its own `__restore_rt` when
/// registering, and reporting glibc's restorer back to bionic-ABI callers would leak a pointer
/// they re-register with the wrong libc's semantics.
const SA_RESTORER_FLAG: c_int = 0x0400_0000;

/// Widen a bionic 64-bit set into a glibc `sigset_t` (zeroed = empty; the kernel consumes only
/// the first word — signals 1–64 — which is the entire bionic set).
fn glibc_sigset_from_bionic(set: BionicSigsetT) -> libc::sigset_t {
    // SAFETY: 2026-06-11 — an all-zero glibc sigset_t is exactly `sigemptyset`'s result (glibc
    // memsets 0); copying the one 64-bit word into the first word makes the same set the kernel
    // would decode from the bionic value (both are the raw kernel bitmask for signals 1–64).
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

/// Narrow a glibc `sigset_t` to the bionic 64-bit set (the first word — signals 1–64 — is all
/// bionic can represent, and all the kernel uses with an 8-byte rt_sigset).
fn bionic_sigset_from_glibc(set: &libc::sigset_t) -> BionicSigsetT {
    let mut b: BionicSigsetT = 0;
    // SAFETY: 2026-06-11 — reads the first 8 bytes of the (≥ 8-byte) glibc set into the bionic
    // word; both are the raw kernel bitmask for signals 1–64.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (set as *const libc::sigset_t).cast::<u8>(),
            (&raw mut b).cast::<u8>(),
            std::mem::size_of::<BionicSigsetT>(),
        );
    }
    b
}

/// Translate a glibc-shaped `sigaction` to the bionic LP64 shape: strip `SA_RESTORER` from
/// `sa_flags`, carry the handler, narrow the mask, and force `sa_restorer = 0` (glibc's restorer
/// must never leak to a bionic-ABI consumer).
///
/// 2026-06-12: factored out of [`eclipse_sigaction`]'s `oldact` back-translation so the
/// early-fault tap's chain-slot seeding ([`install_early_fault_tap`]) and the live translation
/// path cannot drift apart (pinned by `tap_si_code_consts_match_kernel_uapi` and the live SIGURG
/// round-trip test).
fn bionic_action_from_glibc(g: &libc::sigaction) -> BionicSigaction {
    BionicSigaction {
        sa_flags: g.sa_flags & !SA_RESTORER_FLAG,
        handler: g.sa_sigaction,
        sa_mask: bionic_sigset_from_glibc(&g.sa_mask),
        sa_restorer: 0,
    }
}

/// Set the calling thread's `errno` to `EINVAL` and return `-1` (the bionic sigset-op error path).
fn einval() -> c_int {
    // SAFETY: 2026-06-11 — `__errno_location()` returns a valid pointer to the calling thread's
    // errno (same location `eclipse_errno` exposes to the engine, so the engine reads it back).
    unsafe { *libc::__errno_location() = libc::EINVAL };
    -1
}

/// `int sigaction(int signum, const struct sigaction* act, struct sigaction* oldact)` — bionic
/// signal-handler registration. **translate:** bionic struct → glibc struct, forward to glibc
/// `sigaction` (which supplies its own `sa_restorer`), translate the old action back. The
/// handler/flags/mask round-trip losslessly, so a caller that saves `oldact` and later
/// re-registers it (crashpad's chain-to-previous on a non-handled fault) restores the exact
/// kernel state.
///
/// # Safety
/// `act`/`oldact` must each be null or valid pointers to a bionic LP64 `struct sigaction`.
unsafe extern "C" fn eclipse_sigaction(
    signum: c_int,
    act: *const BionicSigaction,
    oldact: *mut BionicSigaction,
) -> c_int {
    // 2026-06-12: early-fault-tap seam — when `signum` is the tapped signal, the diagnostic tap
    // owns the KERNEL slot, and the engine's registration goes to the Eclipse-owned chain slot
    // instead (the tap stays kernel-first by construction; the tap chains to the slot occupant
    // after dumping). Every other signal's translation below stays byte-identical.
    let tapped = TAPPED_SIGNAL.load(Ordering::Acquire);
    if tapped != 0 && signum == tapped {
        // SAFETY: 2026-06-12 — `act`/`oldact` are null or valid bionic sigactions (this fn's
        // caller contract), exactly the contract `tap_chain_register` requires.
        return unsafe { tap_chain_register(act, oldact) };
    }
    let g_act = if act.is_null() {
        None
    } else {
        // SAFETY: 2026-06-11 — `act` is a valid bionic sigaction (caller contract); plain read.
        let b = unsafe { *act };
        // SAFETY: 2026-06-11 — all-zero is a valid glibc sigaction baseline (empty mask, no
        // flags, null restorer); the three bionic fields are then translated in.
        let mut g: libc::sigaction = unsafe { std::mem::zeroed() };
        g.sa_sigaction = b.handler;
        g.sa_mask = glibc_sigset_from_bionic(b.sa_mask);
        g.sa_flags = b.sa_flags & !SA_RESTORER_FLAG;
        Some(g)
    };
    // SAFETY: 2026-06-11 — all-zero is a valid out-param baseline glibc `sigaction` overwrites.
    let mut g_old: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-11 — `g_act`/`g_old` are valid (or null) glibc-layout structs built above;
    // glibc `sigaction` performs the kernel registration with its own restorer.
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
        // SAFETY: 2026-06-11 — `oldact` is a valid bionic sigaction out-param (caller contract);
        // glibc filled `g_old` on success.
        unsafe {
            *oldact = bionic_action_from_glibc(&g_old);
        }
    }
    ret
}

/// `int sigemptyset(sigset_t* set)` — clear the bionic 64-bit set. **minimal-correct** (bionic
/// returns `EINVAL` for a null set; glibc's would zero 128 bytes).
///
/// # Safety
/// `set` must be null or a valid pointer to a bionic `sigset_t` (one 64-bit word).
unsafe extern "C" fn eclipse_sigemptyset(set: *mut BionicSigsetT) -> c_int {
    if set.is_null() {
        return einval();
    }
    // SAFETY: 2026-06-11 — non-null per the check; writes exactly the bionic 8-byte set.
    unsafe { *set = 0 };
    0
}

/// `int sigfillset(sigset_t* set)` — fill the bionic 64-bit set. **minimal-correct** (glibc's
/// would write 128 bytes — the corruption case).
///
/// # Safety
/// `set` must be null or a valid pointer to a bionic `sigset_t` (one 64-bit word).
unsafe extern "C" fn eclipse_sigfillset(set: *mut BionicSigsetT) -> c_int {
    if set.is_null() {
        return einval();
    }
    // SAFETY: 2026-06-11 — non-null per the check; writes exactly the bionic 8-byte set.
    unsafe { *set = !0 };
    0
}

/// `int sigaddset(sigset_t* set, int signum)` — set bit `signum-1` in the bionic 64-bit set.
/// **minimal-correct** (bionic bounds the bit to the set width → `EINVAL` outside 1–64).
///
/// # Safety
/// `set` must be null or a valid pointer to a bionic `sigset_t` (one 64-bit word).
unsafe extern "C" fn eclipse_sigaddset(set: *mut BionicSigsetT, signum: c_int) -> c_int {
    let bit = signum.wrapping_sub(1);
    if set.is_null() || !(0..64).contains(&bit) {
        return einval();
    }
    // SAFETY: 2026-06-11 — non-null per the check; `bit` ∈ 0..64 so the shift is in range.
    unsafe { *set |= 1u64 << bit };
    0
}

/// `int sigprocmask(int how, const sigset_t* set, sigset_t* oldset)` — bionic 64-bit sets.
/// **translate:** widen `set`, forward to glibc (the `how` values are the shared kernel ABI:
/// SIG_BLOCK=0/SIG_UNBLOCK=1/SIG_SETMASK=2), narrow `oldset` back (glibc writing its own
/// 128-byte out-param, not the caller's 8-byte one).
///
/// # Safety
/// `set`/`oldset` must each be null or valid pointers to a bionic `sigset_t` (one 64-bit word).
unsafe extern "C" fn eclipse_sigprocmask(
    how: c_int,
    set: *const BionicSigsetT,
    oldset: *mut BionicSigsetT,
) -> c_int {
    let g_set = if set.is_null() {
        None
    } else {
        // SAFETY: 2026-06-11 — `set` is a valid bionic set (caller contract); plain read.
        Some(glibc_sigset_from_bionic(unsafe { *set }))
    };
    // SAFETY: 2026-06-11 — all-zero is a valid out-param baseline glibc overwrites.
    let mut g_old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-11 — translated glibc-layout sets (or null), per the sigprocmask contract.
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
        // SAFETY: 2026-06-11 — `oldset` is a valid bionic set out-param (caller contract).
        unsafe { *oldset = bionic_sigset_from_glibc(&g_old) };
    }
    ret
}

/// `int pthread_sigmask(int how, const sigset_t* set, sigset_t* oldset)` — the calling thread's
/// mask, bionic 64-bit sets. **translate** (same widening/narrowing as [`eclipse_sigprocmask`];
/// returns an errno VALUE — 0 on success — instead of setting `errno`, per the pthread contract).
///
/// # Safety
/// `set`/`oldset` must each be null or valid pointers to a bionic `sigset_t` (one 64-bit word).
unsafe extern "C" fn eclipse_pthread_sigmask(
    how: c_int,
    set: *const BionicSigsetT,
    oldset: *mut BionicSigsetT,
) -> c_int {
    let g_set = if set.is_null() {
        None
    } else {
        // SAFETY: 2026-06-11 — `set` is a valid bionic set (caller contract); plain read.
        Some(glibc_sigset_from_bionic(unsafe { *set }))
    };
    // SAFETY: 2026-06-11 — all-zero is a valid out-param baseline glibc overwrites.
    let mut g_old: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-11 — translated glibc-layout sets (or null), per the pthread_sigmask
    // contract (thread-local mask; returns the error value directly).
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
        // SAFETY: 2026-06-11 — `oldset` is a valid bionic set out-param (caller contract).
        unsafe { *oldset = bionic_sigset_from_glibc(&g_old) };
    }
    ret
}

// ---- EARLY-FAULT TAP (diagnostic, 2026-06-12) ---------------------------------------------------
//
// A kernel-first SA_SIGINFO handler that dumps the ORIGINAL engine fault's verbatim context
// (si_signo/si_code/si_addr, RIP/RSP/RBP/REG_ERR, a bounded frame-pointer walk) BEFORE handing the
// signal to exactly the handler that would have run without the tap. It exists because crashpad's
// first-chance SIGSEGV handler now runs (the bionic signal ABI above is load-bearing) but dies
// inside its own fputs path before logging the fault it was handling (AGENTS.md §5) — the tap makes
// the original fault visible; it never suppresses, repairs, or reroutes it.
//
// Two-layer chaining keeps the tap kernel-first by construction (the same interposition model
// libsigchain uses, at the layer Eclipse already owns): `install_early_fault_tap` seeds the
// Eclipse-owned chain slot with the kernel's true current action (ART/sigchain's, post-boot —
// QUERIED and seeded BEFORE the tap is raw-glibc-registered, so the handler can never run against
// an empty slot; 2026-06-12), then registers the tap; afterwards, the seam in `eclipse_sigaction`
// routes the engine's (crashpad's) registration for the tapped signal into that same slot instead
// of the kernel. If
// crashpad's registration reached the kernel it would be kernel-first, and its currently-faulting
// logging path would kill the process before ever chaining to the tap — logging NOTHING.

/// The tapped signal number (0 = no tap installed). Doubles as the [`eclipse_sigaction`] seam
/// gate — signals are >= 1, so no separate installed flag is needed. Stored LAST by
/// [`install_early_fault_tap`] (Release), after the chain slot is seeded, so the Acquire-loading
/// seam never sees gate-open with an empty slot.
static TAPPED_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// The Eclipse-owned chain slot: the action the tap dispatches to after dumping. Values are
/// claim-once cells of the static [`TAP_CHAIN_POOL`] — a single `AtomicPtr` store publishes
/// handler/flags/mask atomically (no tearing readable from signal context).
///
/// 2026-06-12: superseded cells are intentionally never freed or reused — a concurrently
/// running tap handler may still hold the pointer. Do NOT "fix" this into reuse — that is a
/// torn read in signal context.
static TAP_CHAIN: AtomicPtr<BionicSigaction> = AtomicPtr::new(std::ptr::null_mut());

/// [`TAP_CHAIN_POOL`] cell count. 2026-06-12: the real signal flow claims 3 (the install's
/// query seed + crashpad's register + crashpad's chain-to-previous restore) — 4 only if a
/// re-registration races the install's query→install window (the one-shot re-seed in
/// [`install_early_fault_tap`]); 8 leaves headroom for re-registrations.
const TAP_CHAIN_POOL_LEN: usize = 8;

/// Backing store for [`TAP_CHAIN`]: a fixed pool of claim-once cells, **no heap**. It exists
/// because [`tap_chain_register`] is reachable INSIDE the fault-handler chain — crashpad's
/// documented not-handled flow (`Signals::RestoreHandlerAndReraiseSignalOnReturn`) re-registers
/// the saved previous action via `sigaction` FROM WITHIN its handler, which resolves through
/// the engine PLT → [`eclipse_sigaction`] → the tapped-signal seam. The interrupted context is
/// arbitrary engine code (the original fault may itself be mid-`malloc`), and glibc's arena
/// lock is not reentrant — an allocation here deadlocks or corrupts the allocator instead of
/// dump+death, in exactly the flow the tap exists to diagnose. 2026-06-12.
///
/// Each cell is claimed at most once ([`TAP_CHAIN_POOL_NEXT`] only grows), fully written, then
/// published through [`TAP_CHAIN`] (Release) — immutable from then on. On exhaustion the slot
/// keeps its last occupant (see [`tap_chain_publish`]).
struct TapChainPool([UnsafeCell<BionicSigaction>; TAP_CHAIN_POOL_LEN]);

impl TapChainPool {
    /// All-unclaimed pool (cells zeroed; a cell's value is meaningless until published).
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

// SAFETY: 2026-06-12 — each cell is written at most once, by the unique claimant of its index
// (the paired fetch_add cursor), strictly BEFORE its pointer is published through an `AtomicPtr`
// Release store; every cross-thread read goes through an Acquire load of that pointer, so each
// read happens-after the one write (no data race, no tearing).
unsafe impl Sync for TapChainPool {}

/// See [`TapChainPool`].
static TAP_CHAIN_POOL: TapChainPool = TapChainPool::new();

/// One-past-the-last claimed [`TAP_CHAIN_POOL`] cell. Grows only (cells are never reclaimed).
static TAP_CHAIN_POOL_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Claim the next unclaimed cell of `pool`, write `b` into it, and publish it to `slot`
/// (Release). **Async-signal-safe by construction:** one `fetch_add`, one plain write to a cell
/// nothing else can reference yet, one Release store — no allocation, no locks (the whole point
/// — see [`TAP_CHAIN_POOL`]). On exhaustion `slot` keeps its current occupant (still a real,
/// previously published action — benign) and `false` is returned, with one `write(2)` note per
/// attempt (fd 2, async-signal-safe).
///
/// `pool` and `next` must be used as an exclusive pair (no other cursor claims this pool's
/// cells) — the process statics above, or a test-local pair (the parametrization exists so the
/// exhaustion test below cannot poison the process-global pool). `next` overrunning the pool is
/// harmless (`get` bounds it; wrapping would take 2^64 claims).
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
        // SAFETY: 2026-06-12 — write(2) is async-signal-safe; MSG is a static byte string.
        unsafe { libc::write(2, MSG.as_ptr().cast::<c_void>(), MSG.len()) };
        return false;
    };
    // SAFETY: 2026-06-12 — `idx` is unique to this call (fetch_add on the pool's exclusive
    // cursor), so this cell is written exactly once, before its pointer is published below; no
    // other reference to it can exist yet, and it is never written again afterwards (see the
    // TapChainPool Sync justification).
    unsafe { cell.get().write(b) };
    slot.store(cell.get(), Ordering::Release);
    true
}

/// Publish `b` as the new [`TAP_CHAIN`] occupant via the static pool ([`tap_chain_publish`]).
fn tap_chain_store(b: BionicSigaction) -> bool {
    tap_chain_publish(&TAP_CHAIN_POOL, &TAP_CHAIN_POOL_NEXT, &TAP_CHAIN, b)
}

/// Re-entry latch: the tid of the thread currently inside the tap handler (0 = none). Only a
/// second SYNCHRONOUS entry on the SAME thread (a recursive tap fault, or an ART-sigchain
/// re-front cycle: sigchain → tap → chained `SignalChain::Handler` → sigchain walks → tap) bails
/// to `SIG_DFL` and returns, so the kernel kills with the ORIGINAL siginfo (dump already written).
///
/// 2026-06-12: tid-scoped, NOT a process-global bool. The tap is kernel-first for EVERY delivery
/// of the tapped signal (the engine-PC filter gates only the dump), and a fault on ANOTHER
/// thread while one thread is mid-handler is routine concurrency — on x86-64 ART delivers
/// managed NPE/StackOverflow fixups via SIGSEGV, and SIGSEGV is blocked only on the handling
/// thread. A global bool misread such a concurrent entry as recursion and bailed it to SIG_DFL,
/// turning two overlapping recoverable faults into whole-process death and stripping the
/// tap+chain from the kernel slot. A different-tid entry proceeds concurrently instead (see
/// [`tap_entry_claim`]): all per-fault handler state is stack-local, [`TAP_CHAIN`] reads are
/// Acquire loads of immutable cells, and the dump is one `write(2)` — at worst two dumps
/// interleave on fd 2.
static TAP_HANDLER_TID: AtomicI64 = AtomicI64::new(0);

/// One tap-handler entry's [`TAP_HANDLER_TID`] outcome (see [`tap_entry_claim`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapEntryClaim {
    /// This thread claimed the latch — release it (store 0) on handler exit.
    Latched,
    /// Another thread is mid-handler: proceed concurrently WITHOUT the latch (never release).
    /// Same-thread recursion detection is degraded for this one entry — accepted: degrading a
    /// diagnostic beats escalating a concurrent recoverable fault to a process kill.
    Unlatched,
    /// This thread is already inside the handler — synchronous re-entry; bail to `SIG_DFL`.
    SameThreadReentry,
}

/// Classify one tap entry against `latch` (claim-or-classify): async-signal-safe by
/// construction — one CAS, no allocation, no locks. Parametrized over the latch so the
/// cross-thread contract — a different tid while the latch is held PROCEEDS, never bails — is
/// unit-testable without touching the process-global [`TAP_HANDLER_TID`] (the
/// [`tap_chain_publish`] parametrization pattern). Kernel tids are >= 1, so 0 is a safe
/// "unheld" sentinel.
fn tap_entry_claim(latch: &AtomicI64, tid: i64) -> TapEntryClaim {
    match latch.compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => TapEntryClaim::Latched,
        Err(owner) if owner == tid => TapEntryClaim::SameThreadReentry,
        Err(_) => TapEntryClaim::Unlatched,
    }
}

/// libroblox's mapped `[base, base+span)` range, published by `engine.rs` after map/relocate/
/// resolve and BEFORE any engine instruction runs. 0 = unknown → the tap dumps every tapped
/// signal (detect, don't assume); published → dumps only engine-PC faults (silences ART's
/// routine managed-fault fixups).
static ENGINE_RANGE_BASE: AtomicU64 = AtomicU64::new(0);
/// See [`ENGINE_RANGE_BASE`].
static ENGINE_RANGE_SPAN: AtomicU64 = AtomicU64::new(0);

// 2026-06-12: kernel UAPI `asm-generic/siginfo.h` SIGSEGV si_codes: SEGV_MAPERR=1 (address not
// mapped), SEGV_ACCERR=2 (invalid permissions). Pinned locally because the pinned libc 0.2.186
// does NOT define them for linux-gnu (only hurd/aix); guarded by
// `tap_si_code_consts_match_kernel_uapi`.
const SEGV_MAPERR: c_int = 1;
const SEGV_ACCERR: c_int = 2;

/// Register/query the tapped signal's handler against the Eclipse-owned chain slot instead of
/// the kernel — the kernel slot stays the tap's, so the tap is always-first by construction.
/// Mirrors the observable `sigaction` contract: `oldact` receives the previous slot occupant
/// (already bionic-shaped, restorer already 0); `act`, when non-null, becomes the new occupant
/// (restorer stripped exactly as the real registration path does). Never calls the kernel;
/// returns 0, so a caller's save/restore round-trip is indistinguishable from real registration.
///
/// # Safety
/// `act`/`oldact` must each be null or valid pointers to a bionic LP64 `struct sigaction`.
unsafe fn tap_chain_register(act: *const BionicSigaction, oldact: *mut BionicSigaction) -> c_int {
    if !oldact.is_null() {
        let prev = TAP_CHAIN.load(Ordering::Acquire);
        let out = if prev.is_null() {
            // Not reachable after install (the slot is seeded before the gate opens) — report the
            // disposition a fresh signal would have: SIG_DFL (handler 0), empty mask, no flags.
            BionicSigaction {
                sa_flags: 0,
                handler: 0,
                sa_mask: 0,
                sa_restorer: 0,
            }
        } else {
            // SAFETY: 2026-06-12 — `prev` is a published TAP_CHAIN_POOL cell: immutable after
            // publication, never freed or reused (see TAP_CHAIN); plain copy read.
            unsafe { *prev }
        };
        // SAFETY: 2026-06-12 — `oldact` is a valid bionic sigaction out-param (caller contract).
        unsafe { *oldact = out };
    }
    if !act.is_null() {
        // SAFETY: 2026-06-12 — `act` is a valid bionic sigaction (caller contract); plain read.
        let mut b = unsafe { *act };
        // The restorer never crosses the seam (mirrors eclipse_sigaction's on-the-wire strip).
        b.sa_flags &= !SA_RESTORER_FLAG;
        b.sa_restorer = 0;
        // 2026-06-12: alloc-free publish — this seam runs in HANDLER context via crashpad's
        // restore-and-reraise flow (see TAP_CHAIN_POOL). On exhaustion the slot keeps its last
        // occupant and 0 is still returned (the chain still dispatches a real action; the real
        // flow claims 3 of the 8 cells — 4 with the install's raced re-seed — so this cannot
        // fire in practice).
        let _ = tap_chain_store(b);
    }
    0
}

/// Re-install the kernel default action for `signo` via raw glibc `sigaction` (async-signal-
/// safe). Returning afterwards re-executes the faulting instruction, so the kernel kills with
/// the ORIGINAL si_code/si_addr — strictly better than `raise()`, which would deliver SI_TKILL.
fn tap_restore_default(signo: c_int) {
    // SAFETY: 2026-06-12 — an all-zero glibc sigaction (handler SIG_DFL=0, empty mask, no flags)
    // is the kernel default action; sigaction(2) is async-signal-safe (signal-safety(7)).
    unsafe {
        let dfl: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signo, &dfl, std::ptr::null_mut());
    }
}

/// Non-faulting read of one `u64` at `addr` in this process's own address space.
///
/// 2026-06-12: one `process_vm_readv(2)` syscall on self — the pinned libc 0.2.186 declares no
/// `process_vm_readv` FUNCTION for linux-gnu (only android/l4re; linux-gnu has only
/// `SYS_process_vm_readv`), so this goes through `libc::syscall`. Not on the POSIX
/// async-signal-safe list, but it is a single non-allocating Linux syscall with no userspace
/// state, and the probe+copy is atomic (no TOCTOU against running engine threads) — an unmapped
/// `addr` yields EFAULT/partial instead of faulting this thread. Linux-only is Eclipse's target.
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
    // SAFETY: 2026-06-12 — the local iovec covers the 8 writable bytes of `val` on this stack;
    // the remote iovec is only an address the KERNEL reads from our own address space (pid =
    // self), per process_vm_readv(2) `(pid, local_iov, 1, remote_iov, 1, flags=0)`. The kernel
    // returns the byte count or -1; it never dereferences in this thread's context.
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

/// Bounded SysV AMD64 frame-pointer walk into `out`; returns the entry count. `out[0]` is `rip`
/// itself; subsequent entries are return addresses from the RBP chain (`[fp+8]` = return
/// address, `[fp]` = caller's frame pointer). A step is accepted iff `fp != 0`, `fp % 8 == 0`,
/// `fp > rsp`, `next > fp` (strictly increasing — guarantees termination) and
/// `next - fp < 1 MiB` (a plausible frame size); every load is a non-faulting [`tap_read_u64`].
/// Code built without frame pointers dies at frame 0 — accepted (the register + si_* lines are
/// the primary diagnostic; a raw-stack-scan fallback is an explicit follow-up, not v1).
fn tap_stack_walk(rip: u64, rsp: u64, rbp: u64, out: &mut [u64; 32]) -> usize {
    const MAX_FRAME_STEP: u64 = 1 << 20; // 1 MiB
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

/// Append `0x<val>` plus, when `val` lies in the published engine range, ` (libroblox+0x<off>)`.
/// Async-signal-safe (the bounded init_run formatters only).
fn tap_write_addr(buf: &mut [u8], n: &mut usize, val: u64, base: u64, span: u64) {
    write_bytes(buf, n, b"0x");
    write_hex(buf, n, val);
    if base != 0 && (base..base.wrapping_add(span)).contains(&val) {
        write_bytes(buf, n, b" (libroblox+0x");
        write_hex(buf, n, val - base);
        write_bytes(buf, n, b")");
    }
}

/// The early-fault tap handler — kernel-first for the tapped signal. Dumps the verbatim fault
/// context with async-signal-safe primitives only (no stdio — the exact crashpad failure mode
/// being diagnosed — no alloc, no locks, no panic path), then chains to the [`TAP_CHAIN`] slot:
/// exactly the handler that would have run without the tap. Purely a diagnostic — it never
/// masks, repairs, or reroutes the fault.
///
/// # Safety
/// Installed via `sigaction` with `SA_SIGINFO`; the kernel invokes it with a valid (or null —
/// read defensively) `siginfo_t*`/`ucontext_t*` for the interrupted context.
unsafe extern "C" fn early_fault_tap_handler(
    signo: c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut c_void,
) {
    // signal-safety(7): preserve the interrupted context's errno across the handler.
    // SAFETY: 2026-06-12 — __errno_location() returns the calling thread's errno slot.
    let saved_errno = unsafe { *libc::__errno_location() };

    // SAFETY: 2026-06-12 — gettid(2) via raw syscall: one async-signal-safe syscall, no
    // userspace state (the glibc gettid() wrapper needs glibc >= 2.30 — the raw syscall stays
    // distro-portable, like tap_read_u64's SYS_process_vm_readv).
    let my_tid = unsafe { libc::syscall(libc::SYS_gettid) } as i64;
    // Re-entry latch (tid-scoped — see TAP_HANDLER_TID): only a second synchronous entry on
    // THIS thread (a recursive tap fault or a sigchain re-front cycle) bails to SIG_DFL and
    // returns, so the original faulting instruction re-executes and the kernel kills with the
    // ORIGINAL siginfo (first dump already written). A different thread mid-handler is routine
    // concurrency and proceeds.
    let claim = tap_entry_claim(&TAP_HANDLER_TID, my_tid);
    if claim == TapEntryClaim::SameThreadReentry {
        tap_restore_default(signo);
        // SAFETY: 2026-06-12 — restore errno (same thread-local slot as above).
        unsafe { *libc::__errno_location() = saved_errno };
        return;
    }

    let (si_signo, si_code, si_addr) = if info.is_null() {
        (signo, 0, 0u64)
    } else {
        // SAFETY: 2026-06-12 — the kernel passes a valid `siginfo_t*` with SA_SIGINFO (null
        // checked above); si_signo/si_code are public c_int fields and `si_addr()` reads the
        // sigfault union member (meaningful for faults, merely informative otherwise).
        unsafe { ((*info).si_signo, (*info).si_code, (*info).si_addr() as u64) }
    };
    let (rip, rsp, rbp, err) = if ctx.is_null() {
        (0u64, 0u64, 0u64, 0u64)
    } else {
        // SAFETY: 2026-06-12 — with SA_SIGINFO the third argument is the `ucontext_t*` of the
        // interrupted context; the greg indices REG_RIP=16/REG_RSP=15/REG_RBP=10/REG_ERR=19 are
        // the pinned glibc x86-64 layout (verified in libc 0.2.186's x86_64 module).
        let uc = unsafe { &*ctx.cast::<libc::ucontext_t>() };
        (
            uc.uc_mcontext.gregs[libc::REG_RIP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_RSP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_RBP as usize] as u64,
            uc.uc_mcontext.gregs[libc::REG_ERR as usize] as u64,
        )
    };

    // Engine-PC filter: with the libroblox range published, dump only engine-PC faults (ART's
    // routine managed-fault fixups stay silent); unpublished (base 0) → dump everything.
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
            // e.g. SI_TKILL (-6) from raise()/tgkill — keep the sign readable.
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
        // REG_ERR = the x86 page-fault error code: bit0 present, bit1 write, bit4 ifetch.
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
        // SAFETY: 2026-06-12 — write(2) is async-signal-safe; `buf[..n]` is an initialized
        // stack byte range. ONE raw write keeps the dump contiguous on fd 2 (the run's log).
        unsafe { libc::write(2, buf.as_ptr().cast::<c_void>(), n) };
    }

    // SAFETY: 2026-06-12 — restore the saved errno (signal-safety(7)).
    unsafe { *libc::__errno_location() = saved_errno };

    // Chain to exactly the handler that would have run without the tap. Deliberate, documented
    // simplifications (irrelevant to crashpad's proven flags 0x08000804): the slot's sa_mask is
    // not applied around the chained call (the tapped signal itself is already blocked), and
    // SA_RESETHAND is not emulated.
    let p = TAP_CHAIN.load(Ordering::Acquire);
    if p.is_null() {
        // 2026-06-12: not reachable from a real boot — `install_early_fault_tap` seeds the slot
        // BEFORE the tap owns the kernel slot (the closed seed-window race). Kept as the
        // defensive floor (the tap test's cleanup nulls the slot).
        tap_restore_default(signo);
    } else {
        // SAFETY: 2026-06-12 — `p` is a published TAP_CHAIN_POOL cell: immutable after
        // publication, never freed or reused.
        let chain = unsafe { *p };
        if chain.handler == libc::SIG_DFL {
            // Re-install the default and RETURN: a fault re-executes and the kernel kills with
            // the ORIGINAL si_code/si_addr; a non-fault signal simply continues.
            tap_restore_default(signo);
        } else if chain.handler == libc::SIG_IGN {
            // Ignore — nothing to call.
        } else if chain.sa_flags & libc::SA_SIGINFO != 0 {
            // SAFETY: 2026-06-12 — the slot holds the handler address the engine registered
            // through the seam with SA_SIGINFO; calling it as the three-argument form with the
            // kernel's (signo, info, ctx) is exactly the delivery the kernel would have
            // performed had the registration reached it. Returning afterwards resumes via the
            // tap's normal sigreturn, so any ucontext fixup the chained handler made (ART's
            // modify-and-return pattern) takes effect.
            let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                unsafe { std::mem::transmute::<usize, _>(chain.handler) };
            f(signo, info, ctx);
        } else {
            // SAFETY: 2026-06-12 — as above, for the classic one-argument sa_handler form.
            let f: extern "C" fn(c_int) = unsafe { std::mem::transmute::<usize, _>(chain.handler) };
            f(signo);
        }
    }
    // Release only what this entry claimed — an Unlatched (concurrent) entry must never clear
    // the owner's latch out from under it.
    if claim == TapEntryClaim::Latched {
        TAP_HANDLER_TID.store(0, Ordering::SeqCst);
    }
}

/// Install the early-fault tap for `signum`: (1) query the kernel's CURRENT action (ART/
/// sigchain's post-boot handler when ART claimed the signal), (2) seed the chain slot from it,
/// (3) register [`early_fault_tap_handler`] kernel-first via RAW glibc `sigaction`, (4) re-seed
/// from the install's returned oldact only if it differs from the queried action, then open the
/// [`eclipse_sigaction`] seam gate. Idempotent: a second call (any signal) is a no-op `Ok`.
///
/// 2026-06-12: seed-BEFORE-install (closes the reviewed seed-window race, AGENTS.md §6 carried
/// note (a)): the tap is kernel-first for EVERY delivery the moment the install syscall
/// returns, including deliveries on OTHER threads (routine on x86-64 — ART delivers managed
/// NPE/StackOverflow fixups via SIGSEGV). With the prior install-then-seed order, such a
/// delivery in the sub-microsecond seed window entered the handler with a null [`TAP_CHAIN`] →
/// `tap_restore_default` installed SIG_DFL process-wide → a recoverable fault killed the
/// process. Seeding first closes the window by construction: the seed's Release publish is
/// program-ordered before the install syscall, and a handler entry can only follow the install,
/// so it always Acquire-loads an occupied slot. Step (4) covers a re-registration landing
/// between query and install (then the install's oldact, not the queried action, is the true
/// pre-tap action); the comparison keeps the quiescent flow at ONE claimed pool cell — two only
/// in that raced case (see [`TAP_CHAIN_POOL_LEN`]).
///
/// 2026-06-12: RAW glibc (not the bionic translating native) because Eclipse is the caller — no
/// bionic ABI boundary to cross — and the kernel registration must really happen (the seam
/// deliberately stops kernel forwarding for the tapped signal, so installing "through" the
/// tapped path would be self-defeating); the glibc-shaped queried/old actions also feed the
/// shared [`bionic_action_from_glibc`] to seed the chain slot. Flags: `SA_SIGINFO | SA_ONSTACK`,
/// empty mask (no SA_NODEFER — the tapped signal stays blocked during the handler).
///
/// 2026-06-12 (core 866509): the earlier "deliberately NO Eclipse `sigaltstack`" stance here is
/// DISPROVEN — its premise ("the known fault is not a stack overflow") missed that the
/// tap→sigchain→ART-dump chain itself overflows ART's 32 KiB heap-backed main-thread altstack
/// (~79.2 KiB measured), silently zeroing live heap below `ss_sp` (the `malloc(): unaligned
/// tcache chunk detected` SIGABRT that destroyed the crash report). Its clobber reasoning was
/// also backwards: ART overwrites whatever altstack exists at attach, not the reverse. Eclipse
/// now REPLACES the main thread's altstack with a guard-paged mmap'd one right after
/// `JNI_CreateJavaVM` — see [`install_guarded_altstack`] and the `runtime::boot` wiring.
pub(super) fn install_early_fault_tap(signum: c_int) -> Result<(), String> {
    if TAPPED_SIGNAL.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // (1) Query the current kernel action (act null = pure query, sigaction(2)); an invalid
    // `signum` fails HERE, before any cell is claimed or anything is installed.
    // SAFETY: 2026-06-12 — all-zero is a valid out-param baseline glibc `sigaction` overwrites;
    // a null act makes this a query that changes nothing.
    let mut queried: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-12 — `queried` is a valid out-param; act is null (query-only).
    if unsafe { libc::sigaction(signum, std::ptr::null(), &mut queried) } != 0 {
        return Err(format!(
            "raw sigaction({signum}) query failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // (2) Seed the chain slot BEFORE the tap can own the kernel slot (the doc comment's
    // seed-window race). Exhaustion is unreachable (the real boot reaches here once, under
    // engine.rs's Once, claiming at most 2 of the 8 cells while the seam is still closed) but
    // stays an explicit Err so the tap can never install over an unseeded slot. If the install
    // below fails, this seed stays published with the gate closed — benign: nothing reads the
    // slot until the gate opens.
    let seed = bionic_action_from_glibc(&queried);
    if !tap_chain_store(seed) {
        return Err("early-fault tap: chain pool exhausted before seeding".to_string());
    }
    // (3) Install the tap kernel-first.
    // SAFETY: 2026-06-12 — all-zero is a valid glibc sigaction baseline (empty mask, no flags);
    // the handler pointer + flags are set before registering. `old` is a valid out-param glibc
    // fills with the previous kernel action on success.
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
    // (4) Re-seed ONLY if a re-registration raced between (1) and (3) — the install's oldact is
    // then the authoritative pre-tap action. Identical in every non-raced run, so no second
    // cell is claimed (pinned by the install phase of
    // `early_fault_tap_intercepts_registration_and_chains`).
    let displaced = bionic_action_from_glibc(&old);
    if displaced != seed && !tap_chain_store(displaced) {
        return Err("early-fault tap: chain pool exhausted on re-seed".to_string());
    }
    // Gate LAST (Release): the seam's Acquire load can never observe gate-open with an
    // empty/unseeded slot.
    TAPPED_SIGNAL.store(signum, Ordering::Release);
    Ok(())
}

/// Publish libroblox's mapped `[base, base+span)` so the tap's engine-PC filter can scope dumps
/// to engine faults. Called by `engine.rs` immediately after map/relocate/resolve — provably
/// before any engine instruction runs (constructors/`JNI_OnLoad` come after).
pub(super) fn publish_engine_text_range(base: u64, span: u64) {
    ENGINE_RANGE_BASE.store(base, Ordering::Relaxed);
    ENGINE_RANGE_SPAN.store(span, Ordering::Relaxed);
}

// =================================================================================================
// Eclipse-owned `sigaltstack` — pure forward + registration attribution (2026-06-12, core 1223806).
// =================================================================================================
//
// Bionic and glibc `stack_t` are layout-identical on x86-64 (`{void* ss_sp; int ss_flags;
// size_t ss_size}`), so the forward passes the caller's pointers through UNTRANSLATED. The native
// exists for observability: core 1223806 died to a kernel `force_sigsegv()` (NT_SIGINFO
// `si_code=128 SI_KERNEL, si_addr=0`) — the signal-frame write to the dying thread's
// registered-but-unwritable SA_ONSTACK altstack faulted, the disposition reset to SIG_DFL, and
// ZERO handler instructions ran (both crash reporters silent by construction). The altstack's
// OWNER was unprovable because engine `sigaltstack` calls passed through to host glibc invisibly.
// Every registration is now logged + ring-buffered with tid / ss_sp / ss_size / ss_flags and the
// CALLER's return address resolved against the loader module table — any recurrence names the
// registrant and the stack region. No logic change: a pure forward (no workaround, no behavior
// edit — the evidence standard found no Eclipse logic defect to fix).

extern "C" {
    /// The C wrapper (`src/loader/sigaltstack_shim.c`) — captures `__builtin_return_address(0)`
    /// (no stable-Rust spelling exists for the caller's return address; the established
    /// `liblog_shim.c` pattern) and tail-calls [`eclipse_sigaltstack_record`]. THIS is the
    /// address registered under the bionic import name `sigaltstack`.
    pub fn eclipse_sigaltstack(ss: *const libc::stack_t, old_ss: *mut libc::stack_t) -> c_int;
}

/// One observed `sigaltstack` registration (the core-1223806 attribution record).
#[derive(Clone, Debug)]
pub struct AltstackRegistration {
    /// The registering thread (raw `SYS_gettid`).
    pub tid: i64,
    /// The registered `ss_sp` (0 for an `SS_DISABLE` registration with a null sp).
    pub ss_sp: u64,
    /// The registered `ss_size`.
    pub ss_size: usize,
    /// The registered `ss_flags` (e.g. `SS_DISABLE`).
    pub ss_flags: c_int,
    /// The caller's return address (who called `sigaltstack`).
    pub caller: u64,
    /// The caller resolved as `"<module>+0x<off>"` (Eclipse module table first, host `dladdr`
    /// fallback); `None` if no module contains the address.
    pub caller_module: Option<String>,
}

/// Bounded history of the most recent registrations (engine threads re-register on every
/// create/exit cycle over a long session; the forensic consumer correlates the DYING tid's most
/// recent registration, so a recent-window ring is sufficient and never grows).
const ALTSTACK_LOG_CAP: usize = 64;

/// The ring (oldest dropped first) + a monotonic total so a wrap is visible.
static ALTSTACK_LOG: std::sync::Mutex<Vec<AltstackRegistration>> =
    std::sync::Mutex::new(Vec::new());
static ALTSTACK_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Snapshot the recent registrations (newest last) — the test pin + any future core triage hook.
#[must_use]
pub fn recent_altstack_registrations() -> Vec<AltstackRegistration> {
    ALTSTACK_LOG
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Total registrations observed since boot (monotonic; exceeds the ring length once it wraps).
#[must_use]
pub fn altstack_registration_total() -> u64 {
    ALTSTACK_TOTAL.load(Ordering::Relaxed)
}

/// Resolve a code address to `"<module>+0x<off>"`: the Eclipse loader module table first (engine
/// callers — the attribution core 1223806 lacked), host `dladdr` for host PCs.
fn describe_code_address(addr: u64) -> Option<String> {
    if let Some(s) = super::module_registry::describe_address(addr) {
        return Some(s);
    }
    // SAFETY: 2026-06-12 — zeroed Dl_info is a valid out-param; `dladdr` only writes it.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-12 — host dladdr over an arbitrary address is a query (no deref of `addr`).
    if unsafe { libc::dladdr(addr as *const c_void, &mut info) } != 0 && !info.dli_fname.is_null() {
        // SAFETY: 2026-06-12 — a nonzero dladdr return makes dli_fname a valid NUL-terminated
        // string owned by the host loader (never freed for a loaded module).
        let name = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
        let short = name.rsplit('/').next().unwrap_or(&name);
        return Some(format!(
            "{short}+{:#x}",
            addr.wrapping_sub(info.dli_fbase as u64)
        ));
    }
    None
}

/// The Rust callee behind [`eclipse_sigaltstack`]: **forward** (layout-identical `stack_t`),
/// then log + record the registration with caller attribution. Queries (`ss == NULL`) forward
/// silently — only kernel-state CHANGES are recorded. Failures log a warning (the kernel
/// rejected the registration; nothing changed). NOT async-signal-safe (allocates/locks) —
/// `sigaltstack` is thread-setup code, not handler code, on every observed engine path
/// (2026-06-12; crashpad's in-handler flow re-registers ACTIONS via `sigaction`, never stacks).
///
/// # Safety
/// `ss`/`old_ss` follow the public `sigaltstack(2)` contract (each null or valid); `caller` is
/// the shim-captured return address, used only as an integer.
#[no_mangle]
pub unsafe extern "C" fn eclipse_sigaltstack_record(
    ss: *const libc::stack_t,
    old_ss: *mut libc::stack_t,
    caller: *const c_void,
) -> c_int {
    // SAFETY: 2026-06-12 — pure forward of the caller's own pointers; bionic/glibc stack_t are
    // layout-identical on x86-64 (the section header note), so no translation is required.
    let ret = unsafe { libc::sigaltstack(ss, old_ss) };
    if ss.is_null() {
        return ret; // pure query — no kernel-state change to record.
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
    // SAFETY: 2026-06-12 — `ss` is non-null and was just accepted by the kernel, so it points at
    // a readable stack_t for the duration of the call.
    let stack = unsafe { *ss };
    // SAFETY: 2026-06-12 — raw SYS_gettid takes no arguments and cannot fail.
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

// =================================================================================================
// Eclipse-owned guard-paged alternate signal stack (2026-06-12 — core 866509).
// =================================================================================================

/// The measured stack cost of the deepest observed fatal-signal handler chain (core 866509,
/// 2026-06-12): Eclipse tap → libsigchain → ART `HandleUnexpectedSignalCommon` → `DumpNativeStack`
/// → `BacktraceMap::Create` → vendored libunwind, whose maps-parser frame alone was 76,816 bytes —
/// ~79.2 KiB total, 51.9 KiB PAST the 32 KiB stack it ran on. [`ALTSTACK_SIZE`] must dominate it.
pub const ALTSTACK_CHAIN_BUDGET: usize = 80 * 1024;

/// The usable size of Eclipse's alternate signal stack: 3× the measured worst chain
/// ([`ALTSTACK_CHAIN_BUDGET`]) — the 128–256 KiB headroom band the core-866509 analysis called
/// for. An overflow past it now lands on the PROT_NONE guard page (a clean fault), never on heap.
pub const ALTSTACK_SIZE: usize = 256 * 1024;

/// A successfully installed Eclipse-owned alternate signal stack (see
/// [`install_guarded_altstack`]). The mapping is `[guard_base, guard_base + mapping_len)`:
/// one PROT_NONE guard page at the bottom, then the `ss_size` usable stack.
#[derive(Debug, Clone, Copy)]
pub struct GuardedAltstack {
    /// `ss_sp` as registered with the kernel: `guard_base + page` (just above the guard page).
    pub ss_sp: u64,
    /// `ss_size` as registered: [`ALTSTACK_SIZE`].
    pub ss_size: usize,
    /// The full mapping's base = the PROT_NONE guard page, one host page below [`Self::ss_sp`].
    pub guard_base: u64,
    /// The full mapping length (guard page + stack) — what a `munmap` of the region would take.
    pub mapping_len: usize,
}

/// Install an Eclipse-owned, mmap'd, guard-paged alternate signal stack on the CALLING thread,
/// replacing whatever stack is currently registered (never freeing it — see below).
///
/// 2026-06-12 — root cause this exists for (core 866509): vendored ART's
/// `Thread::SetUpAlternateSignalStack` (`art/runtime/thread_linux.cc`, run by `Thread::Init` for
/// every attaching thread — including the process main thread inside `JNI_CreateJavaVM`)
/// registers a 32 KiB **glibc-heap** buffer (`new uint8_t[]`) with no guard page and live malloc
/// arena directly below. The fatal-SIGSEGV handler chain measured ~79.2 KiB
/// ([`ALTSTACK_CHAIN_BUDGET`]), so it plunged 51.9 KiB below `ss_sp`, zero-filling live heap —
/// surfacing as glibc's `malloc(): unaligned tcache chunk detected` SIGABRT that destroyed the
/// crash report mid-backtrace. This mmap'd replacement turns any future overflow into a clean
/// guard-page fault instead of silent heap corruption.
///
/// Install-point contract (verified against the vendored ART source, 2026-06-12):
/// - ART's `SetUpAlternateSignalStack` unconditionally overwrites the thread's current altstack
///   at attach, so this must be called AFTER `JNI_CreateJavaVM` on the main thread (the
///   `runtime::boot` wiring) for Eclipse's stack to be the one in effect on signal delivery.
/// - ART's `TearDownAlternateSignalStack` queries the CURRENT `ss_sp` and `delete[]`s it — it
///   does NOT tolerate a foreign pointer. It only runs when a thread detaches from the runtime;
///   Eclipse never destroys the VM or detaches the main thread (`runtime::Vm` has no `Drop`;
///   `DestroyJavaVM` is never called), so ART can never free this mapping. The 32 KiB ART buffer
///   this displaces is never freed either (a one-time, main-thread-only leak by design — freeing
///   a foreign `operator new[]` allocation from Rust would be unsound).
/// - ART-attached ENGINE threads still get ART's heap-backed stack at attach (ART overwrites any
///   pre-installed one, and Eclipse cannot hook the attach point) — a recorded limitation, not
///   fixable on the Eclipse side without modifying vendored ART. Threads with NO altstack are
///   already safe (delivery falls back to the glibc-guard-paged thread stack).
pub fn install_guarded_altstack() -> Result<GuardedAltstack, String> {
    let page = super::map::host_page_size() as usize;
    let mapping_len = page + ALTSTACK_SIZE;
    // Map the whole region PROT_NONE first, then open up the stack part — so the guard page is
    // never writable at any point. MAP_STACK marks the mapping's purpose (advisory on Linux).
    // SAFETY: 2026-06-12 — anonymous private mapping of a kernel-chosen address; no existing
    // memory is touched. A MAP_FAILED return is checked before use.
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
    // SAFETY: 2026-06-12 — `[ss_sp, ss_sp + ALTSTACK_SIZE)` lies inside the mapping just created
    // (its tail; the first page stays PROT_NONE as the guard). Page-aligned by construction.
    if unsafe {
        libc::mprotect(
            ss_sp as *mut c_void,
            ALTSTACK_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
        )
    } != 0
    {
        let e = std::io::Error::last_os_error();
        // SAFETY: 2026-06-12 — unmap exactly the mapping created above (nothing else references it).
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
    // SAFETY: 2026-06-12 — `ss` describes the writable region just mapped; a null `old_ss` is the
    // documented "don't report the previous stack" form. Registers for the calling thread only.
    if unsafe { libc::sigaltstack(&ss, std::ptr::null_mut()) } != 0 {
        let e = std::io::Error::last_os_error();
        // SAFETY: 2026-06-12 — as above; the kernel rejected the registration so nothing uses it.
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

// =================================================================================================
// bionic data OBJECTs — `__stack_chk_guard` (SSP guard word) and `__sF` (stdio FILE table).
// =================================================================================================

/// bionic's `__stack_chk_guard` — the stack-smashing-protector canary word the compiler reads on
/// function entry and verifies on return. bionic exports it as a `uintptr_t` data **object** (glibc
/// keeps its guard in TLS and does not export it under this name). **minimal-correct:** an
/// Eclipse-owned process-global guard word, initialized once to a non-trivial value with a zero low
/// byte (the SSP convention so a string overflow stopping at a NUL cannot trivially forge it).
static ECLIPSE_STACK_CHK_GUARD: AtomicUsize = AtomicUsize::new(0);

/// Initialize (once) and return the address of [`ECLIPSE_STACK_CHK_GUARD`] as the `__stack_chk_guard`
/// data symbol. Idempotent: a second call returns the same address with the same value.
fn eclipse_stack_chk_guard_addr() -> u64 {
    // A fixed-but-nontrivial constant is a correct, stable guard word for the loader path (no real
    // attacker model here). Low byte 0x00 per the SSP NUL-terminator-defense convention. Set once.
    let _ = ECLIPSE_STACK_CHK_GUARD.compare_exchange(
        0,
        0xff0a_55c3_0000_0000usize,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    std::ptr::addr_of!(ECLIPSE_STACK_CHK_GUARD) as u64
}

/// bionic's `__sF` — the stdio `FILE` table backing `stdin`/`stdout`/`stderr`. bionic's PUBLIC
/// pre-API-23 stdio ABI (AOSP NDK `<stdio.h>` + `<bits/struct_file.h>`) declares
/// `extern FILE __sF[]` — an array of **STRUCTS** (`struct __sFILE { char __private[152]; }` on
/// LP64, 8-aligned) — with `stdin == &__sF[0]`, `stdout == &__sF[1]`, `stderr == &__sF[2]`. A
/// bionic-compiled consumer therefore computes `__sF_base + i*152` (an interior address) and never
/// LOADS a pointer from the object.
///
/// 2026-06-12 — ROOT CAUSE (core dump 782252, gdb-verified end to end): Eclipse previously provided
/// `__sF` as a 24-byte array of three glibc `FILE*` POINTER VALUES. crashpad's bionic-compiled
/// logger computed `stderr = GOT(__sF) + 0x130` (= 2×152), which landed 280 bytes past that table
/// inside unrelated Rust statics; glibc `fputs` then read the garbage "stream"'s `_lock` field
/// (0xff) and faulted at `fputs+53` (si_addr 0x107 = 0xff+8) — killing the process INSIDE the crash
/// handler's own logging path. The premise the 2026-06-05 provision relied on ("a relocated
/// reference to `__sF[i]` yields a usable host stdio stream") was false: bionic code takes the
/// slot's ADDRESS, not its value.
///
/// **The fix (bionic-ABI-shaped):** `__sF` is now a zero-initialized 3×152-byte (456-byte)
/// Eclipse-owned backing, so `&__sF[0]`/`&__sF[1]`/`&__sF[2]` are deterministic SENTINEL addresses
/// at `base+0x000`/`+0x098`/`+0x130` that can never alias unrelated statics. Because glibc stdio
/// can only consume genuine glibc `FILE` objects, every FILE*-consuming stdio import of the `__sF`
/// importers is provided as an Eclipse translating native (see the "bionic stdio FILE* translation"
/// section below): [`eclipse_sf_translate_stream`] maps the three sentinels to the host glibc
/// streams and passes every other `FILE*` (a real `fopen`/`fdopen` return) through unchanged.
///
/// 2026-06-12: LP64 `sizeof(struct __sFILE)` per the public AOSP NDK `<bits/struct_file.h>`
/// (`char __private[152]`, `aligned(sizeof(void*))`). Pinned by
/// `sf_backing_is_bionic_shaped_three_structs`.
const SF_FILE_STRIDE: usize = 152;
/// `__sF[0..3)` — the three standard streams the bionic ABI publishes through `__sF`.
const SF_ENTRY_COUNT: usize = 3;
/// Total size of the bionic-shaped `__sF` backing: 3 × 152 = 456 bytes.
const SF_BACKING_LEN: usize = SF_FILE_STRIDE * SF_ENTRY_COUNT;

/// The bionic-shaped `__sF` backing object. The bytes are opaque (`struct __sFILE`'s fields are
/// private in the public ABI; no known importer pokes them directly — readelf audit 2026-06-12),
/// zero-initialized, and WRITABLE (bionic's real `__sF` lives in `.data`; `UnsafeCell` keeps this
/// static out of read-only `.rodata` so a hypothetical direct field write lands in Eclipse-owned
/// memory instead of faulting). Eclipse itself never reads or writes the bytes — only the ADDRESS
/// is meaningful (the sentinel match in [`eclipse_sf_translate_stream`]).
#[repr(C, align(8))]
struct SfBacking(UnsafeCell<[u8; SF_BACKING_LEN]>);
// SAFETY: 2026-06-12 — Eclipse never creates a Rust reference into the cell; the address is handed
// to foreign bionic code as an opaque data symbol. Any concurrent foreign access is to plain bytes
// Eclipse does not touch, so sharing the static across threads is sound.
unsafe impl Sync for SfBacking {}

/// The process-global `__sF` backing (see [`SfBacking`]).
static ECLIPSE_SF: SfBacking = SfBacking(UnsafeCell::new([0u8; SF_BACKING_LEN]));

extern "C" {
    // glibc's stdio handle data symbols (`extern FILE *stdin, *stdout, *stderr;`). The `libc` crate
    // does not re-export these statics, so bind them directly (the default symbol version links).
    // 2026-06-05.
    static stdin: *mut libc::FILE;
    static stdout: *mut libc::FILE;
    static stderr: *mut libc::FILE;
}

/// The address of the bionic-shaped `__sF` backing — the `__sF` data symbol's resolution.
fn eclipse_sf_addr() -> u64 {
    ECLIPSE_SF.0.get() as u64
}

/// Map a bionic `&__sF[i]` stream sentinel to the corresponding host glibc stream; pass every other
/// pointer (a real glibc `FILE*` from `fopen`/`fdopen`/the fall-through `stdin`/`stdout`/`stderr`
/// OBJECT imports — or null, e.g. `fflush(NULL)`) through unchanged. Exact-entry match only: bionic
/// code computes exactly `base + i*152`; an interior pointer is not a stream and passes through.
///
/// `#[no_mangle] extern "C"` because the C stdio shim (`src/loader/stdio_shim.c`) calls it too.
/// Safe to call with ANY pointer value — it only compares addresses, never dereferences.
#[no_mangle]
pub extern "C" fn eclipse_sf_translate_stream(stream: *mut libc::FILE) -> *mut libc::FILE {
    let base = ECLIPSE_SF.0.get() as usize;
    let p = stream as usize;
    if p == base {
        // SAFETY: 2026-06-12 — `stdin` is the process-global glibc stdio `FILE*` data symbol,
        // valid for the process lifetime; reading it is a plain pointer read of a stable global.
        unsafe { stdin }
    } else if p == base + SF_FILE_STRIDE {
        // SAFETY: 2026-06-12 — same contract as `stdin` above.
        unsafe { stdout }
    } else if p == base + 2 * SF_FILE_STRIDE {
        // SAFETY: 2026-06-12 — same contract as `stdin` above.
        unsafe { stderr }
    } else {
        stream
    }
}

// =================================================================================================
// bionic stdio FILE* translation (22 Rust + 3 C-shim) — every FILE*-consuming stdio import of the
// `__sF` importers, remapped through `eclipse_sf_translate_stream` then forwarded to glibc.
// =================================================================================================
//
// 2026-06-12: enumerated via `readelf --dyn-syms` over the five `__sF`-importing engine libs
// (libroblox.so, libbacktrace-native.so, libeigen_blas.so, librenderscript-toolkit.so,
// libzstd-jni-1.5.7-6.so, v2.721.1108): clearerr fclose feof ferror fflush fgets fileno fputc
// fputs fputwc fread __fread_chk fseek fseeko ftell ftello fwrite getc getwc setvbuf ungetc
// ungetwc (fixed-arity, Rust — this section) plus fprintf/fscanf (C-variadic) and vfprintf
// (va_list), DEFINED by the clean-room C shim `src/loader/stdio_shim.c` (Rust stable cannot define
// either shape — the liblog_shim.c pattern). `fopen`/`fdopen` only RETURN a `FILE*` (a real glibc
// stream the translator passes through) and need no native. `__fwrite_chk` (already Eclipse-owned)
// gained the same remap. Each native is **forward (stream-translated)**: glibc's routine is
// ABI-identical for real streams; the translation makes the bionic `&__sF[i]` sentinel a real
// stream first.

/// `void clearerr(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_clearerr(stream: *mut libc::FILE) {
    // SAFETY: 2026-06-12 — the translated stream is a genuine glibc FILE* (sentinel → host stream;
    // anything else is a valid glibc stream per the caller contract).
    unsafe { libc::clearerr(eclipse_sf_translate_stream(stream)) }
}

/// `int fclose(FILE* stream)` — **forward (stream-translated).** Closing a standard-stream sentinel
/// closes the corresponding host stream — exactly what the bionic caller asked for.
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid, open glibc `FILE*`.
unsafe extern "C" fn eclipse_fclose(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`; the stream is open per the caller contract.
    unsafe { libc::fclose(eclipse_sf_translate_stream(stream)) }
}

/// `int feof(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_feof(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::feof(eclipse_sf_translate_stream(stream)) }
}

/// `int ferror(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_ferror(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::ferror(eclipse_sf_translate_stream(stream)) }
}

/// `int fflush(FILE* stream)` — **forward (stream-translated).** A null stream passes through (the
/// documented "flush all open streams" contract).
///
/// # Safety
/// `stream` must be null, a bionic `&__sF[i]` sentinel, or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fflush(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`; null passes through untouched (glibc fflush(NULL)
    // flushes all streams per the public contract).
    unsafe { libc::fflush(eclipse_sf_translate_stream(stream)) }
}

/// `char* fgets(char* buf, int n, FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `buf` must point to at least `n` writable bytes; `stream` must be a bionic `&__sF[i]` sentinel
/// or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fgets(
    buf: *mut c_char,
    n: c_int,
    stream: *mut libc::FILE,
) -> *mut c_char {
    // SAFETY: 2026-06-12 — `buf`/`n` per the caller contract; see `eclipse_clearerr` for the stream.
    unsafe { libc::fgets(buf, n, eclipse_sf_translate_stream(stream)) }
}

/// `int fileno(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fileno(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::fileno(eclipse_sf_translate_stream(stream)) }
}

/// `int fputc(int c, FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fputc(c: c_int, stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::fputc(c, eclipse_sf_translate_stream(stream)) }
}

/// `int fputs(const char* s, FILE* stream)` — **forward (stream-translated).** THE call shape that
/// killed boot 782252: crashpad's `fputs(msg, &__sF[2])`.
///
/// # Safety
/// `s` must be a valid NUL-terminated C string; `stream` must be a bionic `&__sF[i]` sentinel or a
/// valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fputs(s: *const c_char, stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — `s` valid NUL-terminated per the caller contract; see `eclipse_clearerr`
    // for the stream.
    unsafe { libc::fputs(s, eclipse_sf_translate_stream(stream)) }
}

/// `size_t fread(void* buf, size_t size, size_t count, FILE* stream)` — **forward
/// (stream-translated).**
///
/// # Safety
/// `buf` must point to at least `size*count` writable bytes; `stream` must be a bionic `&__sF[i]`
/// sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fread(
    buf: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
) -> usize {
    // SAFETY: 2026-06-12 — `buf` sized per the caller contract; see `eclipse_clearerr`.
    unsafe { libc::fread(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

/// `int fseek(FILE* stream, long offset, int whence)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fseek(
    stream: *mut libc::FILE,
    offset: c_long,
    whence: c_int,
) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::fseek(eclipse_sf_translate_stream(stream), offset, whence) }
}

/// `int fseeko(FILE* stream, off_t offset, int whence)` — **forward (stream-translated).** bionic
/// LP64 `off_t` is 64-bit, same as glibc x86-64.
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fseeko(
    stream: *mut libc::FILE,
    offset: libc::off_t,
    whence: c_int,
) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::fseeko(eclipse_sf_translate_stream(stream), offset, whence) }
}

/// `long ftell(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_ftell(stream: *mut libc::FILE) -> c_long {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::ftell(eclipse_sf_translate_stream(stream)) }
}

/// `off_t ftello(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_ftello(stream: *mut libc::FILE) -> libc::off_t {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::ftello(eclipse_sf_translate_stream(stream)) }
}

/// `size_t fwrite(const void* buf, size_t size, size_t count, FILE* stream)` — **forward
/// (stream-translated).**
///
/// # Safety
/// `buf` must point to at least `size*count` readable bytes; `stream` must be a bionic `&__sF[i]`
/// sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fwrite(
    buf: *const c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
) -> usize {
    // SAFETY: 2026-06-12 — `buf` sized per the caller contract; see `eclipse_clearerr`.
    unsafe { libc::fwrite(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

/// `int getc(FILE* stream)` — **forward (stream-translated).** Forwards to glibc `fgetc`, which
/// ISO C defines as equivalent to `getc` (libc 0.2.186 binds only `fgetc` for linux-gnu).
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_getc(stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::fgetc(eclipse_sf_translate_stream(stream)) }
}

/// `int setvbuf(FILE* stream, char* buffer, int mode, size_t size)` — **forward
/// (stream-translated).**
///
/// # Safety
/// `buffer` must be null or point to at least `size` bytes valid for the stream's lifetime;
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_setvbuf(
    stream: *mut libc::FILE,
    buffer: *mut c_char,
    mode: c_int,
    size: usize,
) -> c_int {
    // SAFETY: 2026-06-12 — `buffer`/`mode`/`size` per the public setvbuf contract; see
    // `eclipse_clearerr` for the stream.
    unsafe { libc::setvbuf(eclipse_sf_translate_stream(stream), buffer, mode, size) }
}

/// `int ungetc(int c, FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_ungetc(c: c_int, stream: *mut libc::FILE) -> c_int {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { libc::ungetc(c, eclipse_sf_translate_stream(stream)) }
}

/// glibc/bionic `wint_t` — `unsigned int` on x86-64 in both ABIs. 2026-06-12: pinned locally
/// because libc 0.2.186 defines `wint_t` for teeos/hurd but NOT linux-gnu.
#[allow(non_camel_case_types)] // 2026-06-12: deliberately matches the public C type name it pins.
type wint_t = std::ffi::c_uint;

extern "C" {
    // glibc's wide-char stdio routines — libc 0.2.186 does not bind them, so declare them directly
    // (the public C signatures; bionic LP64 wchar_t/wint_t are 4 bytes, same as glibc x86-64).
    // 2026-06-12.
    fn fputwc(wc: libc::wchar_t, stream: *mut libc::FILE) -> wint_t;
    fn getwc(stream: *mut libc::FILE) -> wint_t;
    fn ungetwc(wc: wint_t, stream: *mut libc::FILE) -> wint_t;
}

/// `wint_t fputwc(wchar_t wc, FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fputwc(wc: libc::wchar_t, stream: *mut libc::FILE) -> wint_t {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`; glibc fputwc is the public wide-char write.
    unsafe { fputwc(wc, eclipse_sf_translate_stream(stream)) }
}

/// `wint_t getwc(FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_getwc(stream: *mut libc::FILE) -> wint_t {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { getwc(eclipse_sf_translate_stream(stream)) }
}

/// `wint_t ungetwc(wint_t wc, FILE* stream)` — **forward (stream-translated).**
///
/// # Safety
/// `stream` must be a bionic `&__sF[i]` sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_ungetwc(wc: wint_t, stream: *mut libc::FILE) -> wint_t {
    // SAFETY: 2026-06-12 — see `eclipse_clearerr`.
    unsafe { ungetwc(wc, eclipse_sf_translate_stream(stream)) }
}

/// `size_t __fread_chk(void* buf, size_t size, size_t count, FILE* stream, size_t buf_size)` —
/// bionic FORTIFY fread. Aborts if `size * count > buf_size`, else forwards to glibc `fread` with
/// the translated stream. **forward (stream-translated).**
///
/// 2026-06-12: glibc ALSO exports a `__fread_chk`, but with a DIFFERENT argument order
/// (`(ptr, ptrlen, size, n, stream)` per glibc `bits/stdio2.h` — the object bound is the SECOND
/// argument and the stream is LAST), so the previous host fall-through was shape-mismatched on
/// every argument after the first — the same flawed-pattern class as the `__sF` data object
/// (a bionic name resolved to a glibc symbol of a different shape).
///
/// # Safety
/// `buf` must point to at least `buf_size` writable bytes; `stream` must be a bionic `&__sF[i]`
/// sentinel or a valid glibc `FILE*`.
unsafe extern "C" fn eclipse_fread_chk(
    buf: *mut c_void,
    size: usize,
    count: usize,
    stream: *mut libc::FILE,
    buf_size: usize,
) -> usize {
    // bionic aborts if the total bytes (`size * count`) would overflow the destination object.
    match size.checked_mul(count) {
        Some(t) if t <= buf_size => {}
        _ => std::process::abort(),
    }
    // SAFETY: 2026-06-12 — after the check, `buf` has ≥ `size*count` writable bytes; the translated
    // stream is a genuine glibc FILE* (see `eclipse_clearerr`).
    unsafe { libc::fread(buf, size, count, eclipse_sf_translate_stream(stream)) }
}

extern "C" {
    /// `int fprintf(FILE* stream, const char* fmt, ...)` — DEFINED in the C stdio shim
    /// (`src/loader/stdio_shim.c`): remaps the stream via [`eclipse_sf_translate_stream`], then
    /// glibc `vfprintf`. Variadic externs are stable to declare; the address is taken in
    /// [`EclipseNativeProvider::with_bionic_natives`]. 2026-06-12.
    fn eclipse_fprintf(stream: *mut libc::FILE, fmt: *const c_char, ...) -> c_int;

    /// `int fscanf(FILE* stream, const char* fmt, ...)` — DEFINED in the C stdio shim (remap +
    /// glibc `vfscanf`). Address-only use here. 2026-06-12.
    fn eclipse_fscanf(stream: *mut libc::FILE, fmt: *const c_char, ...) -> c_int;

    /// `int vfprintf(FILE* stream, const char* fmt, va_list ap)` — DEFINED in the C stdio shim
    /// (remap + glibc `vfprintf`). 2026-06-12: `va_list` has no stable Rust spelling; in the
    /// x86-64 SysV ABI a `va_list` parameter is a pointer (`__va_list_tag*`), so the declaration
    /// is ABI-accurate — and it is ADDRESS-ONLY here (never called from Rust).
    fn eclipse_vfprintf(stream: *mut libc::FILE, fmt: *const c_char, ap: *mut c_void) -> c_int;
}

// =================================================================================================
// bionic netdb resolver ABI (4) — glibc HAS these names but `struct addrinfo`'s tail field ORDER
// and the AI_/EAI_/NI_ constant VALUES diverge. **translate.**
// =================================================================================================
//
// 2026-06-12 (the engine `HttpError:DnsResolve` root cause; closes the AGENTS.md §6 core-1223806
// resolver-ABI reservation): libroblox.so + libbacktrace-native.so import plain POSIX
// `getaddrinfo`/`freeaddrinfo`/`gai_strerror`/`getnameinfo` (+ `gethostbyname`) `@LIBC`
// (nm-re-verified; NO `android_getaddrinfofornet`/`android_res_*` — netd ruled out; curl's
// threaded-resolver failf string present, zero `ares_*` — a bundled c-ares ruled out). Eclipse
// provided none, so the bionic-compiled callers ran against host glibc:
//   * `struct addrinfo`: offsets 0–16 (flags/family/socktype/protocol/addrlen) are identical,
//     but the tail is SWAPPED — bionic (BSD order; public AOSP `libc/include/netdb.h`,
//     re-fetched 2026-06-12) has `ai_canonname`@24 / `ai_addr`@32, glibc (host
//     /usr/include/netdb.h) has `ai_addr`@24 / `ai_canonname`@32 (`ai_next`@40 in both). A
//     bionic-compiled walker over glibc nodes reads the canonname slot (NULL on effectively
//     every node) as `ai_addr` → zero usable addresses → curl `CURLE_COULDNT_RESOLVE_HOST` —
//     the logged `Could not resolve host`, deterministic under every flag combination (0
//     engine-resolver successes across all validation logs while the SAME process's
//     Java/okhttp/wolfSSL path did real Roblox HTTPS round-trips).
//   * `AI_*` values alias ACROSS libcs: bionic `AI_ADDRCONFIG` (0x400) == glibc
//     `AI_NUMERICSERV`; bionic 0x100/0x200/0x800 (`AI_ALL`/`AI_V4MAPPED_CFG`/`AI_V4MAPPED`)
//     mean other things (or nothing) to glibc → translated BY NAME, never passed raw.
//   * `EAI_*` codes sign-flip AND renumber (bionic positive 1..15, glibc negative) — also what
//     Roblox's own `EAI_AGAIN` retry classification reads (RbxTransport strings).
//   * `NI_*` low bits scramble (bionic NOFQDN=1/NUMERICHOST=2/NAMEREQD=4/NUMERICSERV=8 vs glibc
//     NUMERICHOST=1/NUMERICSERV=2/NOFQDN=4/NAMEREQD=8; DGRAM=16 in both).
// `gethostbyname` stays on the host baseline — bionic and glibc `struct hostent` field order is
// IDENTICAL (h_name/h_aliases/h_addrtype/h_length/h_addr_list; both headers read 2026-06-12), as
// are `inet_ntop`/`inet_pton` (record-only; no native needed).

/// Bionic LP64 `struct addrinfo` (public AOSP `netdb.h`, BSD field order). The head (offsets
/// 0–16) matches glibc; the TAIL is the swap: `ai_canonname`@24, `ai_addr`@32 (glibc has them
/// reversed). Pinned by `bionic_addrinfo_layout_is_bsd_order_and_differs_from_glibc`.
#[repr(C)]
struct BionicAddrinfo {
    /// `int ai_flags` — BIONIC `AI_*` values (translated, never forwarded raw).
    ai_flags: c_int,
    /// `int ai_family` — `AF_*` (shared kernel ABI; identical values).
    ai_family: c_int,
    /// `int ai_socktype` — `SOCK_*` (shared kernel ABI).
    ai_socktype: c_int,
    /// `int ai_protocol` — `IPPROTO_*` (shared kernel ABI).
    ai_protocol: c_int,
    /// `socklen_t ai_addrlen` (+4 bytes padding to the first pointer).
    ai_addrlen: libc::socklen_t,
    /// `char* ai_canonname` — @24 (glibc keeps `ai_addr` here).
    ai_canonname: *mut c_char,
    /// `struct sockaddr* ai_addr` — @32 (glibc keeps `ai_canonname` here).
    ai_addr: *mut libc::sockaddr,
    /// `struct addrinfo* ai_next` — @40 in both ABIs.
    ai_next: *mut BionicAddrinfo,
}

// Bionic `EAI_*` (POSITIVE; public AOSP `netdb.h`, re-fetched 2026-06-12).
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

/// glibc's GNU-extension `EAI_ADDRFAMILY` (host /usr/include/netdb.h: `-9`) — the `libc` crate
/// does not export it for linux-gnu, so it is pinned locally (2026-06-12).
const GLIBC_EAI_ADDRFAMILY: c_int = -9;

/// `(bionic value, glibc value)` `AI_*` pairs, translated BY NAME. Bionic values from the public
/// AOSP `netdb.h`; glibc values via the `libc` crate (matching the host header). Bionic
/// `AI_V4MAPPED_CFG` (0x200, "accept IPv4-mapped if kernel supports") maps to glibc `AI_V4MAPPED`
/// — the closest documented semantic (Linux always supports v4-mapped addresses).
const AI_FLAG_PAIRS: &[(c_int, c_int)] = &[
    (0x0001, libc::AI_PASSIVE),
    (0x0002, libc::AI_CANONNAME),
    (0x0004, libc::AI_NUMERICHOST),
    (0x0008, libc::AI_NUMERICSERV),
    (0x0100, libc::AI_ALL),
    (0x0200, libc::AI_V4MAPPED), // bionic AI_V4MAPPED_CFG
    (0x0400, libc::AI_ADDRCONFIG),
    (0x0800, libc::AI_V4MAPPED),
];

/// `(bionic value, glibc value)` `NI_*` pairs, translated BY NAME (values per the two headers).
const NI_FLAG_PAIRS: &[(c_int, c_int)] = &[
    (0x0001, libc::NI_NOFQDN),
    (0x0002, libc::NI_NUMERICHOST),
    (0x0004, libc::NI_NAMEREQD),
    (0x0008, libc::NI_NUMERICSERV),
    (0x0010, libc::NI_DGRAM),
];

/// Translate a bionic flag word to glibc through a by-name pair table. A bit outside the table is
/// `Err(`[`BIONIC_EAI_BADFLAGS`]`)` — both libcs reject undefined flag bits rather than guessing.
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

/// Translate a glibc `EAI_*` return (negative) to the bionic code of the SAME NAME (positive).
/// `0` stays success; an unmapped/newer glibc code becomes the generic non-recoverable
/// [`BIONIC_EAI_FAIL`] (never silently positive-but-meaningless).
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

/// Deep-copy ONE glibc `addrinfo` node into a single Eclipse-owned `malloc` block laid out as
/// `[BionicAddrinfo][sockaddr bytes][canonname bytes]` — so [`eclipse_freeaddrinfo`] frees each
/// node with exactly one `free`. `bionic_flags` (the caller's original hints word) is what the
/// result nodes report back (POSIX leaves result `ai_flags` unspecified; round-tripping the
/// caller's own bionic value can never hand it a glibc-valued word). Returns null on `malloc`
/// failure (the caller unwinds the partial chain).
///
/// # Safety
/// `g` must be a node of a live chain returned by host glibc `getaddrinfo` (its `ai_addr` has
/// `ai_addrlen` readable bytes; `ai_canonname` is null or a NUL-terminated C string).
unsafe fn bionic_node_from_glibc(g: &libc::addrinfo, bionic_flags: c_int) -> *mut BionicAddrinfo {
    let addr_len = if g.ai_addr.is_null() {
        0
    } else {
        g.ai_addrlen as usize
    };
    let canon_len = if g.ai_canonname.is_null() {
        0
    } else {
        // SAFETY: per the glibc getaddrinfo contract, a non-null ai_canonname is a NUL-terminated
        // C string owned by the live chain.
        unsafe { std::ffi::CStr::from_ptr(g.ai_canonname) }
            .to_bytes_with_nul()
            .len()
    };
    let header = std::mem::size_of::<BionicAddrinfo>();
    let total = header + addr_len + canon_len;
    // SAFETY: `total` ≥ 48; malloc returns a suitably-aligned block or null (handled below). The
    // sockaddr lands at offset 48 (16-aligned block ⇒ 8-aligned slot — over-aligned for sockaddr).
    let block = unsafe { libc::malloc(total) }.cast::<u8>();
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let addr_ptr = if addr_len > 0 {
        // SAFETY: the block has `total` writable bytes; the source has `addr_len` readable bytes
        // (caller contract); the ranges cannot overlap (fresh allocation).
        unsafe {
            std::ptr::copy_nonoverlapping(g.ai_addr.cast::<u8>(), block.add(header), addr_len);
            block.add(header).cast::<libc::sockaddr>()
        }
    } else {
        std::ptr::null_mut()
    };
    let canon_ptr = if canon_len > 0 {
        // SAFETY: as above — `canon_len` includes the NUL; destination range is inside the block.
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
    // SAFETY: `node` is the start of the fresh block, valid + aligned for one BionicAddrinfo.
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

/// `int getaddrinfo(const char* node, const char* service, const struct addrinfo* hints,
/// struct addrinfo** res)` — bionic-shaped resolver. **translate:** hints `AI_*` by name (head
/// fields are layout-identical, so they are read field-wise and rebuilt — the swapped tail is
/// never touched in the hints), forward to host glibc, deep-copy the result chain into
/// Eclipse-owned BIONIC-shaped nodes ([`bionic_node_from_glibc`]), translate the `EAI_*` return
/// to bionic-positive. The tracing line records node/service/flags/family + outcome — the
/// attribution diagnostic AGENTS.md reserved (it names the engine's ACTUAL resolver arguments).
///
/// # Safety
/// `node`/`service` must each be null or valid C strings; `hints` null or a valid bionic
/// `addrinfo`; `res` a valid out-pointer (POSIX caller contract).
unsafe extern "C" fn eclipse_getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const BionicAddrinfo,
    res: *mut *mut BionicAddrinfo,
) -> c_int {
    // For the trace only — lossy, never dereferenced beyond the C-string contract.
    let describe = |p: *const c_char| -> String {
        if p.is_null() {
            "<null>".to_owned()
        } else {
            // SAFETY: non-null ⇒ a valid NUL-terminated C string per this fn's caller contract.
            unsafe { std::ffi::CStr::from_ptr(p) }
                .to_string_lossy()
                .into_owned()
        }
    };
    if res.is_null() {
        // A null out-pointer violates POSIX; answer with the bionic system-error code instead of
        // faulting. errno carries the detail per the EAI_SYSTEM contract.
        // SAFETY: __errno_location() is the calling thread's errno slot (always valid).
        unsafe { *libc::__errno_location() = libc::EINVAL };
        return BIONIC_EAI_SYSTEM;
    }
    let (bionic_flags, g_hints) = if hints.is_null() {
        (0, None)
    } else {
        // SAFETY: `hints` is a valid bionic addrinfo (caller contract); only the head fields
        // (layout-identical offsets 0–16) are read — the swapped tail pointers are not consulted
        // (POSIX requires them null in hints anyway).
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
        // SAFETY: an all-zero glibc addrinfo is the documented empty-hints baseline.
        let mut g: libc::addrinfo = unsafe { std::mem::zeroed() };
        g.ai_flags = g_flags;
        g.ai_family = b.ai_family;
        g.ai_socktype = b.ai_socktype;
        g.ai_protocol = b.ai_protocol;
        (b.ai_flags, Some(g))
    };

    let mut g_res: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: node/service are null-or-valid C strings (caller contract); the hints are a valid
    // glibc-shaped struct built above (or null); `g_res` is a valid out-pointer.
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
        // Save/restore errno around the trace: EAI_SYSTEM callers read errno after return.
        // SAFETY: __errno_location() is the calling thread's errno slot (always valid).
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
        // SAFETY: as above.
        unsafe { *libc::__errno_location() = saved_errno };
        return eai;
    }

    // Deep-copy the glibc chain into bionic-shaped Eclipse-owned nodes.
    let mut head: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut tail: *mut BionicAddrinfo = std::ptr::null_mut();
    let mut count = 0u32;
    let mut cursor = g_res;
    while !cursor.is_null() {
        // SAFETY: `cursor` walks the live glibc chain returned above.
        let g = unsafe { &*cursor };
        // SAFETY: `g` is a live glibc node — exactly bionic_node_from_glibc's contract.
        let bionic_node = unsafe { bionic_node_from_glibc(g, bionic_flags) };
        if bionic_node.is_null() {
            // malloc failure: unwind BOTH chains, report the bionic memory code.
            // SAFETY: `head` is the (possibly empty) chain of nodes THIS call allocated.
            unsafe { eclipse_freeaddrinfo(head) };
            // SAFETY: `g_res` is the live glibc chain; glibc's freeaddrinfo owns it.
            unsafe { libc::freeaddrinfo(g_res) };
            return BIONIC_EAI_MEMORY;
        }
        if head.is_null() {
            head = bionic_node;
        } else {
            // SAFETY: `tail` is the previous Eclipse-owned node (non-null once head is set).
            unsafe { (*tail).ai_next = bionic_node };
        }
        tail = bionic_node;
        count += 1;
        cursor = g.ai_next;
    }
    // SAFETY: the glibc chain is fully copied; return it to glibc's allocator.
    unsafe { libc::freeaddrinfo(g_res) };
    // SAFETY: `res` is a valid out-pointer (checked non-null above).
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

/// `void freeaddrinfo(struct addrinfo* ai)` — free an Eclipse-owned bionic chain.
///
/// Frees ECLIPSE's own malloc'd nodes ONLY (every chain a bionic caller holds came from
/// [`eclipse_getaddrinfo`]); NEVER forwards to glibc — these are not glibc nodes, and glibc's
/// `freeaddrinfo` walking them (field offsets swapped, foreign allocation layout) would corrupt
/// the heap. Null is the documented no-op.
///
/// # Safety
/// `head` must be null or a chain returned by [`eclipse_getaddrinfo`] (each node one `malloc`
/// block), not yet freed.
unsafe extern "C" fn eclipse_freeaddrinfo(head: *mut BionicAddrinfo) {
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a live Eclipse-owned node (caller contract); ai_next is read before
        // the node's single backing block is freed.
        let next = unsafe { (*cursor).ai_next };
        // SAFETY: each node is exactly one malloc block (bionic_node_from_glibc), freed once.
        unsafe { libc::free(cursor.cast()) };
        cursor = next;
    }
}

/// `const char* gai_strerror(int ecode)` — static message table keyed by BIONIC-positive codes
/// (the values [`eclipse_getaddrinfo`]/[`eclipse_getnameinfo`] return). **minimal-correct:**
/// stable process-lifetime strings; glibc's table (keyed by ITS negative codes) would answer
/// "Unknown error" — or worse, a wrong message — for every bionic code.
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
        12 => b"invalid value for hints\0", // bionic EAI_BADHINTS
        13 => b"resolved protocol is unknown\0", // bionic EAI_PROTOCOL
        BIONIC_EAI_OVERFLOW => b"argument buffer overflow\0",
        _ => b"unknown error\0",
    };
    msg.as_ptr().cast()
}

/// `int getnameinfo(const struct sockaddr*, socklen_t, char* host, socklen_t, char* serv,
/// socklen_t, int flags)` — reverse lookup. **translate (flags + return only):** `sockaddr`/
/// `socklen_t` are layout-identical on Linux x86-64 (shared kernel ABI), so the call passes
/// through; the bionic `NI_*` word is translated by name and the glibc `EAI_*` return mapped to
/// bionic-positive. Undefined bionic bits → [`BIONIC_EAI_BADFLAGS`] without touching the host.
///
/// # Safety
/// Standard `getnameinfo` caller contract: `sa` valid for `salen` bytes; `host`/`serv` null or
/// writable for their stated lengths.
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
    // SAFETY: pure pass-through of the caller's pointers under the identical Linux sockaddr ABI;
    // only the flag word was rewritten.
    let rc = unsafe { libc::getnameinfo(sa, salen, host, hostlen, serv, servlen, g_flags) };
    bionic_eai_from_glibc(rc)
}

// =================================================================================================
// ndk-android (libandroid) — the 28 NDK natives. Opaque NDK pointers are Eclipse-owned generational
// registry handles ([`super::ndk_registry`]) cast to `*mut T`, so a stale/fabricated handle is a
// typed `Err` → NDK sentinel (NULL / negative), never a wild dereference / UB.
// =================================================================================================

// ---- shared handle <-> opaque-pointer casts -----------------------------------------------------

/// Cast an Eclipse [`ndk_registry::NdkHandle`] to the opaque NDK `*mut T` returned to C. The handle's
/// generation is ≥ 1, so a live handle is never NULL.
fn handle_to_ptr<T>(h: ndk_registry::NdkHandle) -> *mut T {
    h as usize as *mut T
}

/// Cast an opaque NDK `*const T`/`*mut T` from C back to an Eclipse [`ndk_registry::NdkHandle`].
fn ptr_to_handle<T>(p: *const T) -> ndk_registry::NdkHandle {
    p as usize as ndk_registry::NdkHandle
}

// ---- device defaults (minimal-correct AConfiguration / sound ANativeWindow geometry) ------------
//
// 2026-06-05: sane defaults for a generic portrait phone surface until a real device-config / live
// winit-window geometry source is wired (the winit window does not exist at engine-init time). These
// are documented constants, not magic: a 1080x1920 portrait display at xhdpi (320). dp = px*160/dpi.

/// `ACONFIGURATION_DENSITY_MEDIUM` baseline dpi (1 dp == 1 px). From `<android/configuration.h>`.
const ACONFIGURATION_DENSITY_BASELINE: i32 = 160;
/// `ACONFIGURATION_DENSITY_XHIGH` — Eclipse's default display density (xhdpi).
const ACONFIGURATION_DENSITY_XHIGH: i32 = 320;
/// `ACONFIGURATION_ORIENTATION_PORT` — portrait. From `<android/configuration.h>`.
const ACONFIGURATION_ORIENTATION_PORT: i32 = 1;
/// `ACONFIGURATION_SCREENSIZE_NORMAL` — a normal-size screen. From `<android/configuration.h>`.
const ACONFIGURATION_SCREENSIZE_NORMAL: i32 = 2;
/// `ACONFIGURATION_NAVHIDDEN_YES` — no exposed hardware nav keys (a touchscreen surface).
const ACONFIGURATION_NAVHIDDEN_YES: i32 = 2;
/// Default display width in pixels (portrait phone).
const DEFAULT_DISPLAY_WIDTH_PX: i32 = 1080;
/// Default display height in pixels (portrait phone).
const DEFAULT_DISPLAY_HEIGHT_PX: i32 = 1920;
/// `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM` / `WINDOW_FORMAT_RGBA_8888` = 1 — the default surface
/// format. From `<android/hardware_buffer.h>` / `<android/native_window.h>`.
const WINDOW_FORMAT_RGBA_8888: i32 = 1;

/// Eclipse's default [`ConfigurationState`]: xhdpi portrait phone (see the device-default constants).
/// dp = px * 160 / dpi, so at 320 dpi the 1080x1920 display is 540x960 dp.
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

/// The [`NativeWindowState`] an `ANativeWindow_*` handle reports: the **real live geometry** of
/// Eclipse's engine window when the run/test path has published it ([`ndk_registry::
/// engine_window_geometry`] — the same window the engine's EGL surface presents to, see
/// [`crate::egl_engine`]), else the documented portrait default (the window does not exist yet at
/// engine-init time). 2026-06-05: this is the bind from the engine's `ANativeWindow` to Eclipse's
/// real window — the geometry getters now answer with Eclipse's actual window size, not a fixed
/// phone default, so the engine sizes its framebuffer correctly.
fn default_native_window() -> NativeWindowState {
    let (width, height) = ndk_registry::engine_window_geometry()
        .unwrap_or((DEFAULT_DISPLAY_WIDTH_PX, DEFAULT_DISPLAY_HEIGHT_PX));
    NativeWindowState {
        width,
        height,
        format: WINDOW_FORMAT_RGBA_8888,
    }
}

// ---- AAsset / AAssetManager (6) — REAL: route to Eclipse's own `src/apk` reader -----------------

/// The APK zip prefix Android assets live under (`AAssetManager_open("foo")` → `assets/foo`).
const ASSET_ENTRY_PREFIX: &str = "assets/";

/// `AAssetManager* AAssetManager_fromJava(JNIEnv* env, jobject assetManager)` — obtain a native asset
/// manager from a Java `AssetManager`. **real:** Eclipse owns the asset backing; it serves assets from
/// the APK path the boot path configured via [`ndk_registry::set_apk_path`] (the opaque JNI args
/// cannot yield the APK without the cyber-safeguarded framework code). Returns an Eclipse
/// `AAssetManager*` handle, or NULL if no APK path is configured (a sound "no source", not a fake).
///
/// # Safety
/// `env`/`asset_manager` are the JNI args; this native does not dereference them (Eclipse derives the
/// asset source from its own configured APK path), so any pointer value is accepted safely.
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
        None => std::ptr::null_mut(), // no configured APK → no asset source (sound, not a fake)
    }
}

/// `AAsset* AAssetManager_open(AAssetManager* mgr, const char* filename, int mode)` — open an asset
/// for reading. **real:** reads the `assets/<filename>` zip entry's real bytes via Eclipse's
/// [`crate::apk::Apk`] reader and stores them in an owned `AAsset*` handle. Returns NULL if the
/// manager handle is stale/fabricated, the APK cannot be opened, or the entry is absent — the bionic
/// contract for a missing asset (the caller checks for NULL). `mode` (BUFFER/RANDOM/STREAMING) is
/// advisory; Eclipse always buffers the whole entry (the assets here are small config/XML files).
///
/// # Safety
/// `mgr` must be an `AAssetManager*` previously returned by an Eclipse asset native (or NULL/garbage,
/// which is rejected by the generation check); `filename` must be a valid NUL-terminated C string.
unsafe extern "C" fn eclipse_aassetmanager_open(
    mgr: *mut c_void,
    filename: *const c_char,
    _mode: c_int,
) -> *mut c_void {
    // SAFETY: 2026-06-05 — `filename` is the public C-string arg (null-or-valid NUL-terminated).
    let Some(name) = (unsafe { cstr_opt(filename) }) else {
        return std::ptr::null_mut();
    };
    // Look up the manager's APK path (stale/fabricated handle → Err → NULL, never a deref).
    let apk_path =
        match ndk_registry::asset_managers().with(ptr_to_handle(mgr), |m| m.apk_path.clone()) {
            Ok(p) => p,
            Err(_) => return std::ptr::null_mut(),
        };
    // Read the real bytes of `assets/<name>` through Eclipse's own benign APK reader.
    let entry = format!("{ASSET_ENTRY_PREFIX}{name}");
    let bytes = match crate::apk::Apk::open(&apk_path).and_then(|mut a| a.read_entry(&entry)) {
        Ok(b) => b,
        Err(_) => return std::ptr::null_mut(), // missing/unreadable asset → NULL (bionic contract)
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

/// `void AAsset_close(AAsset* asset)` — free an asset opened by [`eclipse_aassetmanager_open`].
/// **real:** frees the owned-handle slot (and its bytes). A stale/double-close handle is a typed
/// `Err` that is ignored (closing an invalid handle is a harmless no-op, never UB).
///
/// # Safety
/// `asset` must be an `AAsset*` from an Eclipse asset native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aasset_close(asset: *mut c_void) {
    let _ = ndk_registry::assets().remove(ptr_to_handle(asset));
}

/// `const void* AAsset_getBuffer(AAsset* asset)` — get a pointer to the whole asset contents.
/// **real:** returns a pointer to the owned bytes (stable for the asset's lifetime — see
/// [`ndk_registry`]'s pointer-stability note). Returns NULL for a stale/fabricated handle (bionic
/// returns NULL on failure).
///
/// # Safety
/// `asset` must be an `AAsset*` from an Eclipse asset native (or garbage, which is rejected). The
/// returned pointer is valid until [`eclipse_aasset_close`] of the same handle.
unsafe extern "C" fn eclipse_aasset_getbuffer(asset: *mut c_void) -> *const c_void {
    // SAFETY of the returned pointer: 2026-06-05 — `bytes` is a `Box<[u8]>` whose contents have a
    // stable heap address that does not move while the slot lives (read-only after open, never
    // re-allocated); the slot is freed only by `AAsset_close`. So the pointer stays valid for the
    // asset's lifetime, exactly the `AAsset_getBuffer` contract. Returning the address out of the
    // lock is sound because nothing mutates or moves the bytes.
    match ndk_registry::assets().with(ptr_to_handle(asset), |a| a.bytes.as_ptr() as *const c_void) {
        Ok(p) => p,
        Err(_) => std::ptr::null(),
    }
}

/// `off_t AAsset_getLength(AAsset* asset)` — the asset's total length in bytes. **real:** returns
/// `bytes.len()`. Returns 0 for a stale/fabricated handle (a sound "empty", not a wrong non-zero).
///
/// # Safety
/// `asset` must be an `AAsset*` from an Eclipse asset native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aasset_getlength(asset: *mut c_void) -> libc::off_t {
    match ndk_registry::assets().with(ptr_to_handle(asset), |a| a.bytes.len()) {
        Ok(n) => libc::off_t::try_from(n).unwrap_or(libc::off_t::MAX),
        Err(_) => 0,
    }
}

/// `int AAsset_openFileDescriptor(AAsset* asset, off_t* outStart, off_t* outLength)` — get a real file
/// descriptor for direct asset access. 2026-06-14: real Android returns the APK fd + the uncompressed
/// asset's offset here so a caller can `mmap` it directly; Roblox's engine uses THIS path for the large
/// (STORED, 8–18 MB) shader packs and treats a `< 0` return as `Error opening shader pack <path>`
/// rather than falling back to `AAsset_getBuffer`. Eclipse holds the asset's decompressed bytes in
/// memory, so back them with an anonymous in-memory file (`memfd`) and return its fd + `[0, len)`. The
/// caller owns and closes the returned fd. Falls back to the documented `-1` (→ buffer path) only if
/// the handle is stale or a syscall fails.
///
/// # Safety
/// `asset` must be an `AAsset*` from an Eclipse asset native; `out_start`/`out_length` are null or
/// valid `off_t*` (written only on success). The returned fd is owned by the caller.
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
    // SAFETY: 2026-06-14 — `memfd_create` with a valid NUL-terminated name + 0 flags returns a fresh
    // owned fd or -1; on success we size it (`ftruncate`), write exactly `len` bytes from the owned
    // slice, rewind to 0, and hand the fd to the caller (its contract). Every libc call is checked;
    // any failure closes the fd (if opened) and returns the documented -1 (caller uses the buffer
    // path). `out_start`/`out_length` are written only via the null-checked pointers.
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

// ---- AConfiguration (9) — MINIMAL-CORRECT: real getters over Eclipse device values --------------

/// `AConfiguration* AConfiguration_new(void)` — create a configuration object. **minimal-correct:**
/// allocates an Eclipse `AConfiguration*` handle holding [`default_configuration`] (sane device
/// values), or NULL on registry exhaustion.
extern "C" fn eclipse_aconfiguration_new() -> *mut c_void {
    match ndk_registry::configurations().insert(default_configuration()) {
        Ok(h) => handle_to_ptr(h),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `void AConfiguration_delete(AConfiguration* config)` — free a configuration. **minimal-correct:**
/// frees the owned-handle slot; a stale handle is a harmless ignored `Err`.
///
/// # Safety
/// `config` must be an `AConfiguration*` from an Eclipse native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aconfiguration_delete(config: *mut c_void) {
    let _ = ndk_registry::configurations().remove(ptr_to_handle(config));
}

/// `void AConfiguration_fromAssetManager(AConfiguration* out, AAssetManager* am)` — fill `out` with
/// the configuration in use by the asset manager. **minimal-correct:** Eclipse has one device
/// configuration, so it copies [`default_configuration`] into the existing `out` handle (the engine
/// passes an `AConfiguration*` it got from `AConfiguration_new`). A stale `out` handle is ignored.
///
/// # Safety
/// `out` must be an `AConfiguration*` from [`eclipse_aconfiguration_new`]; `am` is unused (Eclipse's
/// config is manager-independent here) and may be any value.
unsafe extern "C" fn eclipse_aconfiguration_fromassetmanager(out: *mut c_void, _am: *mut c_void) {
    let _ =
        ndk_registry::configurations().with(ptr_to_handle(out), |c| *c = default_configuration());
}

/// `void AConfiguration_getCountry(AConfiguration* config, char* outCountry)` — write the 2-char
/// country code (no NUL). **minimal-correct.**
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*`; `out_country` must point to ≥ 2 writable bytes (the
/// `AConfiguration_getCountry` contract). On a stale handle nothing is written.
unsafe extern "C" fn eclipse_aconfiguration_getcountry(
    config: *mut c_void,
    out_country: *mut c_char,
) {
    if out_country.is_null() {
        return;
    }
    if let Ok(country) = ndk_registry::configurations().with(ptr_to_handle(config), |c| c.country) {
        // SAFETY: 2026-06-05 — the public contract guarantees `out_country` has ≥ 2 writable bytes;
        // we write exactly the 2 country chars (no NUL, per the NDK contract).
        unsafe {
            out_country.write(country[0] as c_char);
            out_country.add(1).write(country[1] as c_char);
        }
    }
}

/// `void AConfiguration_getLanguage(AConfiguration* config, char* outLanguage)` — write the 2-char
/// language code (no NUL). **minimal-correct.**
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*`; `out_language` must point to ≥ 2 writable bytes.
unsafe extern "C" fn eclipse_aconfiguration_getlanguage(
    config: *mut c_void,
    out_language: *mut c_char,
) {
    if out_language.is_null() {
        return;
    }
    if let Ok(language) = ndk_registry::configurations().with(ptr_to_handle(config), |c| c.language)
    {
        // SAFETY: 2026-06-05 — the public contract guarantees `out_language` has ≥ 2 writable bytes;
        // we write exactly the 2 language chars (no NUL, per the NDK contract).
        unsafe {
            out_language.write(language[0] as c_char);
            out_language.add(1).write(language[1] as c_char);
        }
    }
}

/// `int32_t AConfiguration_getNavHidden(AConfiguration* config)`. **minimal-correct.** Stale handle →
/// `ACONFIGURATION_NAVHIDDEN_ANY` (0), the "unset" sentinel.
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*` (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aconfiguration_getnavhidden(config: *mut c_void) -> i32 {
    ndk_registry::configurations()
        .with(ptr_to_handle(config), |c| c.nav_hidden)
        .unwrap_or(0)
}

/// `int32_t AConfiguration_getScreenHeightDp(AConfiguration* config)`. **minimal-correct.** Stale
/// handle → `ACONFIGURATION_SCREEN_HEIGHT_DP_ANY` (0).
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*` (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aconfiguration_getscreenheightdp(config: *mut c_void) -> i32 {
    ndk_registry::configurations()
        .with(ptr_to_handle(config), |c| c.screen_height_dp)
        .unwrap_or(0)
}

/// `int32_t AConfiguration_getScreenSize(AConfiguration* config)`. **minimal-correct.** Stale handle
/// → `ACONFIGURATION_SCREENSIZE_ANY` (0).
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*` (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aconfiguration_getscreensize(config: *mut c_void) -> i32 {
    ndk_registry::configurations()
        .with(ptr_to_handle(config), |c| c.screen_size)
        .unwrap_or(0)
}

/// `int32_t AConfiguration_getScreenWidthDp(AConfiguration* config)`. **minimal-correct.** Stale
/// handle → `ACONFIGURATION_SCREEN_WIDTH_DP_ANY` (0).
///
/// # Safety
/// `config` must be an Eclipse `AConfiguration*` (or garbage, which is rejected).
unsafe extern "C" fn eclipse_aconfiguration_getscreenwidthdp(config: *mut c_void) -> i32 {
    ndk_registry::configurations()
        .with(ptr_to_handle(config), |c| c.screen_width_dp)
        .unwrap_or(0)
}

// ---- ALooper (7) — REAL fd-backed, wakeable Eclipse per-thread looper ----------------------------
//
// 2026-06-05: a thread-local Eclipse looper handle backed by a real [`crate::loader::looper::Looper`]
// (an owned wake `eventfd` + the registered `(fd, ident, events)` poll set). `pollOnce` does a genuine
// `poll(2)` over the wake fd + every registered fd, returning the standard NDK outcome: a ready fd's
// `ident`, `ALOOPER_POLL_WAKE` on a wake, `ALOOPER_POLL_TIMEOUT` on the timeout, `ALOOPER_POLL_ERROR`
// on a poll failure. This replaced the prior bookkeeping-only sentinel looper — the looper now
// actually blocks and wakes on its fds, which is what the engine's input/job-system threads need.
//
// (`ALOOPER_POLL_CALLBACK` = -2 is part of the public looper contract for fds added WITH a callback;
// Eclipse's `addFd` rejects a non-null callback — the engine uses the ident form — so `pollOnce` never
// returns CALLBACK. See [`crate::loader::looper`] for the sentinels.)
use crate::loader::looper::{
    PollResult, ALOOPER_EVENT_INPUT, ALOOPER_POLL_ERROR, ALOOPER_POLL_TIMEOUT, ALOOPER_POLL_WAKE,
};

thread_local! {
    /// The calling thread's Eclipse looper handle, set by `ALooper_prepare`, read by
    /// `ALooper_forThread`. `None` until this thread calls `prepare` (then `forThread` → NULL, the
    /// NDK contract for a thread with no looper).
    static THREAD_LOOPER: std::cell::Cell<Option<ndk_registry::NdkHandle>> =
        const { std::cell::Cell::new(None) };
}

/// `ALooper* ALooper_prepare(int opts)` — associate a looper with the calling thread and return it.
/// **minimal-correct:** returns the thread's existing looper if any (the NDK contract), else creates
/// an Eclipse looper handle, stores it thread-locally, and returns it. `opts`
/// (`ALOOPER_PREPARE_ALLOW_NON_CALLBACKS`) is accepted; Eclipse's looper always allows non-callback
/// fds. Returns NULL only on registry exhaustion.
extern "C" fn eclipse_alooper_prepare(_opts: c_int) -> *mut c_void {
    THREAD_LOOPER.with(|tl| {
        if let Some(h) = tl.get() {
            return handle_to_ptr(h); // existing looper for this thread (NDK: prepare is idempotent)
        }
        // Build a real fd-backed looper (allocates its wake `eventfd`). NULL on fd exhaustion (the NDK
        // "no looper" answer) — never a fabricated handle.
        let Some(looper) = crate::loader::looper::Looper::new() else {
            return std::ptr::null_mut();
        };
        // Register this looper's wake handle so the engine-path winit input feed can wake a parked
        // `pollOnce` lock-free (see `ndk_registry::wake_all_loopers`).
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

/// `ALooper* ALooper_forThread(void)` — the calling thread's looper, or NULL if none has been
/// prepared. **minimal-correct:** returns the thread-local looper handle set by `ALooper_prepare`, or
/// NULL (the exact NDK contract for a thread with no looper).
extern "C" fn eclipse_alooper_forthread() -> *mut c_void {
    THREAD_LOOPER.with(|tl| match tl.get() {
        Some(h) => handle_to_ptr(h),
        None => std::ptr::null_mut(),
    })
}

/// `void ALooper_acquire(ALooper* looper)` — add a reference. **minimal-correct:** Eclipse loopers
/// live in the process-global registry for the process lifetime (no per-reference free), so this is a
/// correct no-op — the looper is already kept alive. NOT a landmine (the contract is only "prevent
/// deletion", which Eclipse already guarantees).
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native (unused here; any value is accepted).
unsafe extern "C" fn eclipse_alooper_acquire(_looper: *mut c_void) {}

/// `void ALooper_release(ALooper* looper)` — remove a reference. **minimal-correct:** the matching
/// no-op for [`eclipse_alooper_acquire`] (Eclipse does not refcount-free loopers). NOT a landmine.
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native (unused here; any value is accepted).
unsafe extern "C" fn eclipse_alooper_release(_looper: *mut c_void) {}

/// `int ALooper_pollOnce(int timeoutMillis, int* outFd, int* outEvents, void** outData)` — wait for an
/// event. **REAL:** drives the calling thread's [`crate::loader::looper::Looper`] (set by
/// `ALooper_prepare`) to do a genuine `poll(2)` over its wake fd + registered fds with `timeout_millis`
/// (negative = block forever, `0` = return immediately). Returns the NDK outcome:
/// - a registered fd became ready → its `ident` (≥ 0), with `*out_fd`/`*out_events` set to the fd and
///   its events (and `*out_data` cleared — Eclipse's `addFd` form has no user data);
/// - a wake (`ALooper_wake` is not in libroblox's set, but a winit input event / internal wake uses
///   the same mechanism) → `ALOOPER_POLL_WAKE`;
/// - the timeout expired → `ALOOPER_POLL_TIMEOUT`;
/// - the underlying `poll(2)` failed → `ALOOPER_POLL_ERROR`;
/// - the calling thread has no prepared looper → `ALOOPER_POLL_ERROR` (the NDK "no associated looper").
///
/// # Safety
/// `out_fd`/`out_events`/`out_data` must each be null or valid writable pointers (the NDK contract);
/// this native writes the outcome's values to the non-null ones.
unsafe extern "C" fn eclipse_alooper_pollonce(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    // The calling thread's looper handle (from `ALooper_prepare`). No looper → the NDK no-looper error.
    let Some(handle) = THREAD_LOOPER.with(std::cell::Cell::get) else {
        return ALOOPER_POLL_ERROR;
    };
    // Take a cheap poll snapshot UNDER the slab lock, then the lock is released (the `with` closure
    // returns) BEFORE the blocking poll — never block while holding the registry mutex, or a
    // concurrent wake/addFd would deadlock (see `looper.rs` lock discipline). A stale handle → the
    // no-looper error sentinel.
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
        // SAFETY: 2026-06-05 — caller-provided writable `int*` per the contract; write the firing fd
        // (0 when no fd fired).
        unsafe { out_fd.write(fd) };
    }
    if !out_events.is_null() {
        // SAFETY: 2026-06-05 — caller-provided writable `int*`; write the fd's events (0 when none).
        unsafe { out_events.write(events) };
    }
    if !out_data.is_null() {
        // SAFETY: 2026-06-05 — caller-provided writable `void**`; Eclipse's `addFd` ident-form has no
        // user data, so the NDK out-data is always null here.
        unsafe { out_data.write(std::ptr::null_mut()) };
    }
    ret
}

/// `int ALooper_addFd(ALooper* looper, int fd, int ident, int events, ALooper_callbackFunc callback,
/// void* data)` — register a file descriptor with the looper. **REAL:** adds `(fd, ident, events)` to
/// the looper's `poll(2)` set so `ALooper_pollOnce` actually waits on `fd` and returns `ident` when it
/// fires. Returns `1` on success, `-1` on failure (the NDK contract). Per the NDK, with **no callback**
/// the `ident` MUST be `>= 0` (negative idents are reserved for the poll sentinels), so a non-positive
/// ident is rejected. Eclipse's looper uses the ident form (no callbacks), so a **non-null** `callback`
/// is rejected with `-1` — surfaced honestly rather than silently dropping the engine's callback
/// (libroblox does not use the callback form; if a future caller did, this is the correct signal, not a
/// fake success).
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native (or garbage, which the registry
/// rejects); `fd` must be a valid file descriptor the caller keeps open while registered.
unsafe extern "C" fn eclipse_alooper_addfd(
    looper: *mut c_void,
    fd: c_int,
    ident: c_int,
    events: c_int,
    callback: *mut c_void,
    _data: *mut c_void,
) -> c_int {
    // NDK contract: with no callback, ident must be >= 0. Eclipse does not run callbacks, so a non-null
    // callback is an unsupported (honest -1), not a silent drop.
    if !callback.is_null() || ident < 0 {
        return -1;
    }
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| {
        l.looper.add_fd(fd, ident, events)
    }) {
        Ok(()) => 1,  // NDK: 1 on success
        Err(_) => -1, // NDK: -1 on failure (stale/fabricated looper handle)
    }
}

/// `int ALooper_removeFd(ALooper* looper, int fd)` — unregister a file descriptor. **REAL:** removes
/// `fd` from the looper's `poll(2)` set so `pollOnce` no longer waits on it; returns `1` if the looper
/// handle is valid (the fd may or may not have been present — the NDK returns 1 for "removed or not
/// present"), `-1` for a stale/fabricated handle.
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_alooper_removefd(looper: *mut c_void, fd: c_int) -> c_int {
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| {
        let _ = l.looper.remove_fd(fd);
    }) {
        Ok(()) => 1,
        Err(_) => -1,
    }
}

// ---- winit → ALooper input feed (ENGINE path only) ----------------------------------------------
//
// 2026-06-05: how Roblox's native engine actually consumes input — and why this is a looper WAKE, not
// an NDK AInputQueue. Verified against the real `lib/x86_64/libroblox.so` (llvm-readelf --dyn-symbols):
// the engine imports the 7 `ALooper_*` natives but imports ZERO `AInputQueue_*` / `AInputEvent_*` /
// `AMotionEvent_*` / `AKeyEvent_*` — it is NOT a NativeActivity. It receives input the GLSurfaceView
// way: the Java view layer pushes events INTO the engine via the engine's OWN exported JNI methods
// (`com.roblox.engine.jni.NativeInputInterface.nativePassInput` / `nativePassMouseMove` /
// `NativeGLInterface.nativePassKeyEvent` / `nativePassText` / gamepad / gestures — all DEFINED &&
// exported in libroblox). So the NDK-level role of a host input event for this engine is a LIVENESS
// WAKE: the engine's worker threads `ALooper_prepare` + park in `pollOnce`; a host input event signals
// them to wake and re-check their sources. That is exactly [`ndk_registry::wake_all_loopers`].
//
// This feed is ENGINE-PATH ONLY. The Java-view apps (demo_app, accelerometerdemo, multitouch.test)
// keep the existing `MotionEvent` → `View.dispatchTouchEvent` JNI path in `src/graphics.rs` UNCHANGED
// — this function is never called on that path (no regression).

/// The kind of host user input a winit [`WindowEvent`] carries, for the engine looper feed. A winit
/// `WindowEvent` is mapped to `Some(kind)` for input-bearing variants and `None` for non-input
/// (`RedrawRequested`/`Resized`/`CloseRequested`/…). Kept as a small constructible enum so the
/// kind→wake decision is unit-testable without fabricating winit events (winit's `DeviceId` is not
/// publicly constructible, so a `WindowEvent` cannot be built in a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostInputKind {
    /// Pointer moved (`CursorMoved`) or entered/left the window.
    Pointer,
    /// Mouse button (`MouseInput`).
    MouseButton,
    /// Mouse wheel / scroll (`MouseWheel`).
    Scroll,
    /// Touchscreen contact (`Touch` — down/move/up/cancel).
    Touch,
    /// Keyboard key (`KeyboardInput`).
    Key,
}

/// Classify a winit [`WindowEvent`] into the [`HostInputKind`] it carries, or `None` if it is not
/// user input. The single thin, obvious winit→kind mapping; the kind→wake policy lives in
/// [`host_input_should_wake`] (unit-tested independently).
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

/// Whether a classified host input event should wake the engine's input loopers. Every input kind
/// wakes (a Roblox player drives pointer/touch/mouse/scroll/key, all of which the engine consumes via
/// its JNI input bridge), so this is `true` for any `Some(kind)`. Split out so the policy is a single
/// unit-testable function over the constructible [`HostInputKind`].
pub fn host_input_should_wake(kind: Option<HostInputKind>) -> bool {
    kind.is_some()
}

/// Engine-path winit input feed: if `event` carries user input, wake every prepared engine looper so a
/// parked `ALooper_pollOnce` returns `ALOOPER_POLL_WAKE` and the engine's input thread re-checks its
/// source. Returns the number of loopers woken (`0` if the event is not input, or no looper is
/// prepared yet). Call ONLY from the engine/GL window mode — never from the Java-view event loop.
pub fn feed_winit_input_to_loopers(event: &winit::event::WindowEvent) -> usize {
    if host_input_should_wake(classify_winit_event(event)) {
        ndk_registry::wake_all_loopers()
    } else {
        0
    }
}

/// Dev-host isolation harness (`eclipse __input-test`): drive the REAL ALooper input path end-to-end
/// WITHOUT needing the boot to reach the engine's input loop. Returns a human report on success or a
/// typed message on the first failed assertion.
///
/// It runs the exact native surface a libroblox worker uses:
/// 1. `ALooper_prepare` → a real fd-backed looper on a worker thread;
/// 2. `ALooper_addFd` registers a pipe (the stand-in for the engine's own input source) under an
///    ident; the worker parks in `ALooper_pollOnce`;
/// 3. the main thread writes the pipe (a synthetic engine input "event") → `pollOnce` wakes and
///    returns the registered IDENT with the firing fd — proving the fd genuinely wakes the looper;
/// 4. a synthetic host input WAKE ([`feed_winit_input_to_loopers`]'s primitive, `wake_all_loopers`) is
///    injected while the worker is parked → `pollOnce` returns `ALOOPER_POLL_WAKE` — proving the
///    winit-input → looper-wake feed unblocks a parked engine poll.
///
/// No GPU / no window / no VM — pure event-primitive validation. Mirrors the unit tests but as a live
/// dev-host run a human can invoke (`docs/dev-host-runbook.md`).
pub fn run_input_test() -> Result<String, String> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::mpsc;
    use std::time::Duration;

    // A pipe: the write end signals the read end POLLIN-ready — the engine's input-source stand-in.
    let mut fds = [0i32; 2];
    // SAFETY: 2026-06-05 — pipe2 writes two fresh fds into the 2-element array; both are taken into
    // RAII owners immediately below (closed on scope exit), or the call failed.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err("pipe2 failed (fd exhaustion?)".into());
    }
    // SAFETY: 2026-06-05 — fds[0]/fds[1] are fresh exclusively-owned fds from pipe2.
    let read: OwnedFd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: 2026-06-05 — fds[1] is a fresh exclusively-owned fd from pipe2.
    let mut write: std::fs::File = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let read_fd = read.as_raw_fd();

    const ENGINE_INPUT_IDENT: c_int = 11;
    let (registered_tx, registered_rx) = mpsc::channel::<bool>();
    let (fd_result_tx, fd_result_rx) = mpsc::channel::<(c_int, c_int, c_int)>();
    let (parked_tx, parked_rx) = mpsc::channel::<()>();
    let (wake_result_tx, wake_result_rx) = mpsc::channel::<c_int>();

    let worker = std::thread::spawn(move || {
        // (1) prepare + (2) addFd, then park awaiting the fd signal.
        let looper = eclipse_alooper_prepare(0);
        if looper.is_null() {
            let _ = registered_tx.send(false);
            return;
        }
        // SAFETY: 2026-06-05 — `looper` is a valid Eclipse handle; `read_fd` stays open for the run;
        // null callback + ident >= 0 is the supported ident form.
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
        // (3) park up to 5 s; the main thread writes the pipe.
        // SAFETY: 2026-06-05 — valid writable out-params; blocking poll until the fd fires.
        let rc = unsafe {
            eclipse_alooper_pollonce(5000, &mut out_fd, &mut out_events, std::ptr::null_mut())
        };
        let _ = fd_result_tx.send((rc, out_fd, out_events));

        // (4) remove the fd, then park again on a pure WAKE (no source) — the wake feed must unblock it.
        // SAFETY: 2026-06-05 — valid looper handle.
        let _ = unsafe { eclipse_alooper_removefd(looper, read_fd) };
        let _ = parked_tx.send(());
        // SAFETY: 2026-06-05 — block forever; only the injected wake returns it.
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

    // Stage 2/3: confirm registration, then signal the synthetic engine input.
    match registered_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(true) => {}
        Ok(false) => return Err("ALooper_prepare/addFd failed in the worker".into()),
        Err(_) => return Err("worker did not register its fd (timeout)".into()),
    }
    // A synthetic touch DOWN/MOVE/UP + key are all "the engine's input source has data" at the NDK
    // layer; one write makes the registered fd readable, which is what wakes the parked pollOnce.
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

    // Stage 4: the worker is now parked on a pure WAKE; inject the host-input wake feed.
    parked_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "worker did not re-park for the wake stage (timeout)".to_string())?;
    // Small settle so the worker is actually inside poll(2) before we wake (best-effort; the eventfd
    // wake is edge-safe either way — a wake before the park still leaves the counter non-zero).
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

// ---- EGL display interception (1) — connection-match the engine's EGLDisplay to Eclipse's window --
//
// 2026-06-13 — the engine resolves `egl*` through host `libEGL.so` (bionic_env tier 1) and calls
// `eglGetDisplay(EGL_DEFAULT_DISPLAY)`. Per the Khronos `EGL_KHR_platform_wayland` /
// `EGL_EXT_platform_wayland` registry text (verified 2026-06-13), on Wayland `EGL_DEFAULT_DISPLAY`
// makes EGL open Mesa's OWN `wl_display` via `wl_display_connect(NULL)` — a DIFFERENT connection than
// the one `winit` opened for Eclipse's window. The `ANativeWindow*` Eclipse hands the engine
// (`eclipse_anativewindow_fromsurface` → `current_wsi_window`) wraps a `wl_egl_window*` on `winit`'s
// `wl_surface`, and `eglCreateWindowSurface` requires the EGLDisplay's `wl_display` and the
// `wl_egl_window`'s `wl_surface` to be on the SAME connection — crossing them is `EGL_BAD_ALLOC`
// (3003). Eclipse registers its OWN `eglGetDisplay` at loader tier 0 (`EclipseNativeProvider`), which
// wins over host `libEGL` by `resolve`'s first-strong-match, and remaps `EGL_DEFAULT_DISPLAY` to the
// registered winit `wl_display` ([`ndk_registry::wsi_display`]) before delegating to the HOST
// `eglGetDisplay`, so the engine's EGLDisplay shares the `wl_egl_window`'s connection — identical to
// what `egl_engine` does for `__gl-test-anw`. A non-default `display_id`, or no registered Wayland
// display (X11/other, where the XID is server-scoped so cross-connection is not an issue), is passed
// through unchanged. This is a connection-MATCHING fix, not a workaround.

/// Decide which `EGLNativeDisplayType` value the host `eglGetDisplay` should receive for the engine's
/// request: when the engine asks for `EGL_DEFAULT_DISPLAY` (`0`) AND a winit Wayland `wl_display` is
/// registered, return that pointer (so the engine's EGLDisplay lands on winit's connection); otherwise
/// pass the original `display_id` through unchanged (a caller-chosen non-default display is never
/// rewritten; `EGL_DEFAULT_DISPLAY` on X11/other passes through, preserving X11/NVIDIA).
///
/// 2026-06-13: `EGL_DEFAULT_DISPLAY == 0 == NULL` (khronos-egl 6.0.0 `DEFAULT_DISPLAY`,
/// `NativeDisplayType = *mut c_void`). Pure + JVM-free so the mapping is a deterministic unit test.
fn resolve_egl_display_target(display_id: usize, wsi: Option<usize>) -> usize {
    if display_id == 0 {
        // EGL_DEFAULT_DISPLAY: remap to the winit wl_display on Wayland, else keep 0 (X11/other).
        wsi.unwrap_or(0)
    } else {
        display_id
    }
}

/// The host `libEGL.so` `eglGetDisplay`, dlsym'd once via Eclipse's OWN `dlopen` handle (NOT through
/// the engine's relocated symbol scope), so the tier-0 shim NEVER re-enters the engine's `eglGetDisplay`
/// — no recursion. The handle is `RTLD_NOW | RTLD_LOCAL`, process-lifetime (never `dlclose`d), mirroring
/// [`super::bionic_env::DlopenLibProvider`]. `None` if the host lacks `libEGL.so` or the symbol — the
/// native then returns `EGL_NO_DISPLAY` (a clean EGL failure, never UB). The cached value is the
/// function-pointer address as a `usize` (`Send`/`Sync`-safe to store in a `OnceLock`).
fn host_egl_get_display() -> Option<usize> {
    static HOST_EGL_GET_DISPLAY: OnceLock<Option<usize>> = OnceLock::new();
    *HOST_EGL_GET_DISPLAY.get_or_init(|| {
        // SAFETY: 2026-06-13 — `dlopen(ptr, flags)` reads the NUL-terminated C string at `ptr` (a
        // `'static` `c"…"` literal that outlives the call) and returns an opaque handle or NULL;
        // `dlsym(handle, ptr)` reads the NUL-terminated symbol name and returns the symbol's address or
        // NULL. We pass the standard `RTLD_NOW | RTLD_LOCAL`, never dereference the handle in Rust, and
        // never `dlclose` it (process-lifetime — its address is cached and called for the run). NULL
        // handle/sym is handled below as `None` (no UB). This is the established `DlopenLibProvider`
        // pattern; using our own handle keeps the lookup out of the engine's symbol scope (no recursion).
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

/// `EGLDisplay eglGetDisplay(EGLNativeDisplayType display_id)` — Eclipse-owned tier-0 override of the
/// engine's display acquisition. Remaps `EGL_DEFAULT_DISPLAY` to the registered winit `wl_display` on
/// Wayland (see [`resolve_egl_display_target`]) and delegates to the HOST `eglGetDisplay` so the
/// engine's EGLDisplay shares the `wl_egl_window`'s connection (the `EGL_BAD_ALLOC` 3003 connection
/// fix). Returns `EGL_NO_DISPLAY` (NULL) if the host `eglGetDisplay` is unavailable — a clean EGL
/// failure, never UB. Does NOT dereference `display_id` (it is an opaque native-display token).
///
/// # Safety
/// `display_id` is the opaque `EGLNativeDisplayType` the engine passes; this native treats it only as
/// a pointer-sized token (compares against `0`, forwards it), never dereferencing it.
unsafe extern "C" fn eclipse_egl_get_display(display_id: *mut c_void) -> *mut c_void {
    let Some(host) = host_egl_get_display() else {
        return std::ptr::null_mut(); // EGL_NO_DISPLAY — host libEGL.so / eglGetDisplay unavailable.
    };
    let target = resolve_egl_display_target(display_id as usize, ndk_registry::wsi_display());
    // SAFETY: 2026-06-13 — `host` is the address `dlsym` returned for the host `libEGL.so`
    // `eglGetDisplay` (non-null, checked above), whose C signature is
    // `EGLDisplay eglGetDisplay(EGLNativeDisplayType)` with both parameter and return being
    // `*mut c_void` (khronos-egl 6.0.0). Transmuting the `usize` address to that fn pointer and
    // calling it with a pointer-sized `EGLNativeDisplayType` value matches the ABI exactly. `target`
    // is either the winit `wl_display*` (a live pointer for the window's lifetime) or the original
    // `display_id` / `EGL_DEFAULT_DISPLAY` — all valid `eglGetDisplay` inputs; the host only stores
    // the value (it does not require Eclipse to keep any buffer alive past the call).
    let host_fn: unsafe extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(host) };
    unsafe { host_fn(target as *mut c_void) }
}

// ---- ANativeWindow (6) — WSI-bound: returns the REAL host-EGL native window; getters real geometry
//
// 2026-06-05 — the engine render WSI bind: Roblox's native engine creates its OWN EGL surface by
// calling host `eglCreateWindowSurface(display, config, (EGLNativeWindowType)<the `ANativeWindow*` it
// got from `ANativeWindow_fromSurface`>, …)`. For that surface to present to Eclipse's window, the
// `ANativeWindow*` Eclipse hands the engine must BE the real WSI handle host EGL accepts (Wayland
// `wl_egl_window*` / X11 XID). So when the render path has built an [`crate::egl_engine::
// EngineNativeWindow`] on Eclipse's window, `ANativeWindow_fromSurface` returns THAT real WSI pointer
// ([`ndk_registry::current_wsi_window`]); the geometry getters resolve it via
// [`ndk_registry::wsi_window_geometry`]. OWNERSHIP: Eclipse owns + exposes the native window; it does
// NOT pre-create a competing EGL context on the engine path (the engine owns its context — two
// contexts must not fight over one surface). Validated engine-style by `eclipse __gl-test-anw`.
// Until the window exists (the engine may probe `fromSurface` earlier), it falls back to a sound
// geometry-only slab handle ([`default_native_window`]). `setBuffersGeometry`/`lock`/`unlockAndPost`
// are NOT in libroblox's 5-symbol ANativeWindow import set (verified vs the engine), so they are
// intentionally not registered (§ simplicity); 2026-06-12: `getFormat` IS registered — not for
// libroblox (its set stays the 5) but for `libsurface_util_jni.so`, whose pre-load failed on
// exactly that 1 import. `acquire`/`release` are correct no-ops (Eclipse owns the window for the
// process lifetime).

/// `ANativeWindow* ANativeWindow_fromSurface(JNIEnv* env, jobject surface)` — get a native window for
/// a Java `Surface`. **WSI-bound:** when the render path has built the real WSI window on Eclipse's
/// window, returns that real `EGLNativeWindowType` pointer (a `wl_egl_window*` / XID), so the engine's
/// own `eglCreateWindowSurface(this ANativeWindow*)` presents to Eclipse's window. Until then, falls
/// back to a sound geometry-only slab handle (the window may not exist yet). Returns NULL only on
/// registry exhaustion — never a fake non-window pointer.
///
/// # Safety
/// `env`/`surface` are the JNI args; this native does not dereference them (Eclipse owns the window
/// it returns), so any value is accepted safely.
unsafe extern "C" fn eclipse_anativewindow_fromsurface(
    _env: *mut c_void,
    _surface: *mut c_void,
) -> *mut c_void {
    // Preferred: the real WSI handle host EGL accepts — what the engine will pass to its own
    // eglCreateWindowSurface to present to Eclipse's window.
    if let Some(p) = ndk_registry::current_wsi_window() {
        // 2026-06-13 — render Phase 2 present-loop handoff: the engine has now PULLED the real WSI
        // surface (it called fromSurface and got the real WSI pointer, not the geometry-only
        // fallback). Signal the winit loop to release Eclipse's VulkanRenderer so the engine's own
        // EGL window surface owns the surface alone (two producers must not share one wl_surface).
        ndk_registry::set_engine_claimed_surface(true);
        return p as *mut c_void;
    }
    // Fallback (window not built yet): a sound geometry-only handle from the slab.
    match ndk_registry::native_windows().insert(default_native_window()) {
        Ok(h) => handle_to_ptr(h),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `int32_t ANativeWindow_getWidth(ANativeWindow* window)` — the window width in pixels. **sound:**
/// a real WSI window resolves via the WSI map; a fallback slab handle via the slab; a stale/fabricated
/// pointer → `-1` (the NDK negative-error contract), never a fake size or a dereference.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_anativewindow_getwidth(window: *mut c_void) -> i32 {
    if let Some((w, _)) = ndk_registry::wsi_window_geometry(window as usize) {
        return w;
    }
    ndk_registry::native_windows()
        .with(ptr_to_handle(window), |w| w.width)
        .unwrap_or(-1)
}

/// `int32_t ANativeWindow_getHeight(ANativeWindow* window)` — the window height in pixels. **sound:**
/// real WSI geometry via the WSI map, else the slab handle; stale/fabricated pointer → `-1`.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_anativewindow_getheight(window: *mut c_void) -> i32 {
    if let Some((_, h)) = ndk_registry::wsi_window_geometry(window as usize) {
        return h;
    }
    ndk_registry::native_windows()
        .with(ptr_to_handle(window), |w| w.height)
        .unwrap_or(-1)
}

/// `int32_t ANativeWindow_getFormat(ANativeWindow* window)` — the window pixel format. **sound:**
/// the exact sibling of [`eclipse_anativewindow_getwidth`] — a real WSI window reports Eclipse's
/// surface format (`WINDOW_FORMAT_RGBA_8888`, the RGBA8888 config the engine render path builds on
/// Eclipse's window); a fallback slab handle reports its recorded [`NativeWindowState::format`]; a
/// stale/fabricated pointer → `-1` (the NDK negative-error contract), never a fake format or a
/// dereference. 2026-06-12: provided because `libsurface_util_jni.so`'s pre-load failed on exactly
/// this 1 import (owner live validation `/tmp/eclipse-866509-validate.log` line 98) — a failed
/// pre-load leaves that lib's `System.loadLibrary` armed to delegate into the apkenv shim linker
/// (the fatal NULL `_r_debug_ptr` class, core 866509). NOT a libroblox import (its ANativeWindow
/// set stays the 5 below).
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_anativewindow_getformat(window: *mut c_void) -> i32 {
    if ndk_registry::wsi_window_geometry(window as usize).is_some() {
        return WINDOW_FORMAT_RGBA_8888;
    }
    ndk_registry::native_windows()
        .with(ptr_to_handle(window), |w| w.format)
        .unwrap_or(-1)
}

/// `void ANativeWindow_acquire(ANativeWindow* window)` — add a reference. **sound-stub:** Eclipse
/// windows live in the process-global registry for the process lifetime, so this is a correct no-op
/// (the window is already kept alive). NOT a landmine.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (unused; any value accepted).
unsafe extern "C" fn eclipse_anativewindow_acquire(_window: *mut c_void) {}

/// `void ANativeWindow_release(ANativeWindow* window)` — remove a reference. **sound-stub:** the
/// matching no-op for [`eclipse_anativewindow_acquire`]. NOT a landmine.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (unused; any value accepted).
unsafe extern "C" fn eclipse_anativewindow_release(_window: *mut c_void) {}

// =================================================================================================
// media-ndk (libmediandk) — the 33 NDK media natives. SOUND-STUB: media playback deferred
// (gameplay-time). 2026-06-05.
//
// Media (video decode/encode) is a gameplay-time subsystem libroblox does not need to start/render,
// so each native returns its PUBLIC-ABI failure/unavailable sentinel (per `media/NdkMediaCodec.h`,
// `media/NdkMediaFormat.h`, `media/NdkMediaError.h`): a caller cleanly detects "no media" and never
// acts on a fabricated success. NO opaque handle is ever minted (codec/format constructors return
// NULL), so there is NO global state and the getters/setters/delete are trivial no-ops over a NULL
// the engine never holds — no UB. If the DT_INIT_ARRAY discovery loop later proves any of these is
// init-critical (not gameplay-time), it gets a real host-codec bridge then.
// =================================================================================================

/// `media_status_t` is an `enum` → C `int` (from the public `media/NdkMediaError.h`).
type MediaStatus = c_int;
/// `AMEDIA_ERROR_BASE = -10000` (public `media/NdkMediaError.h`).
const AMEDIA_ERROR_BASE: MediaStatus = -10000;
/// `AMEDIA_ERROR_UNSUPPORTED = AMEDIA_ERROR_BASE - 9 = -10009` ("the required operation or media
/// formats are not supported") — the apt sentinel for an unavailable media subsystem (a caller checks
/// `!= AMEDIA_OK`/`AMEDIA_OK = 0`). 2026-06-05.
const AMEDIA_ERROR_UNSUPPORTED: MediaStatus = AMEDIA_ERROR_BASE - 9;

// ---- AMediaCodec (14) ---------------------------------------------------------------------------

/// `media_status_t AMediaCodec_configure(AMediaCodec*, const AMediaFormat*, ANativeWindow*,
/// AMediaCrypto*, uint32_t flags)`. **sound-stub:** no codec exists (constructors return NULL), so
/// configure reports the media subsystem is unsupported.
///
/// # Safety
/// The pointer args are accepted but never dereferenced (the codec is always NULL here).
unsafe extern "C" fn eclipse_amediacodec_configure(
    _codec: *mut c_void,
    _format: *const c_void,
    _surface: *mut c_void,
    _crypto: *mut c_void,
    _flags: u32,
) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `AMediaCodec* AMediaCodec_createDecoderByType(const char* mime_type)`. **sound-stub:** no host
/// codec bridge yet → NULL (the documented failure: "NULL if the codec cannot be created"). A caller
/// checks for NULL before using the codec.
///
/// # Safety
/// `mime_type` is the C-string arg; it is not dereferenced (always returns NULL).
unsafe extern "C" fn eclipse_amediacodec_createdecoderbytype(
    _mime_type: *const c_char,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// `AMediaCodec* AMediaCodec_createEncoderByType(const char* mime_type)`. **sound-stub:** NULL.
///
/// # Safety
/// `mime_type` is the C-string arg; it is not dereferenced (always returns NULL).
unsafe extern "C" fn eclipse_amediacodec_createencoderbytype(
    _mime_type: *const c_char,
) -> *mut c_void {
    std::ptr::null_mut()
}

/// `media_status_t AMediaCodec_delete(AMediaCodec*)`. **sound-stub:** no codec was ever minted, so
/// deleting a NULL is a no-op → `AMEDIA_OK`-equivalent is wrong here (we never owned it); the public
/// contract returns a status, and reporting unsupported is consistent with "this subsystem is off".
///
/// # Safety
/// `codec` is accepted but never dereferenced (always NULL here).
unsafe extern "C" fn eclipse_amediacodec_delete(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `ssize_t AMediaCodec_dequeueInputBuffer(AMediaCodec*, int64_t timeoutUs)`. **sound-stub:** the
/// public contract returns a buffer index ≥ 0 on success or a negative `AMEDIA_ERROR_*` on failure;
/// no codec → the unsupported error (negative). A caller checks `< 0`.
///
/// # Safety
/// `codec` is accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_dequeueinputbuffer(
    _codec: *mut c_void,
    _timeout_us: i64,
) -> isize {
    AMEDIA_ERROR_UNSUPPORTED as isize
}

/// `ssize_t AMediaCodec_dequeueOutputBuffer(AMediaCodec*, AMediaCodecBufferInfo*, int64_t timeoutUs)`.
/// **sound-stub:** negative unsupported error (no codec). A caller checks `< 0`.
///
/// # Safety
/// the pointer args are accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_dequeueoutputbuffer(
    _codec: *mut c_void,
    _info: *mut c_void,
    _timeout_us: i64,
) -> isize {
    AMEDIA_ERROR_UNSUPPORTED as isize
}

/// `media_status_t AMediaCodec_flush(AMediaCodec*)`. **sound-stub:** unsupported (no codec).
///
/// # Safety
/// `codec` is accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_flush(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `uint8_t* AMediaCodec_getInputBuffer(AMediaCodec*, size_t idx, size_t* out_size)`. **sound-stub:**
/// NULL (no codec → no buffer; the documented failure). The `out_size` out-param is left untouched;
/// callers that get NULL must not read it.
///
/// # Safety
/// the pointer args are accepted but never dereferenced (always returns NULL).
unsafe extern "C" fn eclipse_amediacodec_getinputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    std::ptr::null_mut()
}

/// `uint8_t* AMediaCodec_getOutputBuffer(AMediaCodec*, size_t idx, size_t* out_size)`. **sound-stub:**
/// NULL (no codec).
///
/// # Safety
/// the pointer args are accepted but never dereferenced (always returns NULL).
unsafe extern "C" fn eclipse_amediacodec_getoutputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _out_size: *mut usize,
) -> *mut u8 {
    std::ptr::null_mut()
}

/// `AMediaFormat* AMediaCodec_getOutputFormat(AMediaCodec*)`. **sound-stub:** NULL (no codec → no
/// format).
///
/// # Safety
/// `codec` is accepted but never dereferenced (always returns NULL).
unsafe extern "C" fn eclipse_amediacodec_getoutputformat(_codec: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

/// `media_status_t AMediaCodec_queueInputBuffer(AMediaCodec*, size_t idx, off_t offset, size_t size,
/// uint64_t time, uint32_t flags)`. **sound-stub:** unsupported (no codec).
///
/// # Safety
/// `codec` is accepted but never dereferenced.
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

/// `media_status_t AMediaCodec_releaseOutputBuffer(AMediaCodec*, size_t idx, bool render)`.
/// **sound-stub:** unsupported (no codec).
///
/// # Safety
/// `codec` is accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_releaseoutputbuffer(
    _codec: *mut c_void,
    _idx: usize,
    _render: bool,
) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `media_status_t AMediaCodec_start(AMediaCodec*)`. **sound-stub:** unsupported (no codec).
///
/// # Safety
/// `codec` is accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_start(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `media_status_t AMediaCodec_stop(AMediaCodec*)`. **sound-stub:** unsupported (no codec).
///
/// # Safety
/// `codec` is accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediacodec_stop(_codec: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

// ---- AMediaFormat (9) ---------------------------------------------------------------------------

/// `AMediaFormat* AMediaFormat_new(void)`. **sound-stub:** the media subsystem is deferred, so no
/// format object is minted → NULL. A caller checks for NULL before using the format (and the codec
/// path that would consume a format is itself unavailable).
extern "C" fn eclipse_amediaformat_new() -> *mut c_void {
    std::ptr::null_mut()
}

/// `media_status_t AMediaFormat_delete(AMediaFormat*)`. **sound-stub:** no format was minted → no-op
/// over a NULL; reports unsupported for consistency with the off subsystem.
///
/// # Safety
/// `format` is accepted but never dereferenced (always NULL here).
unsafe extern "C" fn eclipse_amediaformat_delete(_format: *mut c_void) -> MediaStatus {
    AMEDIA_ERROR_UNSUPPORTED
}

/// `bool AMediaFormat_getInt32(AMediaFormat*, const char* name, int32_t* out)`. **sound-stub:** the
/// public contract returns `false` if the key is absent / cannot be read; with no format that is
/// always the case. The `out` param is left untouched (the caller must not read it on `false`).
///
/// # Safety
/// the pointer args are accepted but never dereferenced (always returns false).
unsafe extern "C" fn eclipse_amediaformat_getint32(
    _format: *mut c_void,
    _name: *const c_char,
    _out: *mut i32,
) -> bool {
    false
}

/// `bool AMediaFormat_getBuffer(AMediaFormat*, const char* name, void** data, size_t* size)`.
/// **sound-stub:** `false` (no format → no buffer). Out-params untouched.
///
/// # Safety
/// the pointer args are accepted but never dereferenced (always returns false).
unsafe extern "C" fn eclipse_amediaformat_getbuffer(
    _format: *mut c_void,
    _name: *const c_char,
    _data: *mut *mut c_void,
    _size: *mut usize,
) -> bool {
    false
}

/// `void AMediaFormat_setInt32(AMediaFormat*, const char* name, int32_t value)`. **sound-stub:** no
/// format to mutate → no-op (the function is `void`; no caller depends on a result).
///
/// # Safety
/// the pointer args are accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediaformat_setint32(
    _format: *mut c_void,
    _name: *const c_char,
    _value: i32,
) {
}

/// `void AMediaFormat_setFloat(AMediaFormat*, const char* name, float value)`. **sound-stub:** no-op.
///
/// # Safety
/// the pointer args are accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediaformat_setfloat(
    _format: *mut c_void,
    _name: *const c_char,
    _value: f32,
) {
}

/// `void AMediaFormat_setString(AMediaFormat*, const char* name, const char* value)`. **sound-stub:**
/// no-op.
///
/// # Safety
/// the pointer args are accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediaformat_setstring(
    _format: *mut c_void,
    _name: *const c_char,
    _value: *const c_char,
) {
}

/// `void AMediaFormat_setBuffer(AMediaFormat*, const char* name, const void* data, size_t size)`.
/// **sound-stub:** no-op.
///
/// # Safety
/// the pointer args are accepted but never dereferenced.
unsafe extern "C" fn eclipse_amediaformat_setbuffer(
    _format: *mut c_void,
    _name: *const c_char,
    _data: *const c_void,
    _size: usize,
) {
}

/// A stable, process-global empty NUL-terminated C string for [`eclipse_amediaformat_tostring`].
static EMPTY_CSTR: [u8; 1] = [0];

/// `const char* AMediaFormat_toString(AMediaFormat*)`. **sound-stub:** the public contract returns a
/// human-readable string owned by the format. With no format, returning a stable empty C string
/// (`""`) is the soundest answer — never NULL (so a naive `printf("%s")` cannot crash) and clearly
/// empty (so a caller learns nothing was set). NOT a fake non-empty description.
///
/// # Safety
/// `format` is accepted but never dereferenced; the returned pointer is a process-lifetime static.
unsafe extern "C" fn eclipse_amediaformat_tostring(_format: *mut c_void) -> *const c_char {
    EMPTY_CSTR.as_ptr() as *const c_char
}

// ---- AMEDIAFORMAT_KEY_* (10) — `const char*` DATA objects holding the public key strings ---------
//
// 2026-06-05: each `AMEDIAFORMAT_KEY_*` is declared `extern const char*` (public
// `media/NdkMediaFormat.h`) — a DATA symbol whose VALUE is a pointer to the canonical MediaFormat key
// string (the same strings as the Java `android.media.MediaFormat.KEY_*`). A relocation reads the
// symbol's value (the `char*`), so Eclipse provides, per key, a `*const c_char` static initialized to
// point at a static NUL-terminated key string; the registered address is that pointer object's
// address (the data symbol). These are REAL public constants (minimal-correct data, not a stub).

/// The 10 canonical MediaFormat key strings, in the registration index order used by
/// [`amediaformat_key_addr`]. NUL-terminated for C consumption.
static AMEDIAFORMAT_KEY_STRINGS: [&[u8]; 10] = [
    b"bitrate\0",          // 0: BIT_RATE
    b"channel-count\0",    // 1: CHANNEL_COUNT
    b"color-format\0",     // 2: COLOR_FORMAT
    b"frame-rate\0",       // 3: FRAME_RATE
    b"height\0",           // 4: HEIGHT
    b"i-frame-interval\0", // 5: I_FRAME_INTERVAL
    b"mime\0",             // 6: MIME
    b"sample-rate\0",      // 7: SAMPLE_RATE
    b"stride\0",           // 8: STRIDE
    b"width\0",            // 9: WIDTH
];

/// The 10 `const char*` DATA objects: each holds a pointer to the matching key string. This is the
/// actual storage the `AMEDIAFORMAT_KEY_*` data symbols resolve to (the symbol's value == this
/// pointer). Initialized once by [`amediaformat_key_addr`].
struct KeyPtrTable([*const c_char; 10]);
// SAFETY: 2026-06-05 — the table holds pointers into `AMEDIAFORMAT_KEY_STRINGS`, a process-lifetime
// `static` whose bytes never move and are never mutated. Sharing these read-only pointers across
// threads is sound.
unsafe impl Sync for KeyPtrTable {}
// SAFETY: see the `Sync` note — process-lifetime, read-only static string pointers.
unsafe impl Send for KeyPtrTable {}

static AMEDIAFORMAT_KEY_PTRS: OnceLock<KeyPtrTable> = OnceLock::new();

/// Initialize (once) the `const char*` key-pointer table and return the address of entry `idx` — the
/// `AMEDIAFORMAT_KEY_*` data symbol (a `const char**`-shaped data object whose value is the key
/// string pointer). `idx` is the registration index into [`AMEDIAFORMAT_KEY_STRINGS`].
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

// =================================================================================================
// audio (OpenSL ES) — the 8 audio natives. REAL OpenSL ES → host audio (cpal). 2026-06-05.
//
// `slCreateEngine` is implemented by [`super::opensl`] (a working `SLObjectItf` engine whose vtables
// drive CreateOutputMix/CreateAudioPlayer/Enqueue → a cpal host output stream). The 7 `SL_IID_*` are
// DATA objects of type `SLInterfaceID` (a pointer to a 128-bit interface-UUID struct); each resolves
// to a stable, valid, distinct Eclipse-owned `SLInterfaceID_` so the relocation has a real non-null
// address AND `opensl::obj_get_interface` can match the engine's requested interface by these
// pointers (via [`sl_iid_index`]). Only `slCreateEngine` + these 7 IIDs are imported by libroblox;
// everything else flows through the vtables, so no additional audio symbol is registered (no dead
// natives — AGENTS.md §2.5).
// =================================================================================================

/// The public `SLInterfaceID_` struct layout (a 128-bit interface UUID), from `SLES/OpenSLES.h`:
/// `{ SLuint32 time_low; SLuint16 time_mid; SLuint16 time_hi_and_version; SLuint16 clock_seq;
/// SLuint8 node[6]; }`. `SLInterfaceID` is a pointer to a `const` one of these. Eclipse provides a
/// stable, distinct instance per `SL_IID_*` so the data symbols resolve to valid non-null addresses.
#[repr(C)]
#[derive(Clone, Copy)]
struct SlInterfaceId {
    time_low: u32,
    time_mid: u16,
    time_hi_and_version: u16,
    clock_seq: u16,
    node: [u8; 6],
}

/// The 7 Eclipse-owned `SLInterfaceID_` backing structs. Distinct `time_low` values (the registration
/// index) keep them distinguishable; the exact UUID bytes are irrelevant because audio is unavailable
/// (no engine is ever created to query an interface). Process-lifetime → stable struct addresses.
struct SlIidStructs([SlInterfaceId; 7]);
// SAFETY: 2026-06-05 — process-lifetime `static` (via `OnceLock`); never mutated after init. Sharing
// read-only references/pointers across threads is sound.
unsafe impl Sync for SlIidStructs {}
// SAFETY: see the `Sync` note — process-lifetime, read-only.
unsafe impl Send for SlIidStructs {}

/// The 7 `SLInterfaceID` DATA objects (each a pointer to the matching backing struct) — these are the
/// symbol *values* the relocations read. A SEPARATE `OnceLock` so the pointers are computed from the
/// backing structs' FINAL stable addresses (after they live in [`SL_IID_STRUCTS`]), never from a
/// moved-from local.
struct SlIidPtrs([*const SlInterfaceId; 7]);
// SAFETY: 2026-06-05 — the pointers reference [`SL_IID_STRUCTS`], a process-lifetime static whose
// structs never move. Sharing these read-only pointers across threads is sound.
unsafe impl Sync for SlIidPtrs {}
// SAFETY: see the `Sync` note — process-lifetime, read-only.
unsafe impl Send for SlIidPtrs {}

static SL_IID_STRUCTS: OnceLock<SlIidStructs> = OnceLock::new();
static SL_IID_PTRS: OnceLock<SlIidPtrs> = OnceLock::new();

/// Initialize (once) the `SL_IID_*` storage and return the address of the `SLInterfaceID` data object
/// at index `idx` — a data symbol whose value is a pointer to the backing `SLInterfaceID_` struct.
fn sl_iid_addr(idx: usize) -> u64 {
    // Phase 1: place the backing structs in their final (stable) static location.
    let structs = SL_IID_STRUCTS.get_or_init(|| {
        let mut ids = [SlInterfaceId {
            time_low: 0,
            time_mid: 0,
            time_hi_and_version: 0,
            clock_seq: 0,
            node: [0; 6],
        }; 7];
        for (i, id) in ids.iter_mut().enumerate() {
            id.time_low = i as u32; // distinct per IID (the exact UUID is irrelevant — audio is off)
        }
        SlIidStructs(ids)
    });
    // Phase 2: capture pointers to those FINAL stable struct addresses (never a moved-from local).
    let ptrs = SL_IID_PTRS
        .get_or_init(|| SlIidPtrs(std::array::from_fn(|i| std::ptr::addr_of!(structs.0[i]))));
    std::ptr::addr_of!(ptrs.0[idx]) as u64
}

/// The address of the `SL_IID_*` data symbol at registration index `idx` (0..=6). Exposed to
/// [`super::opensl`]'s `__audio-test` harness so it can pass the same `SLInterfaceID` value the engine
/// would. Idempotent (lazy-inits the storage). 2026-06-05.
pub(crate) fn sl_iid_addr_for_test(idx: usize) -> u64 {
    sl_iid_addr(idx)
}

/// Map an `SLInterfaceID` **value** (the pointer the engine passes to `GetInterface`, which is the
/// value stored at an `SL_IID_*` data symbol — i.e. the address of the backing `SLInterfaceID_`
/// struct) back to its registration index 0..=6:
/// 0=ANDROIDCONFIGURATION, 1=ANDROIDSIMPLEBUFFERQUEUE, 2=BUFFERQUEUE, 3=ENGINE, 4=PLAY, 5=RECORD,
/// 6=VOLUME. Returns `None` for any other pointer. Used by [`super::opensl`]'s `GetInterface` to
/// resolve which interface the engine requested without re-deriving the UUID layout.
pub(crate) fn sl_iid_index(iid_value: usize) -> Option<usize> {
    // Ensure the storage is initialized (idempotent; the natives also call `sl_iid_addr`).
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

    // Serializes the ANativeWindow tests that share the process-global `ANativeWindow_fromSurface`
    // path + the WSI-window registry: one test registers a real WSI window (so fromSurface returns
    // it), the others assert the no-WSI fallback (a slab handle). Running them concurrently would let
    // a transient registration cross-contaminate. A plain `std::sync::Mutex` (no dep) serializes them
    // without weakening any assertion. 2026-06-05.
    static ANW_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ---- liblog emit capture (test-only) -------------------------------------------------------
    //
    // 2026-06-05: a per-thread capture armed by the variadic-shim test. When armed, `emit_log`
    // pushes `(priority, tag, msg)` here and returns instead of hitting `tracing` — so the test
    // observes the EXACT message the C shim formatted from a real format string + varargs. Per
    // thread + RefCell → no cross-test interference, no global lock.
    thread_local! {
        static EMIT_CAPTURE: RefCell<Option<Vec<(c_int, String, String)>>> = const { RefCell::new(None) };
    }

    /// Called by [`emit_log`] in test builds. Returns `true` (consumed) iff a capture is armed on
    /// this thread, recording `(priority, tag, msg)`; otherwise `false` (fall through to `tracing`).
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

    /// Arm the capture, run `body` (which calls into the shim), and return the captured emits.
    fn with_capture(body: impl FnOnce()) -> Vec<(c_int, String, String)> {
        EMIT_CAPTURE.with(|c| *c.borrow_mut() = Some(Vec::new()));
        body();
        EMIT_CAPTURE.with(|c| c.borrow_mut().take().unwrap_or_default())
    }

    // ---- provider registry behavior ------------------------------------------------------------

    #[test]
    fn provider_resolves_registered_and_rejects_unregistered() {
        let p = EclipseNativeProvider::with_bionic_natives();
        // A registered liblog native resolves to a non-null, strong Eclipse address.
        let got = p.resolve("__android_log_write");
        assert!(got.is_some(), "__android_log_write must be registered");
        let got = got.unwrap();
        assert!(got.addr != 0, "registered native address must be non-null");
        assert!(!got.weak, "Eclipse natives are strong definitions");
        // Registered bionic-libc natives resolve too.
        assert!(p.resolve("__memcpy_chk").is_none()); // NOT in this cut (host baseline covers it)
        assert!(p.resolve("__strlen_chk").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__errno").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__stack_chk_guard").is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__sF").is_some_and(|r| r.addr != 0));
        // The two VARIADIC liblog natives are now registered (DEFINED by the C shim, 2026-06-05):
        // they resolve to the shim's strong, non-null addresses.
        assert!(p
            .resolve("__android_log_print")
            .is_some_and(|r| r.addr != 0));
        assert!(p
            .resolve("__android_log_assert")
            .is_some_and(|r| r.addr != 0));
        // The va_list liblog native + the FORTIFY umask wrapper (2026-06-12 — the two
        // libbacktrace-native.so pre-load imports; their absence re-opened the apkenv delegation).
        assert!(p
            .resolve("__android_log_vprint")
            .is_some_and(|r| r.addr != 0));
        assert!(p.resolve("__umask_chk").is_some_and(|r| r.addr != 0));
        // An unregistered name → None (falls through to the host tier).
        assert_eq!(p.resolve("memcpy"), None);
        assert_eq!(p.resolve("__eclipse_no_such_native__"), None);
    }

    #[test]
    fn with_bionic_natives_registers_the_three_implemented_categories() {
        let p = EclipseNativeProvider::with_bionic_natives();
        // 6 liblog (3 fixed-arity Rust + 2 variadic C-shim + 1 va_list C-shim; 2026-06-12 —
        // __android_log_vprint, a libbacktrace-native pre-load import) + 16 bionic-libc
        // (2026-06-12 — __umask_chk, the other libbacktrace-native pre-load import) + 25
        // bionic-stdio FILE*-translation (22 Rust + 3 C-shim; 2026-06-12 — the &__sF[i] sentinel
        // remap) + 7 bionic-signal (6 translating 2026-06-11; + sigaltstack 2026-06-12 — the
        // core-1223806 caller-attribution forward) + 2 bionic link-map introspection
        // (dl_iterate_phdr + dladdr, 2026-06-12 — core 1223806's terminate-loop root cause: the
        // host-glibc walk could never contain the Eclipse-mapped engine images) + 4 bionic-netdb
        // resolver-ABI (getaddrinfo/freeaddrinfo/gai_strerror/getnameinfo, 2026-06-12 — the
        // engine HttpError:DnsResolve root cause: glibc-shaped addrinfo tails read through the
        // bionic field order) + 28 ndk-android
        // (2026-06-12 — ANativeWindow_getFormat, libsurface_util_jni's sole unresolved pre-load
        // import) + 33 media-ndk + 8 audio + 53 bionic-pthread/TLS/sem/syscall (37 + the 14
        // thread-lifecycle natives added 2026-06-05: create/join/detach/setname_np/kill/
        // getattr_np/get+setschedparam/attr_*; + __cxa_thread_atexit_impl & pthread_atfork
        // 2026-06-12 — the core-947663 destructor-order fix and the last libbacktrace-native
        // pre-load import) + 5 bionic-sysconf system-query (sysconf/getauxval/sched_getcpu/
        // getpagesize/sysinfo — the allocator-bootstrap fix, 2026-06-05) + 1 EGL display interception
        // (2026-06-13 — eglGetDisplay, the EGL_BAD_ALLOC 3003 connection-match fix) + 4 Vulkan WSI
        // interception (2026-06-13 — vkGetInstanceProcAddr/vkCreateInstance/vkCreateAndroidSurfaceKHR
        // plus the `dlsym` interposer that routes the engine's runtime-dlopen'd libvulkan lookups to
        // them, the Android→Wayland Vulkan WSI translation; Mode-6 "Unable to create Vulkan instance" fix) = 192.
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
            // liblog (3 fixed-arity Rust + 2 variadic C-shim + 1 va_list C-shim)
            "__android_log_write",
            "__android_log_buf_write",
            "android_set_abort_message",
            "__android_log_print",
            "__android_log_assert",
            "__android_log_vprint",
            // bionic-libc (16)
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
            // bionic stdio FILE* translation (25; 22 Rust + 3 C-shim) — 2026-06-12
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
            // bionic signal ABI (7) — 2026-06-11; sigaltstack 2026-06-12 (core 1223806)
            "sigaction",
            "sigemptyset",
            "sigaddset",
            "sigfillset",
            "sigprocmask",
            "pthread_sigmask",
            "sigaltstack",
            // bionic link-map introspection (2) — 2026-06-12 (core 1223806)
            "dl_iterate_phdr",
            "dladdr",
            // bionic netdb resolver ABI (4) — 2026-06-12 (engine DnsResolve root cause)
            "getaddrinfo",
            "freeaddrinfo",
            "gai_strerror",
            "getnameinfo",
            // EGL display interception (1) — 2026-06-13 (EGL_BAD_ALLOC 3003 connection-match)
            "eglGetDisplay",
            // Vulkan WSI interception (3) — 2026-06-13 (Android→Wayland Vulkan WSI translation)
            "vkGetInstanceProcAddr",
            "vkCreateInstance",
            "vkCreateAndroidSurfaceKHR",
            // ndk-android (28)
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
            // media-ndk (33)
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
            // audio (8)
            "slCreateEngine",
            "SL_IID_ANDROIDCONFIGURATION",
            "SL_IID_ANDROIDSIMPLEBUFFERQUEUE",
            "SL_IID_BUFFERQUEUE",
            "SL_IID_ENGINE",
            "SL_IID_PLAY",
            "SL_IID_RECORD",
            "SL_IID_VOLUME",
            // bionic pthread / TLS / sem / syscall (53) — the threading runtime (2026-06-05;
            // __cxa_thread_atexit_impl + pthread_atfork added 2026-06-12)
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
            // bionic system-query (5) — the allocator-bootstrap fix (2026-06-05)
            "sysconf",
            "getauxval",
            "sched_getcpu",
            "getpagesize",
            "sysinfo",
        ] {
            assert!(p.resolve(name).is_some(), "{name} must be registered");
        }
    }

    // ---- the VARIADIC liblog C shim: EXECUTE it and verify the formatted message ---------------
    //
    // 2026-06-05: this test CALLS `__android_log_print` (DEFINED in src/loader/liblog_shim.c via the
    // cc crate) through the real shim, with a real format string + args, and asserts the C side
    // formatted the line correctly and forwarded it to `eclipse_liblog_emit` → `emit_log`. This is
    // the proof that the variadic bridge works end-to-end (Rust → C varargs → vsnprintf → Rust
    // sink). Safe: it executes ONLY Eclipse's own trivial, unit-tested C shim.

    extern "C" {
        // Re-declare the shim entry points for the test (they are private to the module otherwise).
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
            // SAFETY: 2026-06-05 — `__android_log_print` is Eclipse's own C shim; `tag`/`fmt` are
            // valid NUL-terminated C strings kept alive across the call, and the varargs (i32,
            // *const c_char, u32) match the `%d %s %x` conversions in `fmt`.
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

        // Exactly one line was forwarded, with the priority, tag, and the vsnprintf-formatted body.
        assert_eq!(emits.len(), 1, "shim forwards exactly one line per call");
        let (prio, got_tag, got_msg) = &emits[0];
        assert_eq!(*prio, ANDROID_LOG_INFO, "priority passes through unchanged");
        assert_eq!(got_tag, "EclipseTag", "tag passes through unchanged");
        assert_eq!(
            got_msg, "n=42 s=hi hex=0xbeef",
            "the C shim's vsnprintf formatted the varargs correctly"
        );

        // __android_log_print returns the emitted byte count (> 0) per the liblog contract.
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
            // SAFETY: 2026-06-05 — null tag is explicitly handled by the shim (substitutes ""); the
            // format has no conversions so no varargs are consumed.
            let _ =
                unsafe { __android_log_print(ANDROID_LOG_WARN, std::ptr::null(), fmt.as_ptr()) };
        });
        assert_eq!(emits.len(), 1);
        let (prio, got_tag, got_msg) = &emits[0];
        assert_eq!(*prio, ANDROID_LOG_WARN);
        assert_eq!(got_tag, "", "a null tag becomes an empty string");
        assert_eq!(got_msg, "plain");
    }

    // ---- provider tier ordering: Eclipse beats host --------------------------------------------

    #[test]
    fn eclipse_provider_beats_host_in_scope_order() {
        use crate::loader::resolve::{HostDlsymProvider, Scope};
        // A scope with the Eclipse provider FIRST, host second. For a name BOTH define, Eclipse must
        // win. Register a host-known name (`memcpy`) on the Eclipse provider at a sentinel address.
        let mut eclipse = EclipseNativeProvider::empty();
        eclipse.register("memcpy", 0xdead_beef);
        let mut scope = Scope::new();
        scope.push(Box::new(eclipse));
        scope.push(Box::new(HostDlsymProvider));
        // Eclipse is first in scope → its `memcpy` (the sentinel) wins over host glibc's real one.
        assert_eq!(scope.resolve("memcpy").map(|r| r.addr), Some(0xdead_beef));
        // A name only the host has still falls through to the host tier.
        assert!(scope.resolve("malloc").is_some_and(|r| r.addr != 0));
    }

    // ---- a couple of the _chk natives behave per contract --------------------------------------

    #[test]
    fn strlen_chk_returns_length_within_bound() {
        let s = b"hello\0";
        // SAFETY: test-local valid NUL-terminated buffer; s_len 6 > strlen 5 → within bound.
        let len = unsafe { eclipse_strlen_chk(s.as_ptr().cast(), 6) };
        assert_eq!(len, 5);
    }

    #[test]
    fn strchr_chk_finds_char_within_bound() {
        let s = b"abcde\0";
        // SAFETY: valid NUL-terminated buffer; bound 6 covers the NUL.
        let p = unsafe { eclipse_strchr_chk(s.as_ptr().cast(), b'c' as c_int, 6) };
        assert!(!p.is_null());
        // SAFETY: `p` points into `s` at the 'c'.
        assert_eq!(unsafe { *p } as u8, b'c');
    }

    #[test]
    fn umask_chk_forwards_a_valid_mode_and_round_trips() {
        // 2026-06-12: __umask_chk (bionic FORTIFY umask) must forward a valid 0..=0o777 mode to the
        // kernel and return the PREVIOUS mask — the libbacktrace-native.so pre-load import whose
        // absence (with __android_log_vprint) re-opened the apkenv delegation (core 866509). The
        // invalid-mode branch aborts the process (the FORTIFY contract) and is pinned by the bound
        // check's presence, not exercised here. umask is process-global: save + restore around.
        // 2026-06-12 (invariant): this test is the SOLE umask(2) toucher in the test binary —
        // umask is process-global with no read-only query, so a second concurrent toucher makes
        // the save/round-trip/restore below a flake. grep for `umask` before adding one.
        // SAFETY: umask(2) takes any mode and cannot fail; all modes used are valid masks.
        unsafe {
            let saved = libc::umask(0o022); // returns the pre-test mask; 0o022 is now current
            let prev = eclipse_umask_chk(0o077);
            assert_eq!(prev, 0o022, "__umask_chk returns the previous mask");
            let now = eclipse_umask_chk(0o022);
            assert_eq!(now, 0o077, "__umask_chk installed the requested mask");
            libc::umask(saved); // restore the pre-test mask
        }
    }

    #[test]
    fn errno_returns_thread_errno_location() {
        let p = eclipse_errno();
        assert!(
            !p.is_null(),
            "__errno must return a non-null errno location"
        );
        // It must equal glibc's __errno_location (same thread-local).
        // SAFETY: both are valid pointers to this thread's errno int.
        let host = unsafe { libc::__errno_location() };
        assert_eq!(p, host, "__errno forwards to the glibc per-thread errno");
    }

    #[test]
    fn system_property_get_reports_unset() {
        let mut buf = [0xAAu8; 92]; // poisoned; the native must NUL it
        let name = b"ro.build.version.sdk\0";
        // SAFETY: `name` is a valid NUL-terminated C string; `buf` is ≥ PROP_VALUE_MAX (92) bytes.
        let n =
            unsafe { eclipse_system_property_get(name.as_ptr().cast(), buf.as_mut_ptr().cast()) };
        assert_eq!(n, 0, "an unset property reports length 0");
        assert_eq!(
            buf[0], 0,
            "the value buffer must be an empty NUL-terminated string"
        );
    }

    // ---- bionic signal ABI (2026-06-11) ---------------------------------------------------------

    #[test]
    fn bionic_sigaction_layout_matches_lp64() {
        // Pin the bionic LP64 x86-64 struct sigaction layout (AOSP bits/signal_types.h):
        // flags@0 (+4 pad), handler@8, mask@16, restorer@24, 32 bytes total. A drift here would
        // re-introduce the scrambled-handler registration the core-dump evidence proved
        // (2026-06-11): glibc reading its handler from offset 0, where bionic keeps sa_flags.
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_flags), 0);
        assert_eq!(std::mem::offset_of!(BionicSigaction, handler), 8);
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_mask), 16);
        assert_eq!(std::mem::offset_of!(BionicSigaction, sa_restorer), 24);
        assert_eq!(std::mem::size_of::<BionicSigaction>(), 32);
        // And the incompatibility being translated away: glibc's sigset_t is 16x wider.
        assert_eq!(std::mem::size_of::<BionicSigsetT>(), 8);
        assert_eq!(std::mem::size_of::<libc::sigset_t>(), 128);
    }

    #[test]
    fn bionic_sigset_ops_match_the_bionic_contract() {
        let mut set: BionicSigsetT = 0xdead_beef;
        // SAFETY: `set` is a valid bionic sigset word for all three ops.
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
            // Out-of-range signum → EINVAL (bit 64 would be signal 65 — beyond the bionic set).
            assert_eq!(eclipse_sigaddset(&mut set, 0), -1);
            assert_eq!(eclipse_sigaddset(&mut set, 65), -1);
            assert_eq!(*libc::__errno_location(), libc::EINVAL);
            // Null set → EINVAL, never a write.
            assert_eq!(eclipse_sigemptyset(std::ptr::null_mut()), -1);
            assert_eq!(eclipse_sigfillset(std::ptr::null_mut()), -1);
        }
    }

    #[test]
    fn bionic_sigset_translation_round_trips() {
        let bionic: BionicSigsetT = (1 << (libc::SIGURG - 1)) | (1 << (libc::SIGUSR2 - 1));
        let glibc = glibc_sigset_from_bionic(bionic);
        // The widened set must answer sigismember exactly like the bionic bits…
        // SAFETY: `glibc` is a valid initialized glibc sigset_t.
        unsafe {
            assert_eq!(libc::sigismember(&glibc, libc::SIGURG), 1);
            assert_eq!(libc::sigismember(&glibc, libc::SIGUSR2), 1);
            assert_eq!(libc::sigismember(&glibc, libc::SIGUSR1), 0);
        }
        // …and narrow back losslessly.
        assert_eq!(bionic_sigset_from_glibc(&glibc), bionic);
    }

    /// The live handler the round-trip test registers: records the signal number it received.
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
        // The regression guard for the confirmed bug: register a SA_SIGINFO handler through the
        // BIONIC-shaped sigaction and prove the KERNEL actually calls it (with the scrambled
        // glibc fall-through, the handler address was garbage and delivery double-faulted).
        // SIGURG: default disposition is IGNORE, so even a broken registration cannot kill the
        // test process; raise() delivers to the calling thread.
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
        // SAFETY: `act`/`old` are valid bionic sigaction structs; SIGURG is a valid signal.
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
        // Restore the saved old action THROUGH the bionic path (the crashpad chain-to-previous
        // pattern) and verify a query round-trips it.
        let mut requeried = old;
        // SAFETY: `old`/`requeried` are valid bionic sigaction structs.
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
        // SAFETY: valid bionic set words; SIG_BLOCK/SIG_SETMASK are the shared kernel constants.
        unsafe {
            assert_eq!(eclipse_sigaddset(&mut block, libc::SIGURG), 0);
            assert_eq!(eclipse_sigprocmask(libc::SIG_BLOCK, &block, &mut prev), 0);
            // The kernel must now report SIGURG blocked — query through glibc to cross-check the
            // translation against the real thread mask.
            let mut host_mask: libc::sigset_t = std::mem::zeroed();
            assert_eq!(
                libc::sigprocmask(libc::SIG_BLOCK, std::ptr::null(), &mut host_mask),
                0
            );
            assert_eq!(libc::sigismember(&host_mask, libc::SIGURG), 1);
            // Restore the previous mask through the bionic path.
            assert_eq!(
                eclipse_sigprocmask(libc::SIG_SETMASK, &prev, std::ptr::null_mut()),
                0
            );
        }
    }

    // ---- bionic netdb resolver ABI (2026-06-12) -------------------------------------------------

    #[test]
    fn bionic_addrinfo_layout_is_bsd_order_and_differs_from_glibc() {
        // THE ABI pin (2026-06-12, the engine DnsResolve root cause): bionic addrinfo tail =
        // canonname@24 / addr@32 (BSD order, public AOSP netdb.h); glibc = addr@24 /
        // canonname@32. A drift here re-opens the exact failure (a bionic walker reading the
        // glibc canonname slot — NULL on effectively every node — as ai_addr → zero usable
        // addresses → CURLE_COULDNT_RESOLVE_HOST).
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_flags), 0);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_family), 4);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_socktype), 8);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_protocol), 12);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addrlen), 16);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_canonname), 24);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_addr), 32);
        assert_eq!(std::mem::offset_of!(BionicAddrinfo, ai_next), 40);
        assert_eq!(std::mem::size_of::<BionicAddrinfo>(), 48);
        // Prove the divergence is REAL on this target (the pin is load-bearing, not vacuous):
        // glibc's tail order through the libc crate is the swap of the bionic one.
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_addr), 24);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_canonname), 32);
        assert_eq!(std::mem::offset_of!(libc::addrinfo, ai_next), 40);
        assert_eq!(std::mem::size_of::<libc::addrinfo>(), 48);
    }

    #[test]
    fn bionic_ai_ni_eai_translation_tables_match_both_headers() {
        // AI_*: every bionic bit maps to the glibc bit of the SAME NAME (bionic values from the
        // public AOSP netdb.h re-fetched 2026-06-12; glibc values via the libc crate = the host
        // header). Undefined bits are EAI_BADFLAGS (bionic-positive 3), never guessed.
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
        // The proven aliasing hazard: bionic AI_ADDRCONFIG (0x400) numerically equals glibc
        // AI_NUMERICSERV — a raw pass-through silently flips the flag's meaning.
        assert_eq!(libc::AI_NUMERICSERV, 0x0400);
        assert_ne!(libc::AI_ADDRCONFIG, 0x0400);
        assert_eq!(
            translate_flags_by_name(0x4000, AI_FLAG_PAIRS),
            Err(BIONIC_EAI_BADFLAGS)
        );
        // NI_*: the scrambled low bits translate by name; DGRAM (16) is identical in both.
        assert_eq!(
            translate_flags_by_name(0x1, NI_FLAG_PAIRS),
            Ok(libc::NI_NOFQDN) // glibc 4
        );
        assert_eq!(
            translate_flags_by_name(0x2, NI_FLAG_PAIRS),
            Ok(libc::NI_NUMERICHOST) // glibc 1
        );
        assert_eq!(
            translate_flags_by_name(0x4, NI_FLAG_PAIRS),
            Ok(libc::NI_NAMEREQD) // glibc 8
        );
        assert_eq!(
            translate_flags_by_name(0x8, NI_FLAG_PAIRS),
            Ok(libc::NI_NUMERICSERV) // glibc 2
        );
        assert_eq!(
            translate_flags_by_name(0x10, NI_FLAG_PAIRS),
            Ok(libc::NI_DGRAM)
        );
        assert_eq!(
            translate_flags_by_name(0x100, NI_FLAG_PAIRS),
            Err(BIONIC_EAI_BADFLAGS)
        );
        // EAI_*: glibc-negative → bionic-positive of the SAME NAME (sign-flip AND renumber —
        // e.g. FAMILY is glibc -6 but bionic 5, NONAME glibc -2 but bionic 8).
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
        // An unmapped/newer glibc code degrades to the generic non-recoverable failure.
        assert_eq!(bionic_eai_from_glibc(-100), BIONIC_EAI_FAIL);
    }

    #[test]
    fn bionic_getaddrinfo_returns_bionic_shaped_nodes_and_positive_eai() {
        // Live round-trip through the bionic shape, fully OFFLINE (AI_NUMERICHOST → no DNS):
        // "127.0.0.1" must yield a node whose ai_addr — read at the BIONIC offset (32, via the
        // typed field) — is a non-NULL AF_INET sockaddr holding 127.0.0.1, and (AI_CANONNAME)
        // whose ai_canonname@24 is the deep-copied numeric string; a NAME under AI_NUMERICHOST
        // is the deterministic bionic-POSITIVE EAI_NONAME; gai_strerror answers the bionic code.
        let node = std::ffi::CString::new("127.0.0.1").expect("cstring");
        // SAFETY: all-zero is a valid bionic addrinfo hints baseline (fields set below).
        let mut hints: BionicAddrinfo = unsafe { std::mem::zeroed() };
        hints.ai_flags = 0x0004 | 0x0002; // bionic AI_NUMERICHOST | AI_CANONNAME
        hints.ai_family = libc::AF_INET;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut BionicAddrinfo = std::ptr::null_mut();
        // SAFETY: valid C string + valid hints + valid out-pointer — the fn's caller contract.
        let rc = unsafe { eclipse_getaddrinfo(node.as_ptr(), std::ptr::null(), &hints, &mut res) };
        assert_eq!(rc, 0, "numeric-host lookup must succeed offline");
        assert!(!res.is_null(), "success must produce a chain");
        // SAFETY: rc==0 ⇒ res points at the Eclipse-owned chain head.
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
        // SAFETY: ai_addrlen says this is a sockaddr_in; the node owns the bytes.
        let sin = unsafe { &*(first.ai_addr.cast::<libc::sockaddr_in>()) };
        assert_eq!(sin.sin_family, libc::AF_INET as libc::sa_family_t);
        assert_eq!(u32::from_be(sin.sin_addr.s_addr), 0x7f00_0001);
        // AI_CANONNAME with a numeric host: glibc reports the numeric string; the deep copy must
        // land it in the BIONIC canonname slot (@24).
        assert!(!first.ai_canonname.is_null(), "AI_CANONNAME requested");
        // SAFETY: canonname is the NUL-terminated copy bionic_node_from_glibc made.
        let canon = unsafe { std::ffi::CStr::from_ptr(first.ai_canonname) };
        assert_eq!(canon.to_str().expect("utf-8"), "127.0.0.1");
        // SAFETY: `res` is the chain eclipse_getaddrinfo returned, freed exactly once.
        unsafe { eclipse_freeaddrinfo(res) };

        // Guaranteed-invalid OFFLINE failure: a non-numeric NAME under AI_NUMERICHOST cannot
        // parse → glibc EAI_NONAME → bionic-positive 8 (the sign every bionic caller — including
        // Roblox's EAI retry classification — branches on).
        let bad = std::ffi::CString::new("not-an-ip.invalid").expect("cstring");
        let mut res2: *mut BionicAddrinfo = std::ptr::null_mut();
        // SAFETY: as above.
        let rc = unsafe { eclipse_getaddrinfo(bad.as_ptr(), std::ptr::null(), &hints, &mut res2) };
        assert_eq!(rc, BIONIC_EAI_NONAME);
        assert!(rc > 0, "bionic EAI codes are POSITIVE");
        assert!(res2.is_null(), "failure must not hand out a chain");
        // SAFETY: gai_strerror takes any code and returns a static string.
        let msg = unsafe { eclipse_gai_strerror(rc) };
        assert!(!msg.is_null());
        // SAFETY: the table entries are static NUL-terminated strings.
        let s = unsafe { std::ffi::CStr::from_ptr(msg) }
            .to_str()
            .expect("ascii");
        assert!(s.contains("not known"), "the NONAME message, got: {s}");
    }

    #[test]
    fn bionic_getnameinfo_translates_flags_and_returns_numeric_host() {
        // Fully offline: BIONIC NI_NUMERICHOST|NI_NUMERICSERV (0x2|0x8) — which as RAW glibc
        // bits would mean NUMERICSERV|NAMEREQD and reverse-resolve 127.0.0.1 to "localhost" (or
        // fail) — must yield exactly "127.0.0.1"/"80", proving the by-name translation is
        // load-bearing. An undefined bionic bit is EAI_BADFLAGS without touching the host.
        // SAFETY: all-zero then field-filled is a valid sockaddr_in.
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = 80u16.to_be();
        sin.sin_addr.s_addr = 0x7f00_0001u32.to_be();
        let mut host = [0 as c_char; 64];
        let mut serv = [0 as c_char; 16];
        // SAFETY: valid sockaddr_in + correctly-sized writable buffers — the caller contract.
        let rc = unsafe {
            eclipse_getnameinfo(
                (&raw const sin).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                host.as_mut_ptr(),
                host.len() as libc::socklen_t,
                serv.as_mut_ptr(),
                serv.len() as libc::socklen_t,
                0x2 | 0x8, // bionic NI_NUMERICHOST | NI_NUMERICSERV
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: rc==0 ⇒ both buffers hold NUL-terminated strings.
        let h = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }
            .to_str()
            .expect("ascii");
        // SAFETY: as above.
        let s = unsafe { std::ffi::CStr::from_ptr(serv.as_ptr()) }
            .to_str()
            .expect("ascii");
        assert_eq!(h, "127.0.0.1");
        assert_eq!(s, "80");
        // Undefined bionic NI bit → bionic-positive EAI_BADFLAGS, host untouched.
        // SAFETY: same valid pointers; the flag word alone is invalid.
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

    // ---- early-fault tap (2026-06-12) -----------------------------------------------------------

    /// The "engine handler" the tap test chains to. Its OWN atomic + handler — never shared with
    /// the SIGURG live test above, so the two parallel-running live tests cannot cross-talk.
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
        // SIGWINCH: default disposition is IGNORE (like SIGURG), so a broken registration cannot
        // kill the test process — and a DIFFERENT signal from the SIGURG live test, so the two
        // never race on one process-global disposition under the parallel test runner.
        let sig = libc::SIGWINCH;

        // (a) Snapshot the kernel's current SIGWINCH action for cleanup.
        // SAFETY: query-only raw sigaction (act null) with a valid zeroed out-param.
        let mut snapshot: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut snapshot), 0);
        }

        // (a2) Seed-BEFORE-install ordering pin (2026-06-12, the reviewed seed-window race): on
        // a signal whose action can be QUERIED but never REPLACED (SIGKILL — sigaction(2)
        // EINVAL; nothing is ever raised here), the install must fail AFTER the chain seed.
        // Under the fixed order (query → seed → install) the failed install leaves exactly one
        // newly claimed pool cell and a published slot with the gate still closed; under the
        // buggy install-then-seed order the install failed first and neither happened — this
        // block fails deterministically, no concurrency needed.
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

        // (b) Install the tap; the KERNEL slot must now be the tap with SA_SIGINFO.
        let cursor_pre_install = TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst);
        install_early_fault_tap(sig).expect("tap install");
        // 2026-06-12: a quiescent install claims exactly ONE pool cell (the query seed) — the
        // oldact re-seed must not fire when nothing re-registered between query and install
        // (pins the TAP_CHAIN_POOL_LEN budget accounting).
        assert_eq!(
            TAP_CHAIN_POOL_NEXT.load(Ordering::SeqCst),
            cursor_pre_install + 1,
            "a quiescent install claims exactly one pool cell (no spurious re-seed)"
        );
        // SAFETY: query-only raw sigaction with a valid zeroed out-param.
        let mut kernel: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut kernel), 0);
        }
        assert_eq!(
            kernel.sa_sigaction, early_fault_tap_handler as *const () as usize,
            "the tap is kernel-registered"
        );
        assert_ne!(kernel.sa_flags & libc::SA_SIGINFO, 0, "SA_SIGINFO is set");

        // (c) Query through the seam: the seeded chain slot reports the pre-tap disposition,
        // restorer never leaked.
        let mut old = BionicSigaction {
            sa_flags: 0,
            handler: usize::MAX,
            sa_mask: !0,
            sa_restorer: usize::MAX,
        };
        // SAFETY: `old` is a valid bionic sigaction out-param; query-only (act null).
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

        // (d) Register the "engine handler" through the bionic path: the previous occupant
        // round-trips via oldact, and the KERNEL slot is STILL the tap (the seam never forwarded
        // — the always-first property, the load-bearing assertion).
        let act = BionicSigaction {
            sa_flags: libc::SA_SIGINFO,
            handler: tap_test_chain_handler as *const () as usize,
            sa_mask: 0,
            sa_restorer: 0,
        };
        let mut old2 = old;
        // SAFETY: `act`/`old2` are valid bionic sigaction structs.
        unsafe {
            assert_eq!(eclipse_sigaction(sig, &act, &mut old2), 0);
        }
        assert_eq!(old2.handler, old.handler, "previous occupant round-trips");
        // SAFETY: query-only raw sigaction with a valid zeroed out-param.
        let mut kernel2: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe {
            assert_eq!(libc::sigaction(sig, std::ptr::null(), &mut kernel2), 0);
        }
        assert_eq!(
            kernel2.sa_sigaction, early_fault_tap_handler as *const () as usize,
            "the kernel slot is STILL the tap — the engine registration never reached the kernel"
        );
        // 2026-06-12: the published chain pointer is a static TAP_CHAIN_POOL cell — the
        // no-heap-in-handler-context guard on the REAL statics (tap_chain_register is reachable
        // inside the fault-handler chain via crashpad's restore-and-reraise flow, so a
        // reintroduced Box/malloc publish must fail here).
        let chain_ptr = TAP_CHAIN.load(Ordering::Acquire) as usize;
        let pool_start = TAP_CHAIN_POOL.0.as_ptr() as usize;
        let pool_end = pool_start + std::mem::size_of_val(&TAP_CHAIN_POOL.0);
        assert!(
            (pool_start..pool_end).contains(&chain_ptr),
            "the chain slot points into the static pool, never the heap"
        );

        // (e) Deliver: kernel → tap (the dump runs — engine range unpublished here, so the
        // dump-everything mode also live-exercises the walker on this thread's real context) →
        // chained engine handler. The latch must be clear afterwards.
        // SAFETY: raise() delivers the signal to the calling thread.
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

        // (f) Cross-thread concurrency (the tid-scoped latch contract, 2026-06-12): while a
        // second thread is PARKED inside the chained handler (its tid holds the latch for the
        // whole chained run — on a real engine fault that window spans crashpad's entire dump),
        // a delivery on THIS thread must still chain — a different-tid entry is concurrency,
        // not recursion. The process-global-bool latch this guards against bailed the
        // concurrent entry to SIG_DFL, stripping the tap+chain from the kernel slot and killing
        // the process on two overlapping recoverable faults.
        static TAP_TEST_PARK_RELEASED: AtomicBool = AtomicBool::new(false);
        static TAP_TEST_CHAIN_ENTRIES: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn tap_test_parking_chain_handler(
            _signum: c_int,
            _info: *mut libc::siginfo_t,
            _ctx: *mut c_void,
        ) {
            // The FIRST entry parks until released; later entries return immediately.
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
        // SAFETY: `park_act` is a valid bionic sigaction; registration goes through the seam.
        unsafe {
            assert_eq!(eclipse_sigaction(sig, &park_act, std::ptr::null_mut()), 0);
        }
        let parker = std::thread::spawn(move || {
            // SAFETY: raise() delivers the signal to the calling (parker) thread.
            unsafe { libc::raise(sig) };
        });
        // Wait until the parker thread is parked inside the chained handler (latch held).
        while TAP_TEST_CHAIN_ENTRIES.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let owner_while_parked = TAP_HANDLER_TID.load(Ordering::SeqCst);
        // Deliver on THIS thread while the parker's tid holds the latch.
        // SAFETY: raise() delivers the signal to the calling thread; returns post-handler.
        unsafe {
            libc::raise(sig);
        }
        // Observe BEFORE asserting, then release + join, so a failed assertion can never leave
        // the parker spinning for the rest of the test run.
        let entries_while_parked = TAP_TEST_CHAIN_ENTRIES.load(Ordering::SeqCst);
        // SAFETY: query-only raw sigaction with a valid zeroed out-param.
        let mut kernel3: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
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

        // (g) Crashpad-style restore: re-register the saved oldact, then re-query — the slot
        // reverts (the proven oldact round-trip pattern).
        // SAFETY: `old` is a valid bionic sigaction.
        unsafe {
            assert_eq!(eclipse_sigaction(sig, &old, std::ptr::null_mut()), 0);
        }
        let mut requeried = act;
        // SAFETY: `requeried` is a valid bionic sigaction out-param; query-only (act null).
        unsafe {
            assert_eq!(eclipse_sigaction(sig, std::ptr::null(), &mut requeried), 0);
        }
        assert_eq!(requeried.handler, old.handler, "the chain slot reverts");

        // (h) Cleanup: close the seam gate, clear the slot (the claimed pool cells stay —
        // claim-once, deliberate), and restore the (a) snapshot raw so no disposition leaks to
        // other tests.
        TAPPED_SIGNAL.store(0, Ordering::SeqCst);
        TAP_CHAIN.store(std::ptr::null_mut(), Ordering::SeqCst);
        // SAFETY: `snapshot` is the valid glibc action captured in (a).
        unsafe {
            assert_eq!(libc::sigaction(sig, &snapshot, std::ptr::null_mut()), 0);
        }
    }

    #[test]
    fn tap_chain_pool_publishes_in_place_and_keeps_last_occupant_on_exhaustion() {
        // 2026-06-12: the regression guard for the handler-context alloc ban — the chain slot
        // must be backed by claim-once static-pool cells, never the heap (tap_chain_register is
        // reachable INSIDE the fault-handler chain; see TAP_CHAIN_POOL). A LOCAL pool/cursor/
        // slot triple (the tap_chain_publish parametrization exists for exactly this) so
        // exhausting it cannot poison the process-global pool the live test shares.
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

        // Every successful publish lands IN the pool (the no-heap property), reads back intact,
        // and claims a fresh cell (superseded cells are never reused — the no-tearing property).
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
            // SAFETY: `p` is the just-published pool cell (immutable after publication).
            assert_eq!(unsafe { (*p).handler }, 0x1000 + k);
        }

        // Exhaustion: the publish reports failure and the slot KEEPS the last occupant.
        let last = slot.load(Ordering::Acquire);
        assert!(!tap_chain_publish(&pool, &next, &slot, mk(0xdead)));
        assert_eq!(
            slot.load(Ordering::Acquire),
            last,
            "exhaustion keeps the last occupant"
        );
        // SAFETY: `last` is a published pool cell (immutable after publication).
        assert_eq!(
            unsafe { (*last).handler },
            0x1000 + (TAP_CHAIN_POOL_LEN - 1)
        );
    }

    #[test]
    fn tap_entry_claim_is_tid_scoped_not_process_global() {
        // 2026-06-12: the regression guard for the cross-thread-kill bug — a different-tid
        // entry while the latch is held is CONCURRENCY and must PROCEED (Unlatched); only the
        // SAME tid re-entering is recursion (SameThreadReentry → the SIG_DFL bail). A LOCAL
        // latch (the tap_chain_publish parametrization pattern) so this never touches the
        // process-global TAP_HANDLER_TID the live test exercises.
        let latch = AtomicI64::new(0);
        // The first entry claims the latch with its tid.
        assert_eq!(tap_entry_claim(&latch, 101), TapEntryClaim::Latched);
        assert_eq!(latch.load(Ordering::SeqCst), 101);
        // The SAME tid re-entering is synchronous recursion → bail.
        assert_eq!(
            tap_entry_claim(&latch, 101),
            TapEntryClaim::SameThreadReentry
        );
        // A DIFFERENT tid while held proceeds — the global-bool bug bailed (killed) here.
        assert_eq!(tap_entry_claim(&latch, 202), TapEntryClaim::Unlatched);
        assert_eq!(
            latch.load(Ordering::SeqCst),
            101,
            "an Unlatched entry never disturbs the owner's claim"
        );
        // The owner releases; the next entry (any tid) claims fresh.
        latch.store(0, Ordering::SeqCst);
        assert_eq!(tap_entry_claim(&latch, 202), TapEntryClaim::Latched);
        assert_eq!(latch.load(Ordering::SeqCst), 202);
    }

    #[test]
    fn tap_stack_walk_bounds_and_validates() {
        // A synthetic SysV frame chain in heap memory: [fp] = next fp (strictly ascending),
        // [fp+8] = a fake return address. Frame k sits at base + k*64; the last frame's next-fp
        // is 0 (the SysV outermost-frame convention), which fails `next > fp` and ends the walk.
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
        let rsp = base.wrapping_sub(64); // below the chain so `fp > rsp` holds
        let mut out = [0u64; 32];

        // The happy path: rip + the 4 chained return addresses (the 5th frame's next is 0).
        let n = tap_stack_walk(rip, rsp, base, &mut out);
        assert_eq!(n, 5);
        assert_eq!(out[0], rip, "frame 0 is RIP itself");
        for k in 0..4u64 {
            assert_eq!(out[(k + 1) as usize], 0x1000_0000 + k);
        }

        // A non-8-aligned fp dies at frame 0.
        assert_eq!(tap_stack_walk(rip, rsp, base + 1, &mut out), 1);
        // fp <= rsp dies at frame 0 (the chain must sit above the interrupted stack pointer).
        assert_eq!(tap_stack_walk(rip, base, base, &mut out), 1);
        // next <= fp is rejected (the strictly-increasing-fp termination guarantee).
        mem[0] = base; // self-loop: next == fp
        assert_eq!(tap_stack_walk(rip, rsp, base, &mut out), 1);
        // A >= 1 MiB frame step is rejected.
        mem[0] = base + (1 << 20);
        assert_eq!(tap_stack_walk(rip, rsp, base, &mut out), 1);

        // A longer chain stops at the 32-entry cap (rip + 31 frames).
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

        // The non-faulting probe: Some for a valid local, None for an unmapped null-page address.
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

    /// A do-nothing restorer stand-in for the strip test below (never called).
    extern "C" fn tap_test_dummy_restorer() {}

    #[test]
    fn tap_si_code_consts_match_kernel_uapi() {
        // 2026-06-12: pinned locally because libc 0.2.186 defines SEGV_MAPERR/SEGV_ACCERR for
        // hurd/aix but NOT linux-gnu; the kernel UAPI (asm-generic/siginfo.h) values are 1 and 2.
        assert_eq!(SEGV_MAPERR, 1);
        assert_eq!(SEGV_ACCERR, 2);

        // The anti-drift guard for the shared back-translation helper (eclipse_sigaction's oldact
        // path + the tap's chain-slot seeding): SA_RESTORER is stripped from sa_flags and the
        // restorer is forced to 0 even when the glibc action carries both.
        // SAFETY: all-zero is a valid glibc sigaction baseline; fields are then set directly.
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
        // 2026-06-12 (core 866509): the pin that fails if the heap-backed 32 KiB ART altstack (or
        // any guard-less/undersized stack) ever becomes the active one again on a thread Eclipse
        // owns. Asserts the three load-bearing properties: (1) sigaltstack(NULL, &ss) reports
        // Eclipse's mmap'd region as the ACTIVE stack, (2) ss_size dominates the measured
        // ~79.2 KiB fatal-chain budget, (3) the page below ss_sp is PROT_NONE — an overflow is a
        // clean guard-page fault, never silent heap zeroing. sigaltstack is per-thread, so the
        // install + probes + restore are self-contained on this test thread.
        // SAFETY: query-only sigaltstack with a valid out-param.
        let mut saved: libc::stack_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            // SAFETY: `saved` is a valid out-param; a null ss makes this a pure query.
            unsafe { libc::sigaltstack(std::ptr::null(), &mut saved) },
            0,
            "query the pre-test altstack state"
        );

        let st = install_guarded_altstack().expect("install the Eclipse guard-paged altstack");

        // (1) The kernel reports Eclipse's region as this thread's ACTIVE alternate stack.
        // SAFETY: `q` is a valid out-param; null ss = pure query.
        let mut q: libc::stack_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        assert_eq!(unsafe { libc::sigaltstack(std::ptr::null(), &mut q) }, 0);
        assert_eq!(
            q.ss_sp as u64, st.ss_sp,
            "the active altstack must be Eclipse's mmap'd region"
        );
        assert_eq!(q.ss_size, st.ss_size, "the active size is Eclipse's");
        assert_eq!(q.ss_flags & libc::SS_DISABLE, 0, "the stack is enabled");

        // (2) Sized for the documented chain budget with headroom (the 32 KiB ART stack fails this).
        assert!(
            st.ss_size >= 2 * ALTSTACK_CHAIN_BUDGET,
            "ss_size {} must dominate the measured ~79.2 KiB fatal-chain budget",
            st.ss_size
        );

        // (3) Guard-page geometry + protection: ss_sp sits exactly one page above the mapping
        // base, the stack region is readable, and the guard page (incl. the 8 bytes just below
        // ss_sp) is PROT_NONE — probed via the tap's process_vm_readv self-probe (EFAULT class).
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

        // Restore the pre-test per-thread state and unmap the test region (the real boot keeps
        // its region for the process lifetime; this cleanup is test hygiene only).
        // SAFETY: `saved` is the kernel's own pre-test report (a disabled state round-trips as
        // SS_DISABLE); restoring it cannot install an invalid stack.
        assert_eq!(
            unsafe { libc::sigaltstack(&saved, std::ptr::null_mut()) },
            0
        );
        // SAFETY: unmap exactly the region install_guarded_altstack mapped for THIS test; the
        // kernel no longer references it (the restore above deregistered it).
        unsafe { libc::munmap(st.guard_base as *mut c_void, st.mapping_len) };
    }

    #[test]
    fn sigaltstack_native_forwards_and_records_caller_attribution() {
        // 2026-06-12 (core 1223806): the pin for the Eclipse-owned sigaltstack forward. A
        // registration through the C-shim native must (1) really reach the kernel (the host
        // round-trip reports OUR stack as the active one — a broken forward fails here), and
        // (2) leave an attribution record naming the registering tid and the caller — the
        // observability core 1223806 lacked (an unwritable registered altstack force_sigsegv'd
        // the process with zero in-process evidence of WHO registered it). sigaltstack is
        // per-thread; the ring is matched by OUR unique ss_sp so parallel tests can't flake it.
        // SAFETY: query-only sigaltstack with a valid out-param (save the pre-test state).
        let mut saved: libc::stack_t = unsafe { std::mem::zeroed() };
        assert_eq!(
            // SAFETY: `saved` is a valid out-param; a null ss makes this a pure query.
            unsafe { libc::sigaltstack(std::ptr::null(), &mut saved) },
            0
        );

        // A pure QUERY through the native must not record anything (no kernel-state change).
        let before_total = altstack_registration_total();
        let mut q: libc::stack_t = unsafe { std::mem::zeroed() };
        // SAFETY: null ss = pure query through the shim; `q` is a valid out-param.
        assert_eq!(unsafe { eclipse_sigaltstack(std::ptr::null(), &mut q) }, 0);
        assert_eq!(
            altstack_registration_total(),
            before_total,
            "a pure query records nothing"
        );

        // Register a real stack THROUGH the native (the engine's path).
        let mut stack = vec![0u8; libc::SIGSTKSZ];
        let ss = libc::stack_t {
            ss_sp: stack.as_mut_ptr() as *mut c_void,
            ss_flags: 0,
            ss_size: stack.len(),
        };
        // SAFETY: `ss` describes the live, writable Vec buffer; null old_ss is the documented
        // "don't report the previous stack" form. Per-thread effect only.
        assert_eq!(
            unsafe { eclipse_sigaltstack(&ss, std::ptr::null_mut()) },
            0,
            "the forward must reach the kernel and succeed"
        );

        // (1) The host round-trip: the kernel's view is OUR stack (the pure-forward proof).
        let mut active: libc::stack_t = unsafe { std::mem::zeroed() };
        // SAFETY: valid out-param; null ss = pure query.
        assert_eq!(
            unsafe { libc::sigaltstack(std::ptr::null(), &mut active) },
            0
        );
        assert_eq!(active.ss_sp as u64, ss.ss_sp as u64);
        assert_eq!(active.ss_size, ss.ss_size);

        // (2) The attribution record: our registration is in the ring, naming THIS tid and a
        // non-null caller (the C shim's __builtin_return_address(0), resolved to a module —
        // here the test binary itself via the host-dladdr fallback).
        // SAFETY: raw SYS_gettid takes no arguments and cannot fail.
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

        // Restore the pre-test per-thread state before the Vec buffer drops.
        // SAFETY: `saved` is the kernel's own pre-test report (a disabled state round-trips as
        // SS_DISABLE); restoring it deregisters the Vec-backed test stack.
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
        // 2026-06-12: pins the public AOSP NDK LP64 stdio ABI — `extern FILE __sF[]` is an array
        // of `struct __sFILE { char __private[152]; }` STRUCTS (8-aligned), with
        // stdin/stdout/stderr = &__sF[0]/&__sF[1]/&__sF[2] = base+0x000/+0x098/+0x130. The
        // pre-fix 24-byte FILE*-pointer table FAILED these: +0x98/+0x130 fell 128/280 bytes past
        // its end inside unrelated Rust statics (core 782252 — crashpad's fputs(&__sF[2]) read a
        // garbage `_lock` there and SIGSEGV'd inside its own crash-logging path).
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

        // The registered `__sF` data symbol IS the backing's address, 8-aligned, and every bionic
        // standard-stream address falls strictly INSIDE the Eclipse-owned object.
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
        // 2026-06-12: the translation contract — the three bionic `&__sF[i]` sentinel addresses
        // map to the GENUINE glibc stream objects; everything else (interior pointers, null, real
        // streams) passes through unchanged.
        let base = eclipse_sf_addr() as usize;
        let s0 = eclipse_sf_translate_stream(base as *mut libc::FILE);
        let s1 = eclipse_sf_translate_stream((base + SF_FILE_STRIDE) as *mut libc::FILE);
        let s2 = eclipse_sf_translate_stream((base + 2 * SF_FILE_STRIDE) as *mut libc::FILE);
        // SAFETY: 2026-06-12 — reading the process-global glibc stdin/stdout/stderr data symbols
        // (stable, process-lifetime pointer reads).
        let (g0, g1, g2) = unsafe { (stdin, stdout, stderr) };
        assert_eq!(s0, g0, "&__sF[0] -> glibc stdin");
        assert_eq!(s1, g1, "&__sF[1] -> glibc stdout");
        assert_eq!(s2, g2, "&__sF[2] -> glibc stderr");

        // Host-fd linkage through the actual NATIVE the bionic import binds to (no output): the
        // sentinel goes in, the standard host fd comes out.
        // SAFETY: 2026-06-12 — `eclipse_fileno` translates the sentinel to the host stream before
        // glibc dereferences anything.
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

        // Non-sentinel pointers pass through untouched (exact-entry match only).
        let interior = (base + 8) as *mut libc::FILE;
        assert_eq!(eclipse_sf_translate_stream(interior), interior);
        let null: *mut libc::FILE = std::ptr::null_mut();
        assert_eq!(eclipse_sf_translate_stream(null), null);

        // THE call shape that killed boot 782252 — crashpad's `fputs(msg, &__sF[2])` — now writes
        // to the real host stderr and returns success (one short line of test-stderr noise, the
        // precedented tap-test posture).
        let msg = std::ffi::CString::new(
            "eclipse __sF regression pin: fputs(&__sF[2]) reaches host stderr\n",
        )
        .unwrap();
        // SAFETY: 2026-06-12 — `msg` is a valid NUL-terminated C string kept alive across the
        // call; the stderr sentinel is translated to the genuine glibc stream.
        let ret =
            unsafe { eclipse_fputs(msg.as_ptr(), (base + 2 * SF_FILE_STRIDE) as *mut libc::FILE) };
        assert!(ret >= 0, "fputs through the stderr sentinel succeeds");
    }

    #[test]
    fn sf_stdio_natives_round_trip_a_real_stream() {
        use std::ffi::CString;

        // 2026-06-12: the pass-through branch — a REAL glibc stream (tmpfile) flows through the
        // translating natives unchanged, proving the forwards are glibc-ABI-correct end to end
        // (write via fputs/fprintf(C shim)/fwrite/fputc, then read back via fscanf(C shim)/fgets/
        // fread, with seek/tell/eof/ungetc cross-checks).
        // SAFETY: tmpfile() returns an owned, open glibc stream (deleted on close) or null.
        let f = unsafe { libc::tmpfile() };
        assert!(!f.is_null(), "tmpfile available");

        let line = CString::new("num 42\n").unwrap();
        let fmt_out = CString::new("%s %d\n").unwrap();
        let word = CString::new("val").unwrap();
        let fmt_in = CString::new("num %d").unwrap();

        // SAFETY: 2026-06-12 — `f` is a live glibc stream for the whole block (closed at the end
        // exactly once); all strings are NUL-terminated CStrings kept alive across the calls; the
        // varargs passed to the C-shim fprintf/fscanf match their `%s %d`/`%d` conversions; all
        // buffers are sized per the lengths passed.
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

            // "num 42\n" (7) + "val 7\n" (6) + "bytes" (5) + '\n' (1) = 19 bytes.
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
            // fscanf("num %d") leaves the line's newline in the stream — consume it via the native.
            assert_eq!(eclipse_getc(f), c_int::from(b'\n'));

            let mut buf = [0u8; 32];
            assert!(!eclipse_fgets(buf.as_mut_ptr().cast(), buf.len() as c_int, f).is_null());
            let got = std::ffi::CStr::from_ptr(buf.as_ptr().cast());
            assert_eq!(got.to_bytes(), b"val 7\n");

            let mut tail = [0u8; 6];
            assert_eq!(eclipse_fread(tail.as_mut_ptr().cast(), 1, 6, f), 6);
            assert_eq!(&tail, b"bytes\n");

            // EOF/error state machinery through the natives.
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
        // 2026-06-12: bionic `__fread_chk(buf, size, count, stream, buf_size)` vs glibc's
        // `__fread_chk(ptr, ptrlen, size, n, stream)` — the host fall-through was shape-mismatched
        // on every argument after the first (the stream arrived in the bound slot and vice versa).
        // This pins the Eclipse native consuming the BIONIC order: a read with `size*count <=
        // buf_size` succeeds and fills the buffer.
        // SAFETY: tmpfile() returns an owned, open glibc stream or null.
        let f = unsafe { libc::tmpfile() };
        assert!(!f.is_null(), "tmpfile available");
        // SAFETY: 2026-06-12 — `f` is a live glibc stream; the buffers are sized per the args.
        unsafe {
            assert_eq!(eclipse_fwrite(b"abcdef".as_ptr().cast(), 1, 6, f), 6);
            assert_eq!(eclipse_fseek(f, 0, libc::SEEK_SET), 0);
            let mut buf = [0u8; 8];
            // bionic order: (buf, size=1, count=6, stream, buf_size=8).
            assert_eq!(
                eclipse_fread_chk(buf.as_mut_ptr().cast(), 1, 6, f, buf.len()),
                6
            );
            assert_eq!(&buf[..6], b"abcdef");
            assert_eq!(eclipse_fclose(f), 0);
        }
    }

    // ---- the provider drives a real GOT-slot fill through the reloc core ------------------------

    #[test]
    fn registered_native_fills_a_got_slot_via_reloc_core() {
        use crate::loader::elf::DynSym;
        use crate::loader::resolve::{Scope, ScopedResolver};
        // One UND import the Eclipse provider owns; one GLOB_DAT reloc naming it.
        let dynsyms = vec![DynSym {
            name: "__android_log_write".to_string(),
            value: 0,
            size: 0,
            bind: 1, // STB_GLOBAL
            sym_type: 2,
            shndx: 0, // SHN_UNDEF → an import
        }];
        let mut scope = Scope::new();
        scope.push(Box::new(EclipseNativeProvider::with_bionic_natives()));
        let resolver = ScopedResolver::new(&scope, &dynsyms);
        // Resolve the symbol to the Eclipse native's address (non-null, strong).
        let eclipse_addr = resolver.resolve_symbol(0).expect("Eclipse native resolves");
        assert!(eclipse_addr != 0);
        // Apply a GLOB_DAT into a GOT slot at offset 0 and read it back.
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

    // ---- ndk-android: AAsset round-trips REAL APK bytes via src/apk -----------------------------

    /// Build an in-memory APK (zip) with the given Stored entries and write it to a unique temp file.
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
        // REAL bytes through Eclipse's own src/apk reader: an assets/ entry opened via
        // AAssetManager_open, read back via AAsset_getBuffer + AAsset_getLength, then closed.
        let payload: &[u8] = b"ECLIPSE-ASSET-CONTENTS-1234567890";
        let apk = write_test_apk("rt", &[("assets/config/app.txt", payload)]);

        // Mint an AAssetManager* over this APK (the boot path would do this via fromJava).
        let mgr_h = ndk_registry::asset_managers()
            .insert(AssetManagerState {
                apk_path: apk.clone(),
            })
            .expect("insert asset manager");
        let mgr = handle_to_ptr::<c_void>(mgr_h);

        // AAssetManager_open("config/app.txt") → real bytes from assets/config/app.txt.
        let name = std::ffi::CString::new("config/app.txt").unwrap();
        // SAFETY: `mgr` is a live Eclipse AAssetManager*; `name` is a valid NUL-terminated C string.
        let asset = unsafe { eclipse_aassetmanager_open(mgr, name.as_ptr(), 0) };
        assert!(!asset.is_null(), "opening a present asset must succeed");

        // AAsset_getLength == payload length.
        // SAFETY: `asset` is a live Eclipse AAsset*.
        let len = unsafe { eclipse_aasset_getlength(asset) };
        assert_eq!(len as usize, payload.len(), "getLength == real byte count");

        // AAsset_getBuffer holds the real bytes.
        // SAFETY: `asset` is live; the returned pointer is valid until AAsset_close and covers `len`.
        let buf = unsafe { eclipse_aasset_getbuffer(asset) };
        assert!(!buf.is_null(), "getBuffer must return the asset bytes");
        // SAFETY: `buf` covers `len` readable bytes (the asset contents) per getBuffer's contract.
        let got = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
        assert_eq!(got, payload, "getBuffer returns the exact APK entry bytes");

        // A missing asset → NULL (bionic contract), not a panic / fake.
        let missing = std::ffi::CString::new("does/not/exist").unwrap();
        // SAFETY: `mgr` live; `missing` valid C string.
        let none = unsafe { eclipse_aassetmanager_open(mgr, missing.as_ptr(), 0) };
        assert!(none.is_null(), "a missing asset must open to NULL");

        // AAsset_close frees the handle; a second close is a harmless no-op (stale, ignored).
        // SAFETY: `asset` is live; closing it is the documented free.
        unsafe { eclipse_aasset_close(asset) };
        // SAFETY: `asset` is now stale; close must be an ignored no-op (no UB / double-free).
        unsafe { eclipse_aasset_close(asset) };
        // getBuffer on the now-stale handle → NULL (rejected by the generation check, never UB).
        // SAFETY: `asset` is stale; the registry rejects it and returns NULL.
        assert!(unsafe { eclipse_aasset_getbuffer(asset) }.is_null());

        ndk_registry::asset_managers().remove(mgr_h).ok();
        std::fs::remove_file(&apk).ok();
    }

    #[test]
    fn aasset_open_with_stale_manager_returns_null() {
        // A stale/fabricated AAssetManager* must open to NULL (typed Err → sentinel), never a deref.
        let stale_mgr = handle_to_ptr::<c_void>(0xDEAD_BEEF_0000_0001);
        let name = std::ffi::CString::new("anything").unwrap();
        // SAFETY: `stale_mgr` is a fabricated handle; the registry rejects it (bounds/gen check).
        let asset = unsafe { eclipse_aassetmanager_open(stale_mgr, name.as_ptr(), 0) };
        assert!(asset.is_null(), "a stale manager handle must open to NULL");
    }

    #[test]
    fn aasset_openfiledescriptor_serves_a_real_fd_with_exact_bytes() {
        // 2026-06-14: AAsset_openFileDescriptor must return a REAL, readable fd backing the asset
        // bytes (Roblox's engine mmaps this path for the large STORED shader packs and treats a
        // `< 0` return as "Error opening shader pack"). Regression guard: fd >= 0, out_start == 0,
        // out_length == len, and the fd's contents are byte-exact. A regression to the old `-1`
        // stub — or any wrong-bytes/offset bug — fails this test.
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
        // SAFETY: `asset` is live; both out-params are valid `off_t*` written only on success.
        let fd = unsafe { eclipse_aasset_openfiledescriptor(asset, &mut start, &mut length) };
        assert!(fd >= 0, "a real fd must back the in-memory asset");
        assert_eq!(start, 0, "asset begins at offset 0 in the backing memfd");
        assert_eq!(length, payload.len() as libc::off_t, "length is the asset len");
        // Read the fd's full contents back and require byte-exactness with the source asset.
        let mut got = vec![0u8; payload.len()];
        let mut off = 0usize;
        while off < got.len() {
            // SAFETY: `fd` is live and owned here; `got[off..]` is a valid writable slice.
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
        assert_eq!(got, payload, "fd contents must be byte-exact with the asset");
        // SAFETY: `fd` is the live owned descriptor we received; close it once.
        unsafe { libc::close(fd) };
        ndk_registry::assets().remove(s).ok();
    }

    // ---- ndk-android: AConfiguration getters return the set device values ----------------------

    #[test]
    fn aconfiguration_getters_return_device_values() {
        let cfg = eclipse_aconfiguration_new();
        assert!(!cfg.is_null(), "AConfiguration_new must allocate");
        let def = default_configuration();

        // SAFETY: `cfg` is a live Eclipse AConfiguration* for every getter below.
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

            // Country / language fill a 2-char buffer (no NUL).
            let mut country = [0u8; 2];
            eclipse_aconfiguration_getcountry(cfg, country.as_mut_ptr().cast());
            assert_eq!(&country, &def.country);
            let mut language = [0u8; 2];
            eclipse_aconfiguration_getlanguage(cfg, language.as_mut_ptr().cast());
            assert_eq!(&language, &def.language);

            // fromAssetManager refills the same handle with the device config (idempotent here).
            eclipse_aconfiguration_fromassetmanager(cfg, std::ptr::null_mut());
            assert_eq!(
                eclipse_aconfiguration_getscreenwidthdp(cfg),
                def.screen_width_dp
            );

            eclipse_aconfiguration_delete(cfg);
        }
        // A getter on the deleted handle → the "unset" 0 sentinel (rejected, never UB).
        // SAFETY: `cfg` is now stale; the registry rejects it.
        assert_eq!(unsafe { eclipse_aconfiguration_getscreenwidthdp(cfg) }, 0);
    }

    // ---- ndk-android: ALooper lifecycle (prepare/addFd/removeFd/stale) -------------------------

    #[test]
    fn alooper_prepare_is_idempotent_per_thread_and_pollonce_returns_documented_sentinels() {
        // prepare returns the same thread looper twice (NDK: idempotent); forThread matches.
        let l1 = eclipse_alooper_prepare(0);
        assert!(!l1.is_null(), "ALooper_prepare must return a looper");
        let l2 = eclipse_alooper_prepare(0);
        assert_eq!(l1, l2, "prepare is idempotent for the calling thread");
        assert_eq!(
            eclipse_alooper_forthread(),
            l1,
            "forThread == the prepared looper"
        );

        // addFd records the fd (returns 1); removeFd removes it (returns 1).
        // SAFETY: `l1` is a live Eclipse ALooper*; callback/data are null (unused).
        let added = unsafe {
            eclipse_alooper_addfd(l1, 7, 1, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(added, 1, "addFd on a valid looper returns 1");
        // SAFETY: `l1` is live.
        let removed = unsafe { eclipse_alooper_removefd(l1, 7) };
        assert_eq!(removed, 1, "removeFd on a valid looper returns 1");

        // pollOnce: a finite timeout with no ready source → TIMEOUT (the real poll(2) timed out, never
        // a fake CALLBACK / fd id). (2026-06-05: an INFINITE pollOnce with no source now legitimately
        // BLOCKS — the looper is real — so it is NOT asserted here; the parked-then-woken infinite poll
        // is covered by `winit_feed_wakes_a_parked_native_pollonce`.)
        // SAFETY: out-params are null (allowed by the contract).
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

        // addFd / removeFd on a stale looper → -1 (rejected, never UB).
        let stale = handle_to_ptr::<c_void>(0xCAFE_0000_0000_0001);
        // SAFETY: `stale` is fabricated; the registry rejects it.
        let bad_add = unsafe {
            eclipse_alooper_addfd(stale, 1, 1, 0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(bad_add, -1, "addFd on a stale looper returns -1");

        // acquire/release are sound no-ops (no panic, no UB).
        // SAFETY: refcount no-ops accept any value.
        unsafe {
            eclipse_alooper_acquire(l1);
            eclipse_alooper_release(l1);
        }
        // Note: the thread-local looper is intentionally not removed (prepare keeps it for the thread).
    }

    // ---- ndk-android: ANativeWindow sound-stub geometry ----------------------------------------

    #[test]
    fn anativewindow_getters_return_real_geometry_and_stale_is_negative() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: JNI args are unused by the stub; any value is accepted.
        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert!(!win.is_null(), "fromSurface must mint a window handle");
        let def = default_native_window();
        // SAFETY: `win` is a live Eclipse ANativeWindow*.
        unsafe {
            assert_eq!(eclipse_anativewindow_getwidth(win), def.width);
            assert_eq!(eclipse_anativewindow_getheight(win), def.height);
            // 2026-06-12: getFormat (libsurface_util_jni's sole pre-load import) reports the slab
            // handle's recorded format — the documented RGBA_8888 default.
            assert_eq!(eclipse_anativewindow_getformat(win), def.format);
            // acquire/release are sound no-ops.
            eclipse_anativewindow_acquire(win);
            eclipse_anativewindow_release(win);
        }
        // A stale/fabricated window → -1 (NDK negative-error contract), never a fake positive size.
        let stale = handle_to_ptr::<c_void>(0xBEEF_0000_0000_0001);
        // SAFETY: `stale` is fabricated; the registry rejects it.
        assert_eq!(unsafe { eclipse_anativewindow_getwidth(stale) }, -1);
        // SAFETY: `stale` is fabricated; rejected.
        assert_eq!(unsafe { eclipse_anativewindow_getheight(stale) }, -1);
        // SAFETY: `stale` is fabricated; rejected (the same negative-error contract for getFormat).
        assert_eq!(unsafe { eclipse_anativewindow_getformat(stale) }, -1);
        // Free the live window's slot to keep the registry tidy.
        ndk_registry::native_windows()
            .remove(ptr_to_handle(win))
            .ok();
    }

    #[test]
    fn anativewindow_fromsurface_reports_published_live_window_geometry() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 2026-06-05: the engine's ANativeWindow geometry must reflect Eclipse's REAL live window
        // (the EGL surface presents to it), not the fixed phone default. Publishing a geometry then
        // minting a window must surface that geometry through the getters.
        ndk_registry::set_engine_window_geometry(1600, 900);
        // SAFETY: JNI args unused by the stub; any value accepted.
        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert!(!win.is_null());
        // SAFETY: `win` is a live Eclipse ANativeWindow*.
        unsafe {
            assert_eq!(eclipse_anativewindow_getwidth(win), 1600, "live width");
            assert_eq!(eclipse_anativewindow_getheight(win), 900, "live height");
        }
        ndk_registry::native_windows()
            .remove(ptr_to_handle(win))
            .ok();
    }

    #[test]
    fn anativewindow_fromsurface_returns_the_real_wsi_handle_when_registered() {
        let _guard = ANW_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 2026-06-05 — the engine render WSI bind: when the render path has registered a REAL WSI
        // native-window pointer (the EGLNativeWindowType host EGL accepts — a wl_egl_window*/XID),
        // ANativeWindow_fromSurface must return THAT pointer (so the engine's own
        // eglCreateWindowSurface lands on Eclipse's window), and the geometry getters must resolve it
        // via the WSI map. A fake-but-pointer-shaped value stands in for the real WSI pointer here (no
        // window/GPU in a unit test); the binding logic is identical.
        let fake_wsi: usize = 0x7F00_1234_5670; // 16-byte aligned, never a slab handle (gen high bits)
        ndk_registry::register_wsi_window(fake_wsi, 1280, 720);

        // SAFETY: JNI args unused; any value accepted.
        let win = unsafe {
            eclipse_anativewindow_fromsurface(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(
            win as usize, fake_wsi,
            "fromSurface must return the real WSI handle the engine passes to host eglCreateWindowSurface"
        );
        // SAFETY: `win` is the registered WSI pointer; the getters resolve it via the WSI map (no deref).
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
            // 2026-06-12: a registered WSI window reports Eclipse's surface format (RGBA_8888).
            assert_eq!(
                eclipse_anativewindow_getformat(win),
                WINDOW_FORMAT_RGBA_8888,
                "WSI format is Eclipse's RGBA_8888 surface format"
            );
            // acquire/release are sound no-ops on the WSI handle.
            eclipse_anativewindow_acquire(win);
            eclipse_anativewindow_release(win);
        }
        // After unregister, the now-unknown pointer falls through to the slab → -1 (no stale geometry).
        ndk_registry::unregister_wsi_window(fake_wsi);
        assert_eq!(
            ndk_registry::wsi_window_geometry(fake_wsi),
            None,
            "an unregistered WSI pointer is unknown (the getters then return the NDK -1 sentinel)"
        );
    }

    #[test]
    fn resolve_egl_display_target_maps_default_display_to_winit_wayland_only() {
        // 2026-06-13 — the confirmed-root-cause regression guard for the EGL_BAD_ALLOC 3003
        // connection mismatch. The engine's eglGetDisplay(EGL_DEFAULT_DISPLAY=0) on Wayland MUST be
        // remapped to the registered winit wl_display; everything else passes through unchanged. Pure +
        // deterministic (no JVM, no registry), so this is the smallest check that fails if the mapping
        // logic regresses.
        let winit_wl_display: usize = 0x5000_1000;
        // (a) EGL_DEFAULT_DISPLAY on Wayland remaps to the registered winit wl_display — the exact bug.
        assert_eq!(
            resolve_egl_display_target(0, Some(winit_wl_display)),
            winit_wl_display,
            "EGL_DEFAULT_DISPLAY on Wayland remaps to the registered winit wl_display"
        );
        // (b) EGL_DEFAULT_DISPLAY with no Wayland display (X11/other) passes through unchanged —
        //     preserves X11/NVIDIA, where the XID is server-scoped so cross-connection is fine.
        assert_eq!(
            resolve_egl_display_target(0, None),
            0,
            "EGL_DEFAULT_DISPLAY on X11/other passes through unchanged"
        );
        // (c) a caller-chosen non-default display is NEVER rewritten, even on Wayland.
        assert_eq!(
            resolve_egl_display_target(0xABCD, Some(winit_wl_display)),
            0xABCD,
            "a non-default display_id is never rewritten (Wayland)"
        );
        // (d) a non-default display with no Wayland display also passes through.
        assert_eq!(
            resolve_egl_display_target(0xABCD, None),
            0xABCD,
            "a non-default display_id is never rewritten (X11/other)"
        );
    }

    // ---- media-ndk: sound-stub sentinels --------------------------------------------------------

    #[test]
    fn media_ndk_natives_return_unavailable_sentinels() {
        // Pointer-returning codec/format constructors → NULL (the documented failure).
        // SAFETY: the C-string mime arg is not dereferenced by the stub; null is accepted.
        assert!(unsafe { eclipse_amediacodec_createdecoderbytype(std::ptr::null()) }.is_null());
        // SAFETY: see above.
        assert!(unsafe { eclipse_amediacodec_createencoderbytype(std::ptr::null()) }.is_null());
        assert!(eclipse_amediaformat_new().is_null());
        // SAFETY: codec arg is NULL (never dereferenced); getOutputFormat → NULL.
        assert!(unsafe { eclipse_amediacodec_getoutputformat(std::ptr::null_mut()) }.is_null());

        // media_status_t-returning fns → AMEDIA_ERROR_UNSUPPORTED (a caller checks != AMEDIA_OK).
        // SAFETY: codec arg is NULL (never dereferenced) for each status-returning stub.
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
        // The documented numeric value of AMEDIA_ERROR_UNSUPPORTED (NdkMediaError.h: BASE-9).
        assert_eq!(AMEDIA_ERROR_UNSUPPORTED, -10009);

        // ssize_t dequeue fns → negative error (a caller checks < 0).
        // SAFETY: codec/info args are NULL (never dereferenced).
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

        // bool getters → false (key absent / no format). Out-params untouched.
        // SAFETY: format/name/out args are NULL (never dereferenced when returning false).
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

        // toString → a stable, non-null, EMPTY C string (never NULL → no printf crash).
        // SAFETY: format arg is NULL (never dereferenced); the return is a process-lifetime static.
        let s = unsafe { eclipse_amediaformat_tostring(std::ptr::null_mut()) };
        assert!(!s.is_null(), "toString must never return NULL");
        // SAFETY: `s` points at the static EMPTY_CSTR (a single NUL byte).
        assert_eq!(unsafe { *s }, 0, "toString returns an empty string");
    }

    #[test]
    fn amediaformat_key_data_objects_hold_the_public_key_strings() {
        // Each AMEDIAFORMAT_KEY_* data symbol's VALUE is a `const char*` to the canonical key string.
        // The registered address is a `*const c_char` (the data object); read it and check the string.
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
            // SAFETY: the data symbol stores a `*const c_char`; read it and the string it points to.
            let strp = unsafe { *(addr as *const *const c_char) };
            assert!(!strp.is_null(), "{name} value (the char*) must be non-null");
            // SAFETY: `strp` is a valid NUL-terminated static key string.
            let got = unsafe { std::ffi::CStr::from_ptr(strp) };
            assert_eq!(got.to_str().unwrap(), want, "{name} == \"{want}\"");
        }
    }

    // ---- audio: real OpenSL ES engine wiring ----------------------------------------------------

    #[test]
    fn sl_create_engine_via_provider_produces_a_real_engine() {
        // 2026-06-05: slCreateEngine now returns a WORKING SLObjectItf (super::opensl). Confirm the
        // provider's registered address creates a non-null engine the caller can Destroy. The full
        // create→mix→player→Enqueue path is exercised in `super::opensl::tests` + the __audio-test
        // harness; here we only confirm the provider wiring reaches the real engine.
        let p = EclipseNativeProvider::with_bionic_natives();
        let addr = p.resolve("slCreateEngine").expect("registered").addr;
        assert!(
            addr != 0,
            "slCreateEngine must resolve to an Eclipse address"
        );
        // SAFETY: `addr` is super::opensl::eclipse_sl_create_engine; call it with a valid out-param.
        let create: unsafe extern "C" fn(
            *mut c_void,
            u32,
            *const c_void,
            u32,
            *const c_void,
            *const c_void,
        ) -> u32 = unsafe { std::mem::transmute::<u64, _>(addr) };
        let mut engine: *mut c_void = std::ptr::null_mut();
        // SAFETY: out-param is a valid writable SLObjectItf*; other args are unused by the impl.
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
        // Destroy it via the object vtable to free the registry slot (no leak).
        super::super::opensl::destroy_object_for_test(engine);
    }

    #[test]
    fn sl_iid_data_objects_are_stable_distinct_nonnull_pointers() {
        // Each SL_IID_* data symbol's VALUE is an `SLInterfaceID` (a pointer to a 128-bit struct).
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
            // SAFETY: the data symbol stores an `SLInterfaceID` (a *const SlInterfaceId); read it.
            let iid = unsafe { *(addr as *const *const SlInterfaceId) };
            assert!(
                !iid.is_null(),
                "{name} interface-id pointer must be non-null"
            );
            // Distinct backing struct per IID.
            assert!(
                iface_ptrs.insert(iid as usize),
                "{name} must be a distinct IID"
            );
        }
        // Stability: a second resolve returns the same data-symbol address (process-lifetime static).
        assert_eq!(
            p.resolve("SL_IID_ENGINE").unwrap().addr,
            EclipseNativeProvider::with_bionic_natives()
                .resolve("SL_IID_ENGINE")
                .unwrap()
                .addr,
            "SL_IID_ENGINE address is stable across providers"
        );
    }

    // ---- ALooper natives end-to-end (the engine's real input-loop surface) ---------------------
    //
    // 2026-06-05: drive the actual `extern "C"` ALooper natives (prepare → addFd → pollOnce →
    // removeFd) on the calling thread, proving the C-ABI surface a libroblox worker uses works
    // through the real fd-backed looper + the generational registry. These run on their own threads
    // so each gets a fresh thread-local looper (`ALooper_prepare` is per-thread).

    /// A self-contained pipe whose write end signals the read end POLLIN-ready — a stand-in for an
    /// engine input source registered via `ALooper_addFd`. RAII-closes both ends.
    struct NativeTestPipe {
        read: std::os::fd::OwnedFd,
        write: std::fs::File,
    }
    impl NativeTestPipe {
        fn new() -> Self {
            use std::os::fd::FromRawFd;
            let mut fds = [0i32; 2];
            // SAFETY: test-only — pipe2 writes two fresh fds; both taken into RAII owners below.
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert_eq!(rc, 0, "pipe2");
            // SAFETY: test-only — fresh exclusively-owned fds from pipe2.
            let read = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
            // SAFETY: test-only — fresh exclusively-owned fd from pipe2.
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
            // forThread returns the same handle (prepare is idempotent / thread-local).
            assert_eq!(eclipse_alooper_forthread(), looper, "forThread == prepared");
            // No fds, finite timeout → POLL_TIMEOUT (real poll honored the timeout, no fake event).
            // SAFETY: test-only — null out-params are allowed by the contract.
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
        // The pipe outlives the thread (signaled after the thread parks via a channel handshake).
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<i32>();
        let h = std::thread::spawn(move || {
            let looper = eclipse_alooper_prepare(0);
            assert!(!looper.is_null());
            const IDENT: c_int = 42;
            // SAFETY: test-only — `looper` is a valid Eclipse handle; `fd` stays open (owned by the
            // outer pipe); callback null + ident >= 0 is the supported ident form.
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
            // SAFETY: test-only — valid writable out-params; blocking poll until the fd fires.
            let rc = unsafe {
                eclipse_alooper_pollonce(1000, &mut out_fd, &mut out_events, std::ptr::null_mut())
            };
            assert_eq!(rc, IDENT, "pollOnce returns the registered ident");
            assert_eq!(out_fd, fd, "out_fd is the fd that fired");
            assert!(out_events & 1 != 0, "out_events reports POLLIN");
            done_tx.send(rc).expect("signal done");
        });
        ready_rx.recv().expect("thread registered the fd");
        // Now signal the source; the parked pollOnce must wake and return the ident.
        pipe.signal();
        let rc = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("pollOnce woke");
        assert_eq!(rc, 42);
        h.join().expect("looper thread");
    }

    #[test]
    fn winit_feed_wakes_a_parked_native_pollonce() {
        // Proves the engine-path winit feed (`wake_all_loopers`) wakes a parked `ALooper_pollOnce`.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<i32>();
        let h = std::thread::spawn(move || {
            let looper = eclipse_alooper_prepare(0);
            assert!(!looper.is_null());
            ready_tx.send(()).expect("ready");
            // Indefinite block; only a wake can return it (no fds, negative timeout).
            // SAFETY: test-only — null out-params allowed.
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
            // A non-null callback is unsupported (Eclipse uses the ident form) → -1, not a silent drop.
            // Use a pointer to a real local so it is genuinely non-null (and never dereferenced).
            let mut sentinel: u8 = 0;
            let cb = std::ptr::addr_of_mut!(sentinel).cast::<c_void>();
            // SAFETY: test-only — `cb` is never dereferenced (rejected before use).
            let r1 = unsafe { eclipse_alooper_addfd(looper, 3, 1, 1, cb, std::ptr::null_mut()) };
            assert_eq!(r1, -1, "callback form rejected");
            // A negative ident with no callback is invalid per the NDK → -1.
            // SAFETY: test-only.
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
            // This thread never prepared a looper → POLL_ERROR (NDK "no associated looper"), no panic.
            // SAFETY: test-only — null out-params allowed.
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
        // A fabricated/stale ALooper* must be rejected by the registry → -1, never a wild deref.
        let fabricated = handle_to_ptr::<c_void>(0xDEAD_0000_0001);
        // SAFETY: test-only — the registry validates the handle and rejects it before any fd use.
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
        // SAFETY: test-only.
        let rem = unsafe { eclipse_alooper_removefd(fabricated, 5) };
        assert_eq!(rem, -1, "removeFd on a fabricated handle is -1");
    }

    // ---- winit → looper input feed policy ------------------------------------------------------
    //
    // 2026-06-05: winit's `WindowEvent` cannot be constructed in a test (its `DeviceId` is
    // `pub(crate)`), so the winit→kind mapping (`classify_winit_event`) is a thin obvious match and
    // the testable policy is the kind→wake decision over the constructible `HostInputKind`.

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
        // None = a non-input winit event (RedrawRequested/Resized/CloseRequested/…) → must NOT wake.
        assert!(
            !host_input_should_wake(None),
            "non-input events must not wake the engine input loop"
        );
    }

    #[test]
    fn full_input_path_run_input_test_succeeds() {
        // The dev-host harness is also a unit test: it is GPU/VM-free and self-contained (a pipe + a
        // worker thread), so it runs in the suite and proves the synthetic touch DOWN→fd→pollOnce(ident)
        // and the host-input-wake→pollOnce(WAKE) path end-to-end through the real natives.
        match run_input_test() {
            Ok(report) => assert!(report.contains("ALOOPER_POLL_WAKE"), "report: {report}"),
            Err(e) => panic!("run_input_test failed: {e}"),
        }
    }
}
