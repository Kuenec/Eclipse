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
//! ## ndk-android (libandroid) tier — the 27 NDK natives (added 2026-06-05)
//! The second Eclipse-native category: the 27 `libandroid` C-ABI imports from
//! `docs/bionic-env-worklist.md`. Each is labelled at its definition:
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
//! - **ANativeWindow (5) — sound-stub:** the getters return the real window geometry; the
//!   surface/buffer bits whose real behavior is the upcoming GLES2/EGL render integration return
//!   documented sound sentinels (valid-but-empty handle / negative error per the NDK contract) so
//!   resolution + early init proceed WITHOUT pretending a frame was presented. Deferred-to-render.
//!
//! ## media-ndk (libmediandk, 33) + audio (OpenSL ES, 8) — sound-stubs (added 2026-06-05)
//! The final two work-list categories. Both are **gameplay-time** subsystems (video playback, sound)
//! — NOT needed to start/render — so the soundest minimal step is a contract-correct "unavailable"
//! stub: each native returns its public-ABI failure/unavailable sentinel so a caller cleanly detects
//! "no media / no audio" and never acts on a fabricated success. NO global state, NO UB.
//! - **media-ndk (33) — sound-stub: media playback deferred (gameplay-time):** `AMediaCodec_*` /
//!   `AMediaFormat_*` pointer-returning fns → `NULL`; [`media_status_t`](MEDIA_STATUS)-returning fns
//!   → `AMEDIA_ERROR_UNSUPPORTED`; the `ssize_t` dequeue fns → that error (negative); `bool` getters
//!   → `false`; `delete`/setters → safe no-ops; `AMediaFormat_toString` → a stable empty C string.
//!   The 10 `AMEDIAFORMAT_KEY_*` are real `const char*` data objects holding the documented public
//!   key strings (minimal-correct data, not a stub).
//! - **audio (8) — sound-stub: audio deferred (gameplay-time):** `slCreateEngine` →
//!   `SL_RESULT_FEATURE_UNSUPPORTED` (the public OpenSL ES result a caller checks for "no audio");
//!   the 7 `SL_IID_*` are real, stable, distinct `SLInterfaceID` data objects (valid non-null
//!   addresses; never queried because `slCreateEngine` fails first).
//!
//! ## What this is NOT (honest scope, dated 2026-06-05)
//! Registering a correct address makes the relocation land *and* (for the forward/minimal/real
//! natives) makes a **call** to that symbol behave per its public contract. It does **not** by itself
//! make `libroblox.so` runnable — that needs the rest of the work-list (media-ndk / audio + the 2
//! variadic liblog), binding the image to execution, and running the `DT_INIT_ARRAY` constructors
//! (the runtime tail, main-loop / dev-host only). The ANativeWindow surface/buffer natives are
//! explicitly **deferred to the render integration** (documented sound sentinels until then).

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

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
/// fixed-arity) and bionic-specific libc (15) natives implemented in this module. Prepended before
/// the host baseline in [`super::bionic_env::BionicEnv`] so Eclipse's bionic-correct impls win.
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

    /// Build the provider with the **fixed-arity liblog (3)** + **bionic-specific libc (15)** +
    /// **ndk-android libandroid (27)** natives this module implements registered. Taking each
    /// native's address is safe Rust (a function/data item coerced to a pointer then to `u64`).
    ///
    /// The names are the real work-list from `loader::link::tests::real_libroblox_bionic_env_*`
    /// (`docs/bionic-env-worklist.md`): liblog's full 5 (the 3 fixed-arity Rust natives plus the 2
    /// **variadic** ones — `__android_log_print`/`__android_log_assert` — now DEFINED by the
    /// clean-room C shim, 2026-06-05); bionic-libc's 15; ndk-android's 27 (AAsset* real via
    /// `src/apk`, AConfiguration/ALooper minimal-correct, ANativeWindow sound-stub); media-ndk's 33
    /// and audio's 8 sound-stubs. **88** symbols total — registering them shrinks the engine's
    /// work-list from 88 to **0** (FULL resolution of all 584 libroblox imports to Eclipse/host).
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

        // ---- bionic-specific libc (15) — glibc lacks these exact names --------------------------
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

        // ---- ndk-android (libandroid) — the 27 NDK natives -------------------------------------
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
        // ANativeWindow (5) — SOUND-STUB; getters return real geometry, refcount ops are no-ops.
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

        // ---- audio (OpenSL ES) — the 8 audio natives (sound-stub: gameplay-time) ----------------
        // slCreateEngine → SL_RESULT_FEATURE_UNSUPPORTED so the caller cleanly detects "no audio".
        p.register(
            "slCreateEngine",
            eclipse_sl_create_engine as *const () as u64,
        );
        // SL_IID_* (7) — DATA objects of type `SLInterfaceID` (a pointer to a 128-bit interface UUID
        // struct). Each resolves to a stable, valid, distinct Eclipse-owned `SLInterfaceID_` object so
        // the relocation has a real non-null address; audio being unavailable, no engine ever queries
        // them (slCreateEngine fails first).
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
// liblog — the 2 VARIADIC natives: clean-room C shim → Eclipse sink (added 2026-06-05).
// =================================================================================================
//
// `__android_log_print` / `__android_log_assert` are C-variadic; Rust stable cannot DEFINE a
// variadic `extern "C"` fn (`c_variadic` is nightly-only) but it CAN declare one and take its
// address. The definitions live in the clean-room C shim `src/loader/liblog_shim.c` (compiled by
// build.rs via the `cc` crate); each formats its varargs with `vsnprintf` into a bounded stack
// buffer and forwards the finished line to the Eclipse-owned non-variadic sink below.

