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
//! ## What this is NOT (honest scope, dated 2026-06-05)
//! Registering a correct address makes the relocation land *and* (for the forward/minimal natives)
//! makes a **call** to that symbol behave per its public contract. It does **not** by itself make
//! `libroblox.so` runnable — that needs the rest of the work-list (ndk-android / media-ndk / audio),
//! binding the image to execution, and running the `DT_INIT_ARRAY` constructors (the runtime tail,
//! main-loop / dev-host only).

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

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

    /// Build the provider with the **fixed-arity liblog (3)** + **bionic-specific libc (15)** natives
    /// this module implements registered. Taking each native's address is safe Rust (a function/data
    /// item coerced to a pointer then to `u64`).
    ///
    /// The names are the real work-list from `loader::link::tests::real_libroblox_bionic_env_*`
    /// (`docs/bionic-env-worklist.md`): of liblog's 5, the 2 **variadic** ones
    /// (`__android_log_print`/`__android_log_assert`) stay on the work-list (see the module docs); of
    /// bionic-libc's 15, all are implemented. **18** symbols total — registering them shrinks the
    /// engine's work-list from 88 to **70** (the 2 deferred variadic liblog + ndk-android 27 +
    /// media-ndk 33 + audio 8).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::reloc::{apply_one, Rela, SliceImage, SymbolResolver, R_X86_64_GLOB_DAT};

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
        // The two VARIADIC liblog natives are deliberately NOT registered (work-list; no landmine).
        assert_eq!(p.resolve("__android_log_print"), None);
        assert_eq!(p.resolve("__android_log_assert"), None);
        // An unregistered name → None (falls through to the host tier).
        assert_eq!(p.resolve("memcpy"), None);
        assert_eq!(p.resolve("__eclipse_no_such_native__"), None);
    }

    #[test]
    fn with_bionic_natives_registers_the_two_implemented_categories() {
        let p = EclipseNativeProvider::with_bionic_natives();
        // 3 fixed-arity liblog + 15 bionic-libc = 18 registered natives.
        assert_eq!(p.len(), 18, "3 liblog + 15 bionic-libc natives registered");
        for name in [
            "__android_log_write",
            "__android_log_buf_write",
            "android_set_abort_message",
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
        ] {
            assert!(p.resolve(name).is_some(), "{name} must be registered");
        }
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
}
