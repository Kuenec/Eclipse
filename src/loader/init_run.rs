//! Isolated `DT_INIT_ARRAY` execution harness for `libroblox.so` (dev-host / main-loop only).
//!
//! 2026-06-05: This is the **engine-load runtime tail's first execution step** — the first time
//! `libroblox.so`'s own code runs under Eclipse's loader. Everything upstream (map the 3 PT_LOAD,
//! apply all 527,843 relocations, honor RELRO + BIND_NOW, fully resolve all 584 imports) is DONE
//! and proven (see `link.rs` / `bionic_env.rs` / `native_provider.rs` + AGENTS.md §5). This module
//! reads the engine's `DT_INIT_ARRAY` (3,427 constructors) and **calls each one in order**, which is
//! an inherently `unsafe` jump into mapped foreign code (confined here; the decode/map/reloc cores
//! stay `#![forbid(unsafe_code)]`).
//!
//! ## What this is (and is NOT)
//! A **diagnostic discovery step**, run from a hidden `eclipse __run-libroblox-init` subcommand on
//! the process MAIN thread (a crash must abort this process, not poison a `#[test]` suite). The
//! libc imports resolve to a HOST-glibc baseline (NOT bionic-ABI-correct — errno/pthread/FILE/struct
//! layouts differ), so an early crash in a constructor is the **expected, valuable** result that
//! pinpoints the next obstacle (almost certainly "need a bionic-ABI-correct libc shim, not host
//! glibc"). The harness reports exactly how far it got and where it died — it never fakes success.
//!
//! ## gABI init-array calling convention (2026-06-05)
//! Per the System V gABI, `DT_INIT_ARRAY` holds an array of `void (*)(void)` function pointers, run
//! in array order after relocation. Bionic's linker additionally passes `(int argc, char** argv,
//! char** envp)` to every init function; on x86-64 SysV those go in `rdi`/`rsi`/`rdx`, which a
//! `void(void)` callee simply ignores. So we declare each entry as the 3-arg form and pass a plausible
//! `argc=1 / argv=["libroblox", NULL] / envp=[NULL]` — correct for both the plain and the bionic
//! convention. The array entries themselves are absolute runtime addresses: the base-relocation pass
//! (`R_X86_64_RELATIVE`) already rewrote each slot from `entry_vaddr` to `load_base + entry_vaddr`.

use std::ffi::{c_char, c_int};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::bionic_env::BionicEnv;
use super::link::Linker;
use super::map::host_page_size;
use super::resolve::{LoadedObjectProvider, Scope};

/// Size of one `DT_INIT_ARRAY` entry: a 64-bit function pointer.
const INIT_ENTRY_SIZE: u64 = 8;

/// Number of constructors in a `DT_INIT_ARRAY` of `init_arraysz` bytes (each entry is an 8-byte
/// function pointer). Pure arithmetic — unit-tested, GPU/VM-free.
#[must_use]
pub fn init_array_count(init_arraysz: u64) -> usize {
    (init_arraysz / INIT_ENTRY_SIZE) as usize
}

/// Region-relative byte offset of `DT_INIT_ARRAY` entry `index`, given the array's vaddr (which, for
/// a PIE/`ET_DYN`, is also the region-relative offset of the array within the mapping). The entry
/// read at this offset holds the absolute runtime constructor address (post-`R_X86_64_RELATIVE`).
/// Pure arithmetic — unit-tested, GPU/VM-free.
#[must_use]
pub fn init_array_entry_offset(init_array_vaddr: u64, index: usize) -> u64 {
    init_array_vaddr + (index as u64) * INIT_ENTRY_SIZE
}

/// The faulting constructor's index, published before each call so a signal handler can report which
/// constructor crashed. `usize::MAX` = no constructor running yet.
static CURRENT_INIT_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The faulting constructor's absolute address, published before each call (0 = none yet).
static CURRENT_INIT_ADDR: AtomicU64 = AtomicU64::new(0);

