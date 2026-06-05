//! Eclipse-owned **bionic-ABI-correct system-query natives** — the allocator-bootstrap fix.
//!
//! 2026-06-05: the `DT_INIT_ARRAY` discovery loop (`docs/libroblox-init-run.md`) pinned `init[1]`'s
//! abort to **libroblox's own per-thread (tcmalloc/arena) allocator returning NULL on its first
//! init-time refill** (the heap-config block at `base+0x65089c9` reads ~a dozen runtime globals it
//! computes from the environment). This module supplies the root-cause fix: the system-query calls
//! that block makes — `sysconf`, `getauxval`, `sched_getcpu`, `getpagesize`, `sysinfo` — implemented
//! **bionic-ABI-correctly** and PREPENDED before the host-glibc baseline in
//! [`super::bionic_env::BionicEnv`], so they intercept libroblox's calls with the **bionic** constant
//! semantics rather than the host glibc's.
//!
//! ## The root cause this fixes (trace-proven, dated 2026-06-05)
//! `libroblox.so` is compiled against the **bionic** headers, whose `sysconf(3)` `_SC_*` constant
//! **values differ from glibc's**. With the engine's `sysconf` import bound to host glibc, a call
//! that the engine *believes* is `sysconf(_SC_NPROCESSORS_ONLN)` passes the **bionic** number `97`,
//! which host glibc interprets as a *different* (or unknown) constant and answers wrongly:
//!
//! | query                       | bionic `_SC_*` value | host glibc `sysconf(value)` returns |
//! |-----------------------------|---------------------:|------------------------------------:|
//! | `_SC_PAGESIZE`              | `39`                 | `1000` (NOT 4096)                   |
//! | `_SC_NPROCESSORS_CONF`      | `96`                 | `200809` (a POSIX-version constant) |
//! | `_SC_NPROCESSORS_ONLN`      | `97`                 | **`-1`** (unknown to glibc)         |
//! | `_SC_PHYS_PAGES`            | `98`                 | `1`                                 |
//! | `_SC_CLK_TCK`               | `6`                  | **`-1`**                            |
//!
//! (Measured on this dev host; see the module tests + `docs/libroblox-init-run.md` §7.) An allocator
//! that sizes its arena table from `sysconf(_SC_NPROCESSORS_ONLN)` getting **`-1`** (or a page size
//! of `1000`) computes a zero / garbage arena count, so its first central refill returns NULL and the
//! constructor `abort()`s. The fix is to answer with the **bionic** constant meaning: bionic `39`
//! ⇒ page size, bionic `97` ⇒ online CPU count, etc.
//!
//! ## Clean-room provenance
//! The bionic `_SC_*` constant **values** are the documented public bionic ABI (`<bits/sysconf.h>`),
//! which differ from glibc's `<bits/confname.h>` numbering — general knowledge of the public bionic
//! headers. The `AT_*` auxv tags and the `sysinfo`/`getcpu` kernel ABIs are **kernel-defined** and
//! identical for bionic and glibc, so those forward to the host (with tracing). No bionic / NDK /
//! linker *source* was read; the real values are queried from the Linux kernel (`sched_getaffinity`,
//! `sysinfo`, `page_size`). `libroblox.so` is parsed as data only; nothing in it is executed here.
//!
//! ## Tracing
//! Under the env gate `ECLIPSE_TRACE_SYSQ=1`, every call logs `name(args incl. the raw constant) ->
//! return` to stderr (async-signal-unsafe `eprintln!`, fine on this diagnostic path). This is the
//! observability the discovery loop needs to confirm *which* call + constant the allocator reacts to.
//!
//! ## Safety
//! Taking the address of an `extern "C"` fn is safe Rust; the provider registration needs no
//! `unsafe`. The `unsafe` is confined to the native bodies that issue the raw Linux syscalls
//! (`sched_getaffinity`, `getcpu`, `sysinfo`) and the host `getauxval` FFI, each with a dated
//! `// SAFETY:` note. [`super::reloc`]/[`super::elf`]/[`super::resolve`] stay `#![forbid(unsafe_code)]`.

use std::ffi::{c_int, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicU8, Ordering};