extern "C" {
    /// `int __android_log_print(int prio, const char* tag, const char* fmt, ...)` — DEFINED in the
    /// C shim (`src/loader/liblog_shim.c`). Variadic externs are stable to declare; the address is
    /// taken in [`EclipseNativeProvider::with_bionic_natives`] to bind the engine's relocation.
    fn __android_log_print(prio: c_int, tag: *const c_char, fmt: *const c_char, ...) -> c_int;

    /// `void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...)` —
    /// DEFINED in the C shim (noreturn: emits FATAL then `abort()`). Address-only use here.
    fn __android_log_assert(cond: *const c_char, tag: *const c_char, fmt: *const c_char, ...);
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
// bionic-specific libc (15) — names glibc does not export under these exact identifiers.
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
/// — bionic FORTIFY fwrite. Aborts if `size * count > buf_size`. **forward.**
///
/// # Safety
/// `buf` must point to at least `buf_size` readable bytes; `stream` must be a valid `FILE*`.
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
    // SAFETY: 2026-06-05 — after the check, `buf` has ≥ `size*count` readable bytes and `stream` is
    // a valid `FILE*`; glibc `fwrite` reads that many bytes and writes them, ABI-identical.
    unsafe { libc::fwrite(buf, size, count, stream) }
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

/// bionic's `__sF` — the stdio `FILE` table whose first three entries back `stdin`/`stdout`/`stderr`
/// (bionic's `stdin`/`stdout`/`stderr` macros expand to `&__sF[0..2]`). glibc has **no** `__sF` (its
/// `stdin`/… are individual `FILE*` objects). **forward (host stdio handles):** Eclipse points `__sF`
/// at a small Eclipse-owned table of three glibc `FILE*` (stdin/stdout/stderr), so a relocated
/// reference to `__sF[i]` yields a usable host stdio stream.
///
/// 2026-06-05 — HONEST scope: the bionic `FILE` struct layout differs from glibc's, so this is sound
/// for code that takes `&__sF[i]` and passes the resulting `FILE*` straight to a libc stdio call that
/// ALSO forwards to glibc (the pointer round-trips); it is NOT layout-correct for code that pokes
/// bionic `FILE` fields directly. The three host streams are the closest real, usable backing
/// (documented; not a silent fake — the pointers are genuine glibc streams).
struct SfTable([*mut libc::FILE; 3]);
// SAFETY: 2026-06-05 — the table holds the process-global glibc stdio handles (`stdin`/`stdout`/
// `stderr`), valid for the whole process lifetime and shared across threads by glibc (it locks
// internally). Storing/sharing these raw pointers is sound (read-only, never closed).
unsafe impl Sync for SfTable {}
// SAFETY: see the `Sync` note — process-lifetime, never-closed glibc stdio handles.
unsafe impl Send for SfTable {}

extern "C" {
    // glibc's stdio handle data symbols (`extern FILE *stdin, *stdout, *stderr;`). The `libc` crate
    // does not re-export these statics, so bind them directly (the default symbol version links).
    // 2026-06-05.
    static stdin: *mut libc::FILE;
    static stdout: *mut libc::FILE;
    static stderr: *mut libc::FILE;
}

static ECLIPSE_SF: OnceLock<SfTable> = OnceLock::new();

/// Initialize (once) Eclipse's `__sF` table from the host stdio handles and return its address.
fn eclipse_sf_addr() -> u64 {
    let t = ECLIPSE_SF.get_or_init(|| {
        // SAFETY: 2026-06-05 — `stdin`/`stdout`/`stderr` are the process-global glibc stdio `FILE*`
        // data symbols (valid for the process lifetime). We snapshot the three pointers into an
        // Eclipse-owned table; reading these externs is a plain pointer read of stable globals.
        unsafe { SfTable([stdin, stdout, stderr]) }
    });
    std::ptr::addr_of!(t.0) as u64
}

// =================================================================================================
// ndk-android (libandroid) — the 27 NDK natives. Opaque NDK pointers are Eclipse-owned generational
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

/// Eclipse's default [`NativeWindowState`]: the default portrait display geometry, RGBA8888.
fn default_native_window() -> NativeWindowState {
    NativeWindowState {
        width: DEFAULT_DISPLAY_WIDTH_PX,
        height: DEFAULT_DISPLAY_HEIGHT_PX,
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

/// `int AAsset_openFileDescriptor(AAsset* asset, off_t* outStart, off_t* outLength)` — get a file
/// descriptor for direct asset access. **sound-stub:** Eclipse serves assets from in-memory bytes
/// (read from the APK zip), so there is no backing file descriptor for the asset region. The NDK
/// contract is to return `< 0` "if direct fd access is not possible (for example, if the asset is
/// compressed)" — Eclipse returns `-1`, the documented sound failure, so callers fall back to
/// `AAsset_getBuffer`/`AAsset_read` (which Eclipse serves with real bytes). NOT a fake fd.
///
/// # Safety
/// `asset` must be an `AAsset*` from an Eclipse asset native; `out_start`/`out_length` are null or
/// valid `off_t*`. This native writes neither (it returns the failure sentinel), so they are unused.
unsafe extern "C" fn eclipse_aasset_openfiledescriptor(
    _asset: *mut c_void,
    _out_start: *mut libc::off_t,
    _out_length: *mut libc::off_t,
) -> c_int {
    -1 // direct fd access not possible (in-memory asset) — bionic's documented "< 0" → buffer fallback
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

// ---- ALooper (7) — MINIMAL-CORRECT Eclipse per-thread looper ------------------------------------
//
// 2026-06-05: a thread-local Eclipse looper handle + an fd-registry. `pollOnce` returns the
// documented `ALOOPER_POLL_*` sentinel (TIMEOUT when given a finite timeout, ERROR when blocking
// forever with no event source) — a sentinel the NDK contract requires callers to handle, NOT a
// fake "an event happened" success. Real epoll wiring is deferred until there is an event source
// (the render/input integration).

// (`ALOOPER_POLL_WAKE` = -1 and `ALOOPER_POLL_CALLBACK` = -2 are part of the public looper contract
// but Eclipse's natives never return them — `ALooper_wake` is not in libroblox's 27-symbol set and
// `pollOnce` never fakes a CALLBACK — so they are intentionally not defined here. 2026-06-05.)
/// `ALOOPER_POLL_TIMEOUT` = -3: no data before the timeout expired. From `<android/looper.h>`.
const ALOOPER_POLL_TIMEOUT: c_int = -3;
/// `ALOOPER_POLL_ERROR` = -4: no associated looper / unrecoverable error. From `<android/looper.h>`.
const ALOOPER_POLL_ERROR: c_int = -4;

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
        match ndk_registry::loopers().insert(LooperState::default()) {
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

/// `int ALooper_pollOnce(int timeoutMillis, int* outFd, int* outEvents, void** outData)` — wait for
/// an event. **minimal-correct (documented sentinel, NOT a fake success):** Eclipse's looper has no
/// event source yet (deferred to the render/input integration), so it cannot deliver a real event.
/// Per the NDK contract it clears the out-params and returns the *correct* sentinel for the situation:
/// `ALOOPER_POLL_TIMEOUT` for a finite (≥ 0) timeout (no data arrived), and `ALOOPER_POLL_ERROR` for
/// an infinite (< 0) wait — blocking forever with no source would hang, so reporting the
/// no-source error is the sound, non-hanging answer a caller must handle. Never returns
/// `ALOOPER_POLL_CALLBACK` or a fd id (no fake "an event happened").
///
/// # Safety
/// `out_fd`/`out_events`/`out_data` must each be null or valid writable pointers (the NDK contract);
/// this native writes the documented "no event" values to the non-null ones.
unsafe extern "C" fn eclipse_alooper_pollonce(
    timeout_millis: c_int,
    out_fd: *mut c_int,
    out_events: *mut c_int,
    out_data: *mut *mut c_void,
) -> c_int {
    // Clear the out-params to the "no fd / no events / no data" values (NDK: set when no fd fires).
    if !out_fd.is_null() {
        // SAFETY: 2026-06-05 — caller-provided writable `int*` per the contract; write the no-fd value.
        unsafe { out_fd.write(0) };
    }
    if !out_events.is_null() {
        // SAFETY: 2026-06-05 — caller-provided writable `int*`; write the no-events value.
        unsafe { out_events.write(0) };
    }
    if !out_data.is_null() {
        // SAFETY: 2026-06-05 — caller-provided writable `void**`; write the no-data null.
        unsafe { out_data.write(std::ptr::null_mut()) };
    }
    if timeout_millis >= 0 {
        ALOOPER_POLL_TIMEOUT // finite wait, nothing arrived — the honest sentinel
    } else {
        ALOOPER_POLL_ERROR // infinite wait with no event source: report error, never hang
    }
}

/// `int ALooper_addFd(ALooper* looper, int fd, int ident, int events, ALooper_callbackFunc callback,
/// void* data)` — register a file descriptor with the looper. **minimal-correct:** records `(fd,
/// ident)` in the Eclipse looper's fd set (bookkeeping-correct for `removeFd`); returns `1` on
/// success and `-1` on failure (the NDK contract). Eclipse does not yet poll the fd (no epoll until
/// the event integration), so `pollOnce` will not deliver its events — documented, not a fake.
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native; `callback`/`data` are stored by value
/// (callback unused here) and are not dereferenced.
unsafe extern "C" fn eclipse_alooper_addfd(
    looper: *mut c_void,
    fd: c_int,
    ident: c_int,
    _events: c_int,
    _callback: *mut c_void,
    _data: *mut c_void,
) -> c_int {
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| l.fds.push((fd, ident))) {
        Ok(()) => 1,  // NDK: 1 on success
        Err(_) => -1, // NDK: -1 on failure (stale/fabricated looper handle)
    }
}

/// `int ALooper_removeFd(ALooper* looper, int fd)` — unregister a file descriptor. **minimal-correct:**
/// removes all entries for `fd` from the Eclipse looper's fd set; returns `1` if the looper handle is
/// valid (the fd may or may not have been present — the NDK returns 1 for "removed or not present"),
/// `-1` for a stale/fabricated handle.
///
/// # Safety
/// `looper` must be an `ALooper*` from an Eclipse looper native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_alooper_removefd(looper: *mut c_void, fd: c_int) -> c_int {
    match ndk_registry::loopers().with(ptr_to_handle(looper), |l| l.fds.retain(|&(f, _)| f != fd)) {
        Ok(()) => 1,
        Err(_) => -1,
    }
}

// ---- ANativeWindow (5) — SOUND-STUB: real geometry getters; refcount ops no-op ------------------
//
// 2026-06-05: `ANativeWindow_fromSurface` mints an Eclipse window handle holding the default display
// geometry; the getters return that real geometry. The surface/buffer-presentation natives
// (`setBuffersGeometry`/`lock`/`unlockAndPost`) are NOT in libroblox's 27-symbol set — when the
// render integration lands they will route to the GLES2/EGL surface. `acquire`/`release` are correct
// no-ops (Eclipse windows live for the process lifetime in the registry). DEFERRED-TO-RENDER for the
// surface/buffer behavior; the geometry returned here is real.

/// `ANativeWindow* ANativeWindow_fromSurface(JNIEnv* env, jobject surface)` — get a native window for
/// a Java `Surface`. **sound-stub:** Eclipse mints an `ANativeWindow*` handle holding
/// [`default_native_window`] (the real default display geometry); the actual GLES2/EGL surface
/// binding is deferred to the render integration. Returns a valid Eclipse handle (so the getters
/// return real geometry), or NULL on registry exhaustion — never a fake non-window pointer.
///
/// # Safety
/// `env`/`surface` are the JNI args; this native does not dereference them (the surface binding is
/// deferred), so any value is accepted safely.
unsafe extern "C" fn eclipse_anativewindow_fromsurface(
    _env: *mut c_void,
    _surface: *mut c_void,
) -> *mut c_void {
    match ndk_registry::native_windows().insert(default_native_window()) {
        Ok(h) => handle_to_ptr(h),
        Err(_) => std::ptr::null_mut(),
    }
}

/// `int32_t ANativeWindow_getWidth(ANativeWindow* window)` — the window width in pixels. **sound:**
/// returns the real stored geometry; a stale/fabricated handle → `-1` (the NDK negative-error
/// contract), never a fake positive size.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_anativewindow_getwidth(window: *mut c_void) -> i32 {
    ndk_registry::native_windows()
        .with(ptr_to_handle(window), |w| w.width)
        .unwrap_or(-1)
}

/// `int32_t ANativeWindow_getHeight(ANativeWindow* window)` — the window height in pixels. **sound:**
/// real stored geometry; stale/fabricated handle → `-1`.
///
/// # Safety
/// `window` must be an `ANativeWindow*` from an Eclipse window native (or garbage, which is rejected).
unsafe extern "C" fn eclipse_anativewindow_getheight(window: *mut c_void) -> i32 {
    ndk_registry::native_windows()
        .with(ptr_to_handle(window), |w| w.height)
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
// audio (OpenSL ES) — the 8 audio natives. SOUND-STUB: audio deferred (gameplay-time). 2026-06-05.
//
// Sound is a gameplay-time subsystem libroblox does not need to start/render. Per the PUBLIC OpenSL
// ES 1.0.1 C-ABI (`SLES/OpenSLES.h`): `slCreateEngine` returns `SL_RESULT_FEATURE_UNSUPPORTED`
// (0x0000000C = 12) — the documented result a caller checks to detect "no audio" cleanly. The 7
// `SL_IID_*` are DATA objects of type `SLInterfaceID` (a pointer to a 128-bit interface-UUID struct);
// each resolves to a stable, valid, distinct Eclipse-owned `SLInterfaceID_` so the relocation has a
// real non-null address. Audio being unavailable, no engine is ever created to query them.
// =================================================================================================

/// `SLresult` is `SLuint32` → C `u32`. `SL_RESULT_FEATURE_UNSUPPORTED = 0x0000000C` from the public
/// OpenSL ES 1.0.1 header — "the requested feature is not supported", the clean "no audio" sentinel.
const SL_RESULT_FEATURE_UNSUPPORTED: u32 = 0x0000_000C;

/// `SLresult slCreateEngine(SLObjectItf* pEngine, SLuint32 numOptions,
/// const SLEngineOption* pEngineOptions, SLuint32 numInterfaces, const SLInterfaceID* pInterfaceIDs,
/// const SLboolean* pInterfaceRequired)`. **sound-stub:** audio is deferred, so the engine cannot be
/// created → `SL_RESULT_FEATURE_UNSUPPORTED`. Per the OpenSL ES contract a non-success result means no
/// object was produced, so `*pEngine` is left untouched and the caller must not use it. NOT a fake
/// engine the caller would `Realize`/`GetInterface` and then crash on.
///
/// # Safety
/// the pointer args are the OpenSL ES C-ABI params; none is dereferenced (the call fails before
/// producing an object), so any value (incl. null) is accepted safely.
unsafe extern "C" fn eclipse_sl_create_engine(
    _p_engine: *mut c_void,
    _num_options: u32,
    _p_engine_options: *const c_void,
    _num_interfaces: u32,
    _p_interface_ids: *const c_void,
    _p_interface_required: *const c_void,
) -> u32 {
    SL_RESULT_FEATURE_UNSUPPORTED
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::reloc::{apply_one, Rela, SliceImage, SymbolResolver, R_X86_64_GLOB_DAT};
    use std::cell::RefCell;

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
        // An unregistered name → None (falls through to the host tier).
        assert_eq!(p.resolve("memcpy"), None);
        assert_eq!(p.resolve("__eclipse_no_such_native__"), None);
    }

    #[test]
    fn with_bionic_natives_registers_the_three_implemented_categories() {
        let p = EclipseNativeProvider::with_bionic_natives();
        // 5 liblog (3 fixed-arity Rust + 2 variadic C-shim) + 15 bionic-libc + 27 ndk-android + 33
        // media-ndk + 8 audio + 37 bionic-pthread/TLS/sem/syscall + 5 bionic-sysconf system-query
        // (sysconf/getauxval/sched_getcpu/getpagesize/sysinfo — the allocator-bootstrap fix,
        // 2026-06-05) = 130.
        assert_eq!(
            p.len(),
            88 + super::super::bionic_pthread::PTHREAD_NATIVE_COUNT
                + super::super::bionic_sysconf::SYSQ_NATIVE_COUNT,
            "5 liblog + 15 bionic-libc + 27 ndk-android + 33 media-ndk + 8 audio + 37 pthread + 5 \
             sysconf system-query natives registered"
        );
        for name in [
            // liblog (3 fixed-arity Rust + 2 variadic C-shim)
            "__android_log_write",
            "__android_log_buf_write",
            "android_set_abort_message",
            "__android_log_print",
            "__android_log_assert",
            // bionic-libc (15)
            "__strlen_chk",
            "__strchr_chk",
            "__strncpy_chk2",
            "__write_chk",
            "__fwrite_chk",
            "__sendto_chk",
            "__FD_SET_chk",
            "__FD_CLR_chk",
            "__FD_ISSET_chk",
            "__errno",
            "__assert2",
            "__gnu_strerror_r",
            "__system_property_get",
            "__stack_chk_guard",
            "__sF",
            // ndk-android (27)
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
            // bionic pthread / TLS / sem / syscall (45) — the threading runtime (2026-06-05)
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
    fn sf_table_points_at_three_host_streams() {
        let addr = eclipse_sf_addr();
        assert!(addr != 0, "__sF address must be non-null");
        let t = ECLIPSE_SF.get().expect("table initialized");
        for fp in t.0 {
            assert!(!fp.is_null(), "each __sF entry is a host FILE*");
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
    fn aasset_openfiledescriptor_reports_no_direct_fd() {
        // Eclipse serves in-memory assets → no backing fd → the documented "< 0" (buffer fallback).
        let s = ndk_registry::assets()
            .insert(AssetState {
                bytes: Box::from(&b"x"[..]),
                cursor: 0,
            })
            .expect("insert asset");
        let asset = handle_to_ptr::<c_void>(s);
        // SAFETY: `asset` is live; out-params are null (the native returns the failure sentinel).
        let fd = unsafe {
            eclipse_aasset_openfiledescriptor(asset, std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert!(
            fd < 0,
            "no direct fd access for an in-memory asset (bionic < 0)"
        );
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

    // ---- ndk-android: ALooper minimal-correct sentinels ----------------------------------------

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

        // pollOnce: finite timeout → TIMEOUT; infinite → ERROR (never a fake CALLBACK / fd id).
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
            "finite-timeout pollOnce → TIMEOUT"
        );
        // SAFETY: out-params are null.
        let infinite = unsafe {
            eclipse_alooper_pollonce(
                -1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            infinite, ALOOPER_POLL_ERROR,
            "infinite pollOnce (no source) → ERROR"
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
        // Free the live window's slot to keep the registry tidy.
        ndk_registry::native_windows()
            .remove(ptr_to_handle(win))
            .ok();
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

    // ---- audio: sound-stub sentinels ------------------------------------------------------------

    #[test]
    fn sl_create_engine_reports_feature_unsupported() {
        // slCreateEngine → SL_RESULT_FEATURE_UNSUPPORTED (0x0C); the caller cleanly detects "no audio"
        // and must NOT use *pEngine (left untouched). NOT a fake engine.
        let mut engine: *mut c_void = 0xDEAD as *mut c_void; // poison; must stay untouched
                                                             // SAFETY: the OpenSL ES params are not dereferenced (the call fails before producing an object).
        let r = unsafe {
            eclipse_sl_create_engine(
                std::ptr::addr_of_mut!(engine).cast(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(r, SL_RESULT_FEATURE_UNSUPPORTED);
        assert_eq!(
            r, 0x0000_000C,
            "OpenSL ES public value of FEATURE_UNSUPPORTED"
        );
        assert_eq!(
            engine, 0xDEAD as *mut c_void,
            "a failed slCreateEngine must not write *pEngine"
        );
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
}