/// Errors the harness can report before it reaches the (unsafe) constructor-call phase.
#[derive(Debug)]
pub enum InitRunError {
    /// No Roblox APK found (env `ECLIPSE_ROBLOX_APK` or the default dev-host path).
    NoApk,
    /// Opening or reading `lib/x86_64/libroblox.so` from the APK failed.
    Apk(String),
    /// Staging the extracted `.so` to a temp path failed.
    Stage(String),
    /// The map / relocate / resolve pipeline failed (a real loader error, not a constructor crash).
    Link(String),
    /// The mapped image lacks a `DT_INIT_ARRAY` (unexpected — the engine has 3,427).
    NoInitArray,
    /// A non-fatal setup error (e.g. installing the signal handler).
    Setup(String),
}

impl std::fmt::Display for InitRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoApk => write!(
                f,
                "no Roblox APK (set ECLIPSE_ROBLOX_APK or place it at the default dev-host path)"
            ),
            Self::Apk(e) => write!(f, "APK read: {e}"),
            Self::Stage(e) => write!(f, "stage libroblox.so: {e}"),
            Self::Link(e) => write!(f, "map/relocate/resolve: {e}"),
            Self::NoInitArray => write!(f, "mapped libroblox.so has no DT_INIT_ARRAY"),
            Self::Setup(e) => write!(f, "harness setup: {e}"),
        }
    }
}

impl std::error::Error for InitRunError {}

/// The Roblox APK candidates: the explicit env override, then the default dev-host location.
/// Mirrors `link.rs`'s test helper (kept in sync, dev-host only).
fn find_roblox_apk() -> Option<std::path::PathBuf> {
    std::env::var_os("ECLIPSE_ROBLOX_APK")
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(std::env::var_os("HOME").map(|home| {
            Path::new(&home).join("eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk")
        }))
        .find(|p| p.exists())
}