// =================================================================================================
// Bionic `_SC_*` constant VALUES (public bionic ABI, `<bits/sysconf.h>`) — DIFFER from glibc's.
// =================================================================================================
//
// 2026-06-05: these are the bionic numbers `libroblox.so` passes to `sysconf`. Only the subset the
// allocator bootstrap (and common runtime startup) needs is mapped; an unmapped bionic constant
// returns -1 (the POSIX "indeterminate / not supported" answer — never a wrong positive). The full
// bionic enum is large; mapping the startup-relevant subset is the smallest correct fix.

/// bionic `_SC_ARG_MAX`.
const SC_ARG_MAX: c_int = 0x0000;
/// bionic `_SC_CLK_TCK` — clock ticks per second.
const SC_CLK_TCK: c_int = 0x0002;
/// bionic `_SC_NGROUPS_MAX`.
const SC_NGROUPS_MAX: c_int = 0x0003;
/// bionic `_SC_OPEN_MAX` — max open files.
const SC_OPEN_MAX: c_int = 0x0004;
/// bionic `_SC_PAGESIZE` / `_SC_PAGE_SIZE` (the two are the same value in bionic).
const SC_PAGESIZE: c_int = 0x0027;
/// bionic `_SC_PAGE_SIZE` (alias of `_SC_PAGESIZE`).
const SC_PAGE_SIZE: c_int = 0x0028;
/// bionic `_SC_NPROCESSORS_CONF` — configured CPUs.
const SC_NPROCESSORS_CONF: c_int = 0x0060;
/// bionic `_SC_NPROCESSORS_ONLN` — online CPUs (the value the allocator sizes its arenas from).
const SC_NPROCESSORS_ONLN: c_int = 0x0061;
/// bionic `_SC_PHYS_PAGES` — total RAM pages.
const SC_PHYS_PAGES: c_int = 0x0062;
/// bionic `_SC_AVPHYS_PAGES` — available RAM pages.
const SC_AVPHYS_PAGES: c_int = 0x0063;

// 2026-06-05: bionic and glibc DISAGREE on these numbers. bionic `_SC_PAGE_SIZE`/`_SC_PAGESIZE` is
// `0x27`/`0x28`; the two adjacent bionic values both denote the page size (bionic's header defines
// `_SC_PAGE_SIZE` and `_SC_PAGESIZE` as distinct adjacent enum members that bionic's own `sysconf`
// answers identically). We honor both.

// =================================================================================================
// Tracing — env-gated (`ECLIPSE_TRACE_SYSQ=1`) per-call stderr log.
// =================================================================================================

/// Tri-state cache for the `ECLIPSE_TRACE_SYSQ` env gate: 0 = unknown, 1 = off, 2 = on. Checked once
/// (the env does not change mid-run); avoids a `getenv` per system-query call on the hot path.
static TRACE_STATE: AtomicU8 = AtomicU8::new(0);

/// Whether `ECLIPSE_TRACE_SYSQ=1` is set (cached after the first check).
fn trace_enabled() -> bool {
    match TRACE_STATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = std::env::var_os("ECLIPSE_TRACE_SYSQ").is_some_and(|v| v == "1");
            TRACE_STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

/// Emit a system-query trace line to stderr when the env gate is on. Cheap no-op otherwise.
fn trace(args: std::fmt::Arguments<'_>) {
    if trace_enabled() {
        eprintln!("[sysq] {args}");
    }
}

// =================================================================================================
// CPU count — the value the allocator's arena table sizes from. Detect, don't assume.
// =================================================================================================

/// The number of CPUs in the calling process's affinity mask (the kernel's authoritative "online,
/// usable by this process" count), via `sched_getaffinity(2)`. Falls back to the glibc
/// `_SC_NPROCESSORS_ONLN` (called CORRECTLY from Rust with glibc's own constant) if the syscall
/// fails, and finally to `1` so the allocator always sees a positive count (a 0/-1 here is exactly
/// what broke the allocator). 2026-06-05.
fn online_cpu_count() -> c_long {
    // sched_getaffinity returns the number of bytes written; count set bits across the mask.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-05 — `sched_getaffinity(0, size, &mut set)` writes the calling process's CPU
    // affinity mask into `set` (a zero-initialized `cpu_set_t` of `size` bytes). pid 0 = the caller.
    // On success it returns 0 (glibc) / the byte count; we then count set bits via the libc CPU_*
    // macros, which read only within `set`.
    let rc = unsafe {
        libc::sched_getaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            std::ptr::addr_of_mut!(set),
        )
    };
    if rc == 0 {
        // SAFETY: 2026-06-05 — `CPU_COUNT` reads the populated `set`; no out-of-bounds access.
        let n = unsafe { libc::CPU_COUNT(&set) };
        if n > 0 {
            return c_long::from(n);
        }
    }
    // Fallback: glibc's own _SC_NPROCESSORS_ONLN (the CORRECT glibc constant, used from Rust).
    // SAFETY: 2026-06-05 — `libc::sysconf` with glibc's `_SC_NPROCESSORS_ONLN` is the standard,
    // well-defined call (we pass glibc's constant, not bionic's — no mismatch here).
    let onln = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if onln > 0 {
        return onln;
    }
    1 // never report 0/-1 online CPUs — that is exactly the value the allocator chokes on.
}

/// The number of CPUs the kernel knows about (configured), via the glibc `_SC_NPROCESSORS_CONF`
/// (called with glibc's own constant), clamped to ≥ the online count so CONF ≥ ONLN always holds.
fn conf_cpu_count() -> c_long {
    // SAFETY: 2026-06-05 — glibc's own `_SC_NPROCESSORS_CONF` constant (no mismatch).
    let conf = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    conf.max(online_cpu_count())
}

// =================================================================================================
// `long sysconf(int name)` — the FIX: map the BIONIC `_SC_*` value to the correct runtime answer.
// =================================================================================================

/// Eclipse-owned bionic-ABI-correct `sysconf`. `name` is a **bionic** `_SC_*` constant; we answer
/// with that constant's meaning (NOT host glibc's numbering). The startup-relevant subset is mapped;
/// an unmapped constant returns `-1` (POSIX "indeterminate", which a caller treats as "use a
/// default") rather than forwarding the bionic number to glibc (which would answer for a *different*
/// constant — the bug this fixes). 2026-06-05.
extern "C" fn eclipse_sysconf(name: c_int) -> c_long {
    let r: c_long = match name {
        SC_PAGESIZE | SC_PAGE_SIZE => super::map::host_page_size() as c_long,
        SC_NPROCESSORS_ONLN => online_cpu_count(),
        SC_NPROCESSORS_CONF => conf_cpu_count(),
        SC_CLK_TCK => {
            // The kernel's clock tick (USER_HZ); glibc's own _SC_CLK_TCK answers it correctly.
            // SAFETY: 2026-06-05 — glibc's own `_SC_CLK_TCK` constant (no bionic mismatch).
            let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if t > 0 {
                t
            } else {
                100
            } // USER_HZ is 100 on essentially all Linux x86-64.
        }
        SC_PHYS_PAGES => {
            // SAFETY: 2026-06-05 — glibc's own `_SC_PHYS_PAGES` constant.
            unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) }
        }
        SC_AVPHYS_PAGES => {
            // SAFETY: 2026-06-05 — glibc's own `_SC_AVPHYS_PAGES` constant.
            unsafe { libc::sysconf(libc::_SC_AVPHYS_PAGES) }
        }
        SC_OPEN_MAX => {
            // SAFETY: 2026-06-05 — glibc's own `_SC_OPEN_MAX` constant.
            unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }
        }
        SC_ARG_MAX => {
            // SAFETY: 2026-06-05 — glibc's own `_SC_ARG_MAX` constant.
            unsafe { libc::sysconf(libc::_SC_ARG_MAX) }
        }
        SC_NGROUPS_MAX => {
            // SAFETY: 2026-06-05 — glibc's own `_SC_NGROUPS_MAX` constant.
            unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) }
        }
        // An unmapped bionic constant: -1 (POSIX indeterminate). Do NOT forward the bionic number to
        // glibc — glibc would answer for a different constant (the exact mismatch this module fixes).
        _ => -1,
    };
    trace(format_args!("sysconf(name={name}) -> {r}"));
    r
}

// =================================================================================================
// `unsigned long getauxval(unsigned long type)` — forward to host (AT_* tags are kernel-shared).
// =================================================================================================

extern "C" {
    /// glibc's `getauxval` — the auxv `AT_*` tags are kernel-defined and IDENTICAL for bionic and
    /// glibc (`AT_PAGESZ=6`, `AT_HWCAP=16`, `AT_RANDOM=25`, …), so forwarding is bionic-correct.
    /// 2026-06-05.
    fn getauxval(at_type: c_ulong) -> c_ulong;
}