/// Load + map + relocate + fully-resolve `libroblox.so`, then call its `DT_INIT_ARRAY` constructors
/// in order. **Diagnostic harness — dev-host / main-loop only.** Returns the number of constructors
/// that completed before the function returns; if a constructor crashes (SIGSEGV/SIGABRT/…), the
/// installed signal handler logs the faulting index + address and `_exit`s the process non-zero
/// (this fn never returns in that case). All progress is logged to stderr/stdout (flushed) so the
/// caller's `> /tmp/eclipse-libroblox-init.log 2>&1` redirect captures it.
///
/// This runs on the **process main thread** by contract (the caller invokes it from `main`), so the
/// foreign code sees the real main-thread stack and a crash aborts this process cleanly.
pub fn run_libroblox_init() -> Result<usize, InitRunError> {
    let mut log = std::io::stderr();
    let _ = writeln!(
        log,
        "eclipse __run-libroblox-init: isolated DT_INIT_ARRAY execution harness (dev-host)"
    );

    // ---- 1) Read libroblox.so from the APK + stage it ---------------------------------------------
    let apk_path = find_roblox_apk().ok_or(InitRunError::NoApk)?;
    let _ = writeln!(log, "APK: {}", apk_path.display());
    // Configure the asset source for the ndk-android natives (AAssetManager_*), in case a constructor
    // touches it — serves assets/* from this APK via Eclipse's own src/apk reader (idempotent).
    super::ndk_registry::set_apk_path(apk_path.clone());

    let mut apk = crate::apk::Apk::open(&apk_path).map_err(|e| InitRunError::Apk(e.to_string()))?;
    let so_bytes = apk
        .read_entry("lib/x86_64/libroblox.so")
        .map_err(|e| InitRunError::Apk(e.to_string()))?;
    let _ = writeln!(log, "libroblox.so: {} bytes read from APK", so_bytes.len());

    // Stage to a temp file so the linker (which loads from a path) can map it. Cleaned up at the end
    // on the non-crash path; on a crash the temp file leaks (acceptable for a dev-host diagnostic).
    let dir = std::env::temp_dir().join(format!("eclipse-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| InitRunError::Stage(e.to_string()))?;
    let so_path = dir.join("libroblox.so");
    std::fs::write(&so_path, &so_bytes).map_err(|e| InitRunError::Stage(e.to_string()))?;

    // ---- 2) Map + base-relocate root-only (deps env-provided, host fallback off) -------------------
    let linker = Linker::new(Vec::<std::path::PathBuf>::new())
        .with_host_fallback(false)
        .with_tolerate_missing_deps(true);
    let mut set = linker
        .load(&so_path)
        .map_err(|e| InitRunError::Link(e.to_string()))?;
    let page = host_page_size();
    let _ = writeln!(
        log,
        "mapped: objects={} RELATIVE_applied={} RELR_applied={} RELRO_applied={}",
        set.objects.len(),
        set.stats.relative_applied,
        set.stats.relr_applied,
        set.relro_applied
    );

    // ---- 3) Build the FULL Eclipse scope + apply the symbol relocations (FULL resolution) ----------
    // [LoadedObjectProvider(libroblox)] + Eclipse-native tier prepended before the host baseline
    // (eclipse_natives=true) — the same scope that resolves all 584 imports (work-list 0).
    let base = set.objects[0].load_base();
    let dynsyms = {
        let img = set.objects[0]
            .image()
            .map_err(|e| InitRunError::Link(e.to_string()))?;
        img.dynsyms.clone()
    };
    let mut scope = Scope::new();
    scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
    for p in BionicEnv::with_host_baseline(true, true).into_providers() {
        scope.push(p);
    }
    let sym_stats = set
        .relocate_object_symbols_partial("libroblox.so", &scope, page)
        .map_err(|e| InitRunError::Link(e.to_string()))?;
    let _ = writeln!(
        log,
        "symbol relocs: applied_nonnull={} applied_weak_zero={} unresolved_strong={} (work-list)",
        sym_stats.applied_nonnull, sym_stats.applied_weak_zero, sym_stats.unresolved_strong
    );
    if sym_stats.unresolved_strong != 0 {
        let _ = writeln!(
            log,
            "WARNING: {} unresolved-strong imports remain (work-list non-empty: {:?}); \
             constructors may jump through null GOT slots",
            sym_stats.unresolved_strong, sym_stats.unresolved
        );
    }

    // ---- 4) Confirm the text segment is PROT_EXEC + locate DT_INIT_ARRAY ---------------------------
    let obj = &set.objects[0];
    let (init_array_vaddr, init_arraysz, exec_ok, exec_detail) = {
        let img = obj.image().map_err(|e| InitRunError::Link(e.to_string()))?;
        // The mapper sets each PT_LOAD's final protection to its p_flags; verify the executable
        // segment carries PF_X. Cross-check the live mapping via /proc/self/maps (detect, don't assume).
        let text = img
            .loads
            .iter()
            .find(|s| s.flags & super::elf::PF_X != 0)
            .copied();
        let (exec_ok, exec_detail) = match text {
            Some(seg) => {
                let runtime_start = base + seg.vaddr;
                let proc_exec = proc_maps_is_exec(runtime_start);
                (
                    proc_exec.unwrap_or(true),
                    format!(
                        "text seg vaddr={:#x} runtime=[{:#x},{:#x}) flags={:#x} /proc-exec={}",
                        seg.vaddr,
                        runtime_start,
                        runtime_start + seg.mem_size,
                        seg.flags,
                        proc_exec
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    ),
                )
            }
            None => (false, "no PF_X segment in PT_LOAD table".to_string()),
        };
        let (va, sz) = img.dyn_info.init_array.ok_or(InitRunError::NoInitArray)?;
        (va, sz, exec_ok, exec_detail)
    };
    let _ = writeln!(log, "text PROT_EXEC: {exec_ok} ({exec_detail})");
    if !exec_ok {
        return Err(InitRunError::Setup(format!(
            "text segment is not executable: {exec_detail}"
        )));
    }

    let count = init_array_count(init_arraysz);
    let _ = writeln!(
        log,
        "DT_INIT_ARRAY: vaddr={init_array_vaddr:#x} size={init_arraysz} bytes -> {count} constructors"
    );
    let _ = log.flush();

    // ---- 5) Install a minimal signal handler (logs the faulting constructor, then _exit) ----------
    install_crash_handler().map_err(InitRunError::Setup)?;

    // ---- 6) Call each constructor IN ORDER --------------------------------------------------------
    // Snapshot the absolute addresses first (a constructor could, in principle, repaginate the array;
    // RELRO already made it read-only, so the snapshot is stable and matches the live slots).
    let mut entries: Vec<u64> = Vec::with_capacity(count);
    for i in 0..count {
        let off = init_array_entry_offset(init_array_vaddr, i) as usize;
        let addr = obj.mapped.read_u64(off).map_err(|e| {
            InitRunError::Setup(format!("read init_array[{i}] @ off {off:#x}: {e}"))
        })?;
        entries.push(addr);
    }
    let _ = writeln!(
        log,
        "calling {count} constructors (argc=1, argv=[\"libroblox\",NULL], envp=[NULL]) …"
    );
    let _ = log.flush();

    // Plausible (argc, argv, envp) for the bionic init-array convention (ignored by void(void) ctors).
    let arg0 = b"libroblox\0";
    let mut argv: [*mut c_char; 2] = [arg0.as_ptr() as *mut c_char, std::ptr::null_mut()];
    let mut envp: [*mut c_char; 1] = [std::ptr::null_mut()];
    let argc: c_int = 1;

    let mut completed = 0usize;
    for (i, &addr) in entries.iter().enumerate() {
        if addr == 0 {
            // A null constructor slot would be a base-relocation bug; report and stop (don't jump to 0).
            let _ = writeln!(
                log,
                "init[{i}/{count}] @ NULL — aborting (null constructor slot)"
            );
            let _ = log.flush();
            return Err(InitRunError::Setup(format!(
                "null constructor slot at index {i}"
            )));
        }
        let offset = addr.wrapping_sub(base);
        let _ = writeln!(log, "init[{i}/{count}] @ base+{offset:#x} (abs {addr:#x})");
        let _ = log.flush();
        // Publish the current constructor so the crash handler can name it.
        CURRENT_INIT_INDEX.store(i, Ordering::SeqCst);
        CURRENT_INIT_ADDR.store(addr, Ordering::SeqCst);

        // SAFETY: 2026-06-05 — `addr` is a constructor function pointer read from the engine's
        // DT_INIT_ARRAY after the base-relocation pass rewrote each slot to its absolute runtime
        // address (`load_base + entry_vaddr`); the slot lies inside the mapped, relocated, RELRO-
        // hardened image, and the text it points into is in the PF_X segment confirmed PROT_EXEC
        // above. The gABI/bionic init-array convention is `void(int,char**,char**)` (a `void(void)`
        // ctor ignores the three SysV-register args), so calling through that signature is ABI-safe
        // either way. This jumps into FOREIGN code: if it faults, the installed handler logs the
        // index+address and `_exit`s — a valid diagnostic, not UB we can recover from. Runs on the
        // process main thread (caller contract), so the foreign code has a real, deep stack.
        let ctor: extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) =
            unsafe { std::mem::transmute::<u64, _>(addr) };
        ctor(argc, argv.as_mut_ptr(), envp.as_mut_ptr());

        completed += 1;
    }
    CURRENT_INIT_INDEX.store(usize::MAX, Ordering::SeqCst);

    let _ = writeln!(
        log,
        "ALL {completed}/{count} constructors completed without a crash"
    );
    let _ = log.flush();
    let _ = std::io::stdout().flush();

    // 2026-06-05 — The diagnostic's defined job (run every DT_INIT_ARRAY constructor and report how
    // far it got) is now COMPLETE: all `count` constructors ran. We must NOT return through `main`
    // here, because process teardown would run two things this bare init harness cannot support:
    //   1. `munmap`ping libroblox (RAII drop of `set`, see `link::LoadedImageSet`) — but the engine's
    //      constructors spawned BACKGROUND WORKER THREADS (its job system; `ECLIPSE_TRACE_THREADS=1`
    //      shows one `pthread_create` → a thread named "RBX Worker A") that keep executing libroblox
    //      text. Unmapping it out from under a live worker faults it (gdb-proven).
    //   2. glibc `exit()` running libroblox's C++ static destructors / `atexit` finalizers — a SHUTDOWN
    //      lifecycle phase (distinct from init) this bare init harness does not support. (Historical
    //      note: until 2026-06-12 a finalizer's `fflush(&__sF[i])` ALSO faulted here, because `__sF`
    //      was provided as a 24-byte host-stdio POINTER table while bionic's public ABI makes
    //      `&__sF[i]` an array-of-structs interior address (gdb-proven 2026-06-05 at exit time —
    //      the same mechanism later killed crashpad's logging, core 782252). `__sF` is now a
    //      bionic-shaped 3x152-byte backing with translating stdio natives (native_provider.rs),
    //      so that specific fault is fixed — but reason 1 alone still makes returning unsafe.)
    // So once init has fully succeeded, exit the process IMMEDIATELY and cleanly with `_exit(0)`: it
    // bypasses destructors/finalizers and the still-running workers, and the OS reclaims the mapping,
    // the staged temp file's mapping, and the worker threads. This is correct for a diagnostic that
    // measures INIT (not shutdown); `set`/`dir` are intentionally left for the OS to reclaim.
    let _ = (&set, &dir);
    // SAFETY: 2026-06-05 — `_exit(2)` is async-signal-safe and terminates the process without running
    // atexit/finalizers or unwinding; all output is already flushed above. This never returns.
    unsafe { libc::_exit(0) };
}

/// Best-effort check that the live mapping containing `addr` is executable, by scanning
/// `/proc/self/maps` for the region whose `[start,end)` covers `addr` and testing its `x` perm bit.
/// Returns `None` if `/proc/self/maps` is unavailable or the address is not found (detect, don't
/// assume — the caller treats `None` as "unknown", not "not executable").
fn proc_maps_is_exec(addr: u64) -> Option<bool> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in maps.lines() {
        // Format: "start-end perms offset dev inode pathname"
        let mut parts = line.split_whitespace();
        let range = parts.next()?;
        let perms = parts.next()?;
        let (start_hex, end_hex) = range.split_once('-')?;
        let start = u64::from_str_radix(start_hex, 16).ok()?;
        let end = u64::from_str_radix(end_hex, 16).ok()?;
        if addr >= start && addr < end {
            return Some(perms.as_bytes().get(2) == Some(&b'x'));
        }
    }
    None
}