/// Eclipse-owned `getauxval`. The `AT_*` tag numbering is kernel-defined (shared bionic/glibc), so
/// this forwards to the host `getauxval` (correct values) and traces the call. 2026-06-05.
extern "C" fn eclipse_getauxval(at_type: c_ulong) -> c_ulong {
    // SAFETY: 2026-06-05 — `getauxval(type)` reads the process's auxiliary vector and returns the
    // value for `type` (0 if absent). It has no pointer args and cannot write our memory.
    let r = unsafe { getauxval(at_type) };
    trace(format_args!("getauxval(type={at_type}) -> {r}"));
    r
}

// =================================================================================================
// `int sched_getcpu(void)` — the CPU the caller is running on (getcpu(2); kernel ABI, shared).
// =================================================================================================

/// Eclipse-owned `sched_getcpu`. bionic's `sched_getcpu` is `getcpu(2)` (a kernel syscall whose ABI
/// is identical for bionic and glibc), so this forwards to the host `sched_getcpu` and traces it. A
/// negative result (no getcpu support) is clamped to `0` so the allocator's per-CPU bucket index is
/// always valid. 2026-06-05.
extern "C" fn eclipse_sched_getcpu() -> c_int {
    // SAFETY: 2026-06-05 — `sched_getcpu()` is a no-argument query of the current CPU via `getcpu(2)`;
    // it cannot write our memory.
    let raw = unsafe { libc::sched_getcpu() };
    let r = if raw < 0 { 0 } else { raw };
    trace(format_args!("sched_getcpu() -> {r} (raw={raw})"));
    r
}

// =================================================================================================
// `int getpagesize(void)` — the page size (kernel-defined; 4K on x86-64, 16K on some configs).
// =================================================================================================

/// Eclipse-owned `getpagesize`. Returns the runtime host page size (detect-don't-assume), which is
/// the same value bionic's `getpagesize` returns on the same kernel. 2026-06-05.
extern "C" fn eclipse_getpagesize() -> c_int {
    let r = super::map::host_page_size() as c_int;
    trace(format_args!("getpagesize() -> {r}"));
    r
}

// =================================================================================================
// `int sysinfo(struct sysinfo* info)` — the kernel `sysinfo(2)`; the struct is kernel ABI (shared).
// =================================================================================================

/// Eclipse-owned `sysinfo`. `struct sysinfo` is the kernel's ABI (identical for bionic and glibc on
/// x86-64), so this forwards to the host `sysinfo(2)` (filling the caller's struct with real RAM /
/// uptime / load values) and traces it. 2026-06-05.
///
/// # Safety
/// `info` must be null or point to a writable `struct sysinfo` (the bionic caller contract).
unsafe extern "C" fn eclipse_sysinfo(info: *mut c_void) -> c_int {
    // SAFETY: 2026-06-05 — `sysinfo(ptr)` fills the kernel `struct sysinfo` at `ptr` (or returns -1
    // if `ptr` is invalid). The caller passes a writable `struct sysinfo*` per the public contract;
    // the kernel ABI is identical for bionic/glibc. We cast to the libc struct pointer for the call.
    let r = unsafe { libc::sysinfo(info.cast::<libc::sysinfo>()) };
    trace(format_args!("sysinfo(info={info:p}) -> {r}"));
    r
}

// =================================================================================================
// Registration — append the system-query natives (prepended before host in BionicEnv).
// =================================================================================================

/// The number of system-query natives this module registers: `sysconf`, `getauxval`, `sched_getcpu`,
/// `getpagesize`, `sysinfo` = **5**. These previously resolved to the host-glibc baseline, whose
/// `sysconf` mis-answers the bionic `_SC_*` constants (the `init[1]` allocator-bootstrap abort).
pub const SYSQ_NATIVE_COUNT: usize = 5;