/// Install a minimal `SIGSEGV`/`SIGABRT`/`SIGBUS`/`SIGILL`/`SIGFPE` handler that logs the faulting
/// constructor index + the published constructor address + the hardware fault address (`si_addr`)
/// using only async-signal-safe primitives (`write`/`_exit`), then exits the process non-zero.
fn install_crash_handler() -> Result<(), String> {
    // SAFETY: 2026-06-05 — `sigaction` with a `SA_SIGINFO` handler is the standard POSIX way to
    // install a signal handler. The handler (`crash_handler`) calls only async-signal-safe functions
    // (`libc::write`, atomic loads, integer formatting into a stack buffer, `libc::_exit`), so it is
    // safe to run from a signal context. We zero-initialize the `sigaction` struct (all-zero is a
    // valid empty mask + default flags) and only set the fields we use. `sa_sigaction` holds the
    // handler pointer when `SA_SIGINFO` is set (the libc union is represented by `sa_sigaction`).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = crash_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        for sig in [
            libc::SIGSEGV,
            libc::SIGABRT,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
        ] {
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(format!("sigaction({sig}) failed"));
            }
        }
    }
    Ok(())
}

/// Async-signal-safe crash handler: writes "FATAL signal N in init[i] @ base+addr (fault=si_addr)"
/// to fd 2, then `_exit(128+signo)`. Calls only `write`/`_exit`/atomic loads/integer formatting.
extern "C" fn crash_handler(signo: c_int, info: *mut libc::siginfo_t, _ctx: *mut std::ffi::c_void) {
    // si_addr: the faulting memory address (valid for SIGSEGV/SIGBUS; arbitrary otherwise — still
    // informative). Read it defensively.
    let fault_addr: u64 = if info.is_null() {
        0
    } else {
        // SAFETY: 2026-06-05 — the kernel passes a valid `siginfo_t*` with `SA_SIGINFO`; reading
        // `si_addr` is a plain field read of a kernel-provided, properly-aligned struct.
        unsafe { (*info).si_addr() as u64 }
    };
    let idx = CURRENT_INIT_INDEX.load(Ordering::SeqCst);
    let ctor_addr = CURRENT_INIT_ADDR.load(Ordering::SeqCst);

    // Build the message in a fixed stack buffer with async-signal-safe integer formatting.
    let mut buf = [0u8; 160];
    let mut n = 0usize;
    write_bytes(&mut buf, &mut n, b"\n*** FATAL signal ");
    write_dec(&mut buf, &mut n, signo as u64);
    write_bytes(&mut buf, &mut n, b" in constructor init[");
    if idx == usize::MAX {
        write_bytes(&mut buf, &mut n, b"none");
    } else {
        write_dec(&mut buf, &mut n, idx as u64);
    }
    write_bytes(&mut buf, &mut n, b"] ctor=0x");
    write_hex(&mut buf, &mut n, ctor_addr);
    write_bytes(&mut buf, &mut n, b" fault=0x");
    write_hex(&mut buf, &mut n, fault_addr);
    write_bytes(&mut buf, &mut n, b" ***\n");

    // SAFETY: 2026-06-05 — `write(2)` and `_exit(2)` are async-signal-safe; `buf[..n]` is a valid
    // initialized byte range on the handler's stack. We write to fd 2 (stderr → the log redirect)
    // and exit non-zero so the parent observes the crash; we never return to the faulting context.
    unsafe {
        libc::write(2, buf.as_ptr() as *const std::ffi::c_void, n);
        libc::_exit(128 + signo);
    }
}

/// Append `src` to `buf` at cursor `n` (async-signal-safe; bounded by `buf.len()`).
///
/// 2026-06-12: `pub(super)` so the loader's other async-signal-safe handler (the early-fault tap
/// in [`super::native_provider`]) reuses these proven formatters instead of duplicating them.
pub(super) fn write_bytes(buf: &mut [u8], n: &mut usize, src: &[u8]) {
    for &b in src {
        if *n < buf.len() {
            buf[*n] = b;
            *n += 1;
        }
    }
}

/// Append `val` as decimal to `buf` (async-signal-safe; no allocation).
pub(super) fn write_dec(buf: &mut [u8], n: &mut usize, val: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    let mut v = val;
    if v == 0 {
        write_bytes(buf, n, b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    write_bytes(buf, n, &tmp[i..]);
}

/// Append `val` as lowercase hex to `buf` (async-signal-safe; no allocation).
pub(super) fn write_hex(buf: &mut [u8], n: &mut usize, val: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut tmp = [0u8; 16];
    let mut i = tmp.len();
    let mut v = val;
    if v == 0 {
        write_bytes(buf, n, b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        tmp[i] = HEX[(v & 0xf) as usize];
        v >>= 4;
    }
    write_bytes(buf, n, &tmp[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure init-array pointer arithmetic — GPU/VM-free, no APK, runs everywhere.

    #[test]
    fn init_array_count_divides_by_entry_size() {
        // docs/libroblox-characterization.md: DT_INIT_ARRAYSZ = 27,416 bytes → 3,427 constructors.
        assert_eq!(init_array_count(27_416), 3_427);
        assert_eq!(init_array_count(0), 0);
        assert_eq!(init_array_count(8), 1);
        assert_eq!(init_array_count(16), 2);
        // A non-multiple-of-8 size truncates (an entry is a whole 8-byte pointer).
        assert_eq!(init_array_count(15), 1);
    }

    #[test]
    fn init_array_entry_offset_strides_by_eight() {
        assert_eq!(init_array_entry_offset(0x1000, 0), 0x1000);
        assert_eq!(init_array_entry_offset(0x1000, 1), 0x1008);
        assert_eq!(init_array_entry_offset(0x1000, 2), 0x1010);
        // The last entry of the real engine's 3,427-entry array.
        assert_eq!(init_array_entry_offset(0x1000, 3_426), 0x1000 + 3_426 * 8);
    }

    #[test]
    fn write_dec_and_hex_are_correct() {
        let mut buf = [0u8; 64];
        let mut n = 0;
        write_dec(&mut buf, &mut n, 0);
        write_dec(&mut buf, &mut n, 3427);
        assert_eq!(&buf[..n], b"03427");

        let mut buf = [0u8; 64];
        let mut n = 0;
        write_hex(&mut buf, &mut n, 0);
        write_hex(&mut buf, &mut n, 0xdead_beef);
        assert_eq!(&buf[..n], b"0deadbeef");
    }

    #[test]
    fn write_bytes_is_bounded() {
        // A buffer-overrun-resistant writer: writing past capacity is silently dropped, never UB.
        let mut buf = [0u8; 4];
        let mut n = 0;
        write_bytes(&mut buf, &mut n, b"abcdefgh");
        assert_eq!(n, 4);
        assert_eq!(&buf, b"abcd");
    }
}