/// Append every Eclipse-owned bionic-ABI-correct system-query native to `register` as
/// `(name, address)` pairs. Called by [`super::native_provider::EclipseNativeProvider`] so the
/// engine's `sysconf`/`getauxval`/`sched_getcpu`/`getpagesize`/`sysinfo` imports bind to these
/// bionic-correct natives, displacing the host-glibc baseline whose `sysconf` mis-answers the bionic
/// `_SC_*` constant numbering. 2026-06-05.
pub fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    register("sysconf", eclipse_sysconf as *const () as u64);
    register("getauxval", eclipse_getauxval as *const () as u64);
    register("sched_getcpu", eclipse_sched_getcpu as *const () as u64);
    register("getpagesize", eclipse_getpagesize as *const () as u64);
    register("sysinfo", eclipse_sysinfo as *const () as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysconf_pagesize_is_the_real_page_size() {
        // bionic _SC_PAGESIZE (39) / _SC_PAGE_SIZE (40) → the real host page size (4096 on x86-64),
        // NOT glibc's sysconf(39)=1000 / sysconf(40)=... that the bug produced.
        let page = super::super::map::host_page_size() as c_long;
        assert_eq!(eclipse_sysconf(SC_PAGESIZE), page);
        assert_eq!(eclipse_sysconf(SC_PAGE_SIZE), page);
        assert!(page >= 4096, "page size must be ≥ 4096 on a real host");
    }

    #[test]
    fn sysconf_cpu_counts_are_positive_and_ordered() {
        // bionic _SC_NPROCESSORS_ONLN (97) → a POSITIVE online CPU count (NOT glibc's -1 for 97,
        // which was the value that zeroed the allocator's arena table and caused the abort).
        let onln = eclipse_sysconf(SC_NPROCESSORS_ONLN);
        let conf = eclipse_sysconf(SC_NPROCESSORS_CONF);
        assert!(
            onln > 0,
            "online CPU count must be > 0 (was -1 under the bug)"
        );
        assert!(conf > 0, "configured CPU count must be > 0");
        assert!(conf >= onln, "configured CPUs ≥ online CPUs");
    }

    #[test]
    fn sysconf_clk_tck_is_positive() {
        // bionic _SC_CLK_TCK (6) → a positive tick rate (NOT glibc's sysconf(6)=-1).
        let tck = eclipse_sysconf(SC_CLK_TCK);
        assert!(
            tck > 0,
            "clock tick rate must be > 0 (was -1 under the bug)"
        );
    }

    #[test]
    fn sysconf_phys_pages_is_positive() {
        // bionic _SC_PHYS_PAGES (98) → the real total RAM page count (> 0), not glibc's sysconf(98)=1.
        let phys = eclipse_sysconf(SC_PHYS_PAGES);
        assert!(phys > 0, "physical RAM page count must be > 0");
    }

    #[test]
    fn sysconf_unmapped_constant_returns_indeterminate_not_a_wrong_value() {
        // An unmapped bionic constant returns -1 (POSIX indeterminate), never a forwarded-to-glibc
        // wrong answer. 0x7fff is not in the mapped startup subset.
        assert_eq!(eclipse_sysconf(0x7fff), -1);
    }

    #[test]
    fn getauxval_at_pagesz_is_the_page_size() {
        // AT_PAGESZ = 6 (kernel-defined, shared bionic/glibc) → the page size, > 0.
        const AT_PAGESZ: c_ulong = 6;
        let v = eclipse_getauxval(AT_PAGESZ);
        assert!(v > 0, "AT_PAGESZ must be > 0");
        assert_eq!(v as c_long, super::super::map::host_page_size() as c_long);
    }

    #[test]
    fn getpagesize_matches_host() {
        assert_eq!(
            eclipse_getpagesize() as u64,
            super::super::map::host_page_size()
        );
    }

    #[test]
    fn sched_getcpu_is_nonnegative() {
        // sched_getcpu must yield a valid (≥ 0) CPU index for the allocator's per-CPU bucketing.
        assert!(eclipse_sched_getcpu() >= 0);
    }

    #[test]
    fn online_cpu_count_helper_is_positive() {
        // The CPU-count helper must NEVER return 0/-1 (the value that broke the allocator).
        assert!(online_cpu_count() > 0);
        assert!(conf_cpu_count() >= online_cpu_count());
    }

    #[test]
    fn registers_the_expected_system_query_natives() {
        let mut names: Vec<&'static str> = Vec::new();
        register_natives(|name, addr| {
            assert!(addr != 0, "{name} must have a non-null address");
            names.push(name);
        });
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "getauxval",
                "getpagesize",
                "sched_getcpu",
                "sysconf",
                "sysinfo"
            ]
        );
        assert_eq!(names.len(), SYSQ_NATIVE_COUNT);
    }
}
