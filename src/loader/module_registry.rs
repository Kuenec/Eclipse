//! Eclipse-loader module registry — the bionic `dl_iterate_phdr` / `dladdr` natives.
//!
//! 2026-06-12 — root cause this exists for (core 1223806): `libroblox.so`'s statically-linked
//! libc++abi unwinder resolves FDEs via its `dl_iterate_phdr@LIBC` import. Eclipse provided no
//! native, so the import fell through to HOST GLIBC's `dl_iterate_phdr`, which walks only glibc's
//! own link map — Eclipse's anonymously-mmapped engine images are invisible to it. On the first
//! C++ throw of the boot (the engine HTTP worker's DNS-failure error path) phase-1 unwind found
//! no FDE for any libroblox PC → `std::terminate` → Roblox's terminate handler re-raised to
//! classify → unwind failed again: a 3-frame cycle repeated 61,497 times, consuming 12.2 MB of
//! stack at 208 B/iteration — deterministically fatal. Under that gap ANY engine throw (which
//! Roblox uses routinely for recoverable errors) was process-fatal.
//!
//! The fix: every image Eclipse's own loader maps **and keeps alive** (the [`LoadedEngine`]
//! contract — see `engine::map_resolve_app_lib` registration + `LoadedEngine::drop`
//! unregistration) is recorded here with its load base, name, and the run-time address of its
//! mapped program-header table (`PT_GNU_EH_FRAME` lives behind it). [`eclipse_dl_iterate_phdr`]
//! walks these records FIRST, then delegates to host glibc for the host modules — so both the
//! engine's own PCs and host-library PCs resolve. [`eclipse_dladdr`] is the same-class companion
//! (module/symbol lookup by address; engine backtrace symbolization) with a host-glibc `dladdr`
//! fallback for host PCs.
//!
//! ABI verified against the PUBLIC bionic `libc/include/link.h` (aosp-mirror, fetched
//! 2026-06-12): bionic `struct dl_phdr_info` is the same 8 fields in the same order as glibc's
//! (the tail four — `dlpi_adds`/`dlpi_subs`/`dlpi_tls_modid`/`dlpi_tls_data` — exist on bionic
//! since API 30; the `size` callback argument versions the struct). Pinned by
//! `dl_phdr_info_layout_matches_glibc` below. `Dl_info` (4 pointers) is likewise identical.
//!
//! [`LoadedEngine`]: super::engine::LoadedEngine

use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use super::elf::DynSym;

// ---- The pinned bionic `dl_phdr_info` ----------------------------------------------------------

/// Bionic LP64 `struct dl_phdr_info` (public AOSP `libc/include/link.h`, verified 2026-06-12).
/// Field-for-field identical to glibc's `struct dl_phdr_info` on x86-64 — pinned by
/// `dl_phdr_info_layout_matches_glibc` — so the host delegation in
/// [`eclipse_dl_iterate_phdr`] can pass the caller's callback through unchanged.
#[repr(C)]
pub struct BionicDlPhdrInfo {
    /// The module's load base (`ElfW(Addr)`).
    pub dlpi_addr: u64,
    /// The module's path/name (NUL-terminated; owned by the registry record for Eclipse modules).
    pub dlpi_name: *const c_char,
    /// The run-time address of the module's mapped program-header table.
    pub dlpi_phdr: *const libc::Elf64_Phdr,
    /// Number of program headers (`ElfW(Half)`).
    pub dlpi_phnum: u16,
    /// Cumulative module-load events (cache-invalidation counter; bionic since API 30).
    pub dlpi_adds: libc::c_ulonglong,
    /// Cumulative module-unload events (cache-invalidation counter; bionic since API 30).
    pub dlpi_subs: libc::c_ulonglong,
    /// Static-TLS module id (0 = none; the engine libs carry no `PT_TLS` — verified 2026-06-12).
    pub dlpi_tls_modid: usize,
    /// This thread's TLS block for the module (null = none).
    pub dlpi_tls_data: *mut c_void,
}

/// The `dl_iterate_phdr` callback type (public bionic/glibc contract: nonzero return stops the
/// walk and becomes `dl_iterate_phdr`'s return value).
pub type DlIteratePhdrCb =
    unsafe extern "C" fn(info: *mut BionicDlPhdrInfo, size: usize, data: *mut c_void) -> c_int;

extern "C" {
    // 2026-06-12: host glibc's dl_iterate_phdr, declared over the pinned [`BionicDlPhdrInfo`]
    // (layouts field-for-field identical — see `dl_phdr_info_layout_matches_glibc`) so the host
    // delegation needs no fn-pointer transmute.
    #[link_name = "dl_iterate_phdr"]
    fn host_dl_iterate_phdr(callback: Option<DlIteratePhdrCb>, data: *mut c_void) -> c_int;
}

// ---- The registry ------------------------------------------------------------------------------

/// One defined dynamic symbol of a registered module (the `dladdr` lookup table).
struct ModuleSymbol {
    /// The symbol name (owned; `dli_sname` points at this buffer — stable across `Vec` moves,
    /// freed only when the record is unregistered, i.e. when its image is unmapped anyway).
    name: CString,
    /// `st_value` (module-relative).
    value: u64,
    /// `st_size` (containment bound; a zero-size symbol never matches, mirroring bionic).
    size: u64,
}

/// One Eclipse-loader-mapped module: everything [`eclipse_dl_iterate_phdr`] /
/// [`eclipse_dladdr`] report about it.
pub struct ModuleRecord {
    /// The path the module was loaded from (NUL-terminated; `dlpi_name` / `dli_fname`).
    name: CString,
    /// The run-time load base.
    base: u64,
    /// The mapped span in bytes (`[base, base+span)` = the containment range).
    span: u64,
    /// Run-time address of the mapped program-header table.
    phdr_addr: u64,
    /// Number of program headers.
    phnum: u16,
    /// Defined dynamic symbols, sorted by `value` (the `dladdr` table).
    syms: Vec<ModuleSymbol>,
}

/// The process-global module list. Write-locked only by [`register_module`]/
/// [`unregister_module`] (the engine-load path, boot-time, main thread); the natives take read
/// locks, so concurrent engine-thread walks never contend. 2026-06-12: callbacks run UNDER the
/// read lock — the same property as bionic's loader lock (a callback that re-enters
/// `dl_iterate_phdr`/`dladdr` re-reads, which `RwLock` allows; registration never happens from a
/// callback).
static MODULES: RwLock<Vec<ModuleRecord>> = RwLock::new(Vec::new());

/// Cumulative registrations (the Eclipse-side `dlpi_adds`). Host entries carry glibc's own
/// counters — two counter domains; consumers use them only for unwind-cache invalidation, where
/// a mismatch at worst flushes a cache (never wrong unwind data). 2026-06-12.
static MODULE_ADDS: AtomicU64 = AtomicU64::new(0);
/// Cumulative unregistrations (the Eclipse-side `dlpi_subs`).
static MODULE_SUBS: AtomicU64 = AtomicU64::new(0);

/// ELF constants for [`ModuleRecord::for_image`] (the same public ELF-64 layout `elf.rs` decodes:
/// `e_phoff`@32, `e_phentsize`@54, `e_phnum`@56; `Elf64_Phdr` is 56 bytes).
const E_PHOFF_OFF: usize = 32;
const E_PHENTSIZE_OFF: usize = 54;
const E_PHNUM_OFF: usize = 56;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;

impl ModuleRecord {
    /// Build a record for an image mapped at `base` (span `span` bytes) from its original file
    /// bytes + decoded dynamic symbols.
    ///
    /// The program-header run-time address is derived the way the bionic loader derives it:
    /// prefer `PT_PHDR`'s `p_vaddr`; otherwise locate the `PT_LOAD` whose file range contains
    /// `[e_phoff, e_phoff + e_phnum*56)` and translate the file offset into that segment's vaddr.
    /// An image whose program headers are not covered by any `PT_LOAD` cannot be reported with a
    /// valid `dlpi_phdr` (consumers dereference it), so it is a typed `Err` — the caller logs and
    /// skips it rather than fabricate a pointer. 2026-06-12.
    pub fn for_image(
        path: &Path,
        file_bytes: &[u8],
        dynsyms: &[DynSym],
        base: u64,
        span: u64,
    ) -> Result<Self, String> {
        let read_u16 = |off: usize| -> Result<u16, String> {
            file_bytes
                .get(off..off + 2)
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .ok_or_else(|| format!("ELF header truncated at {off}"))
        };
        let read_u32 = |off: usize| -> Result<u32, String> {
            file_bytes
                .get(off..off + 4)
                .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
                .ok_or_else(|| format!("program header truncated at {off}"))
        };
        let read_u64 = |off: usize| -> Result<u64, String> {
            file_bytes
                .get(off..off + 8)
                .map(|s| u64::from_le_bytes(s.try_into().expect("8-byte slice")))
                .ok_or_else(|| format!("program header truncated at {off}"))
        };

        let e_phoff = read_u64(E_PHOFF_OFF)? as usize;
        let e_phentsize = read_u16(E_PHENTSIZE_OFF)? as usize;
        let e_phnum = read_u16(E_PHNUM_OFF)?;
        if e_phentsize != PHDR_SIZE {
            return Err(format!("e_phentsize {e_phentsize} != {PHDR_SIZE}"));
        }
        let table_len = (e_phnum as usize)
            .checked_mul(PHDR_SIZE)
            .ok_or("phnum * phentsize overflow")?;
        let table_end = e_phoff
            .checked_add(table_len)
            .ok_or("e_phoff + table overflow")?;

        // Pass 1: a PT_PHDR gives the table's vaddr directly. Pass 2: find the PT_LOAD whose
        // FILE range contains the table and translate the offset into its vaddr space.
        let mut phdr_vaddr: Option<u64> = None;
        let mut load_hit: Option<u64> = None;
        for i in 0..e_phnum as usize {
            let ph = e_phoff + i * PHDR_SIZE;
            let p_type = read_u32(ph)?;
            let p_offset = read_u64(ph + 8)?;
            let p_vaddr = read_u64(ph + 16)?;
            let p_filesz = read_u64(ph + 32)?;
            if p_type == PT_PHDR {
                phdr_vaddr = Some(p_vaddr);
            }
            if p_type == PT_LOAD
                && (p_offset as usize) <= e_phoff
                && (table_end as u64) <= p_offset.saturating_add(p_filesz)
            {
                load_hit = Some(p_vaddr + (e_phoff as u64 - p_offset));
            }
        }
        let vaddr = phdr_vaddr
            .or(load_hit)
            .ok_or("program-header table not covered by PT_PHDR or any PT_LOAD")?;

        let name = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| "path contains a NUL byte".to_string())?;
        // The dladdr table: defined (shndx != SHN_UNDEF), named symbols, sorted by value.
        let mut syms: Vec<ModuleSymbol> = dynsyms
            .iter()
            .filter(|s| s.shndx != 0 && !s.name.is_empty())
            .filter_map(|s| {
                CString::new(s.name.as_str()).ok().map(|name| ModuleSymbol {
                    name,
                    value: s.value,
                    size: s.size,
                })
            })
            .collect();
        syms.sort_by_key(|s| s.value);

        Ok(Self {
            name,
            base,
            span,
            phdr_addr: base + vaddr,
            phnum: e_phnum,
            syms,
        })
    }

    /// The record's load base (the registry key for [`unregister_module`]).
    #[must_use]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The record's program-header table address (test/diagnostic accessor).
    #[must_use]
    pub fn phdr_addr(&self) -> u64 {
        self.phdr_addr
    }

    /// The record's program-header count (test/diagnostic accessor).
    #[must_use]
    pub fn phnum(&self) -> u16 {
        self.phnum
    }
}

/// Register a mapped module. Call ONLY where the image is committed to stay mapped for as long
/// as the record stays registered (the [`LoadedEngine`](super::engine::LoadedEngine) contract);
/// `unregister_module` is the symmetric removal (`LoadedEngine::drop`).
pub fn register_module(record: ModuleRecord) {
    let mut guard = MODULES.write().unwrap_or_else(|e| e.into_inner());
    guard.push(record);
    MODULE_ADDS.fetch_add(1, Ordering::Relaxed);
}

/// Unregister the module whose load base is `base`. Returns `true` if a record was removed
/// (idempotent: a base that was never registered — e.g. an image dropped before its
/// registration point — is a no-op `false`).
pub fn unregister_module(base: u64) -> bool {
    let mut guard = MODULES.write().unwrap_or_else(|e| e.into_inner());
    match guard.iter().position(|m| m.base == base) {
        Some(i) => {
            guard.remove(i);
            MODULE_SUBS.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Number of currently registered Eclipse modules (diagnostics/tests).
#[must_use]
pub fn registered_module_count() -> usize {
    MODULES.read().unwrap_or_else(|e| e.into_inner()).len()
}

/// Describe `addr` against the registered modules: `"<file name>+0x<offset>"` for a hit, `None`
/// for an address in no Eclipse module (the caller may fall back to host `dladdr`). Used by the
/// `sigaltstack` attribution log (core 1223806 — name WHO registers each altstack).
#[must_use]
pub fn describe_address(addr: u64) -> Option<String> {
    let guard = MODULES.read().unwrap_or_else(|e| e.into_inner());
    let m = guard
        .iter()
        .find(|m| addr >= m.base && addr - m.base < m.span)?;
    // The file name (not the full staged path) keeps the log line readable.
    let name = m.name.to_string_lossy();
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    Some(format!("{short}+{:#x}", addr - m.base))
}

// ---- dl_iterate_phdr ---------------------------------------------------------------------------

/// The walk core, parameterized over the module list for testability (the
/// `module_registry_enumerates_loader_mapped_and_host_modules` pin in `link::tests` runs it over
/// a local list — `pub(crate)` for exactly that). Eclipse modules first (engine PCs are the hot
/// lookups), then the host modules via glibc delegation. Honors the public contract: the first
/// nonzero callback return stops the walk and is returned.
pub(crate) fn iterate_with_host(
    modules: &[ModuleRecord],
    adds: u64,
    subs: u64,
    callback: DlIteratePhdrCb,
    data: *mut c_void,
) -> c_int {
    for m in modules {
        let mut info = BionicDlPhdrInfo {
            dlpi_addr: m.base,
            dlpi_name: m.name.as_ptr(),
            dlpi_phdr: m.phdr_addr as *const libc::Elf64_Phdr,
            dlpi_phnum: m.phnum,
            dlpi_adds: adds,
            dlpi_subs: subs,
            // The engine libs carry no PT_TLS (re-verified for core 1223806; the loader's
            // static-TLS layout would own these fields if one ever does).
            dlpi_tls_modid: 0,
            dlpi_tls_data: std::ptr::null_mut(),
        };
        // SAFETY: 2026-06-12 — `info` is a valid, fully initialized struct for the duration of
        // the call; `size` is the full bionic API-30+ struct size (the callback's version gate);
        // `callback` is the caller's fn pointer (the public contract makes calling it with these
        // arguments the caller's expectation). `dlpi_phdr`/`dlpi_name` point into the registered
        // record's mapped image / owned CString, both alive while the record is registered.
        let rc = unsafe { callback(&mut info, std::mem::size_of::<BionicDlPhdrInfo>(), data) };
        if rc != 0 {
            return rc;
        }
    }
    // SAFETY: 2026-06-12 — glibc's dl_iterate_phdr with the caller's own callback/data; the
    // pinned-identical struct layout makes the type reinterpretation sound (see the extern decl).
    unsafe { host_dl_iterate_phdr(Some(callback), data) }
}

/// `int dl_iterate_phdr(int (*cb)(struct dl_phdr_info*, size_t, void*), void* data)` — the
/// bionic-semantics native [`super::native_provider::EclipseNativeProvider`] registers so the
/// engine's `dl_iterate_phdr@LIBC` import binds to ECLIPSE, not host glibc (whose walk can never
/// contain the Eclipse-mapped images — core 1223806, 2026-06-12). Walks the Eclipse-loader
/// modules first, then delegates to host glibc for the host link map.
///
/// # Safety
/// `callback` must be a valid `dl_iterate_phdr` callback (the importing module's own code);
/// `data` is passed through opaquely.
pub unsafe extern "C" fn eclipse_dl_iterate_phdr(
    callback: Option<DlIteratePhdrCb>,
    data: *mut c_void,
) -> c_int {
    let Some(callback) = callback else {
        // A null callback has nothing to observe; 0 = "walk completed" (glibc would fault —
        // intentional defensive divergence, unreachable from real consumers).
        return 0;
    };
    let guard = MODULES.read().unwrap_or_else(|e| e.into_inner());
    iterate_with_host(
        &guard,
        MODULE_ADDS.load(Ordering::Relaxed),
        MODULE_SUBS.load(Ordering::Relaxed),
        callback,
        data,
    )
}

// ---- dladdr ------------------------------------------------------------------------------------

/// The `dladdr` lookup core over an explicit module list (testable like [`iterate_with_host`]).
/// Returns the containing module and (if any) the containing defined symbol, as raw pointers into
/// the record's owned buffers (valid while the record stays registered — i.e. while its image
/// stays mapped). Symbol containment matches the public bionic rule: defined, `st_value <= a`,
/// `a < st_value + st_size` (zero-size symbols never match).
#[allow(clippy::type_complexity)] // one call site + tests; the tuple is the whole story.
fn dladdr_lookup(
    modules: &[ModuleRecord],
    addr: u64,
) -> Option<(*const c_char, u64, Option<(*const c_char, u64)>)> {
    let m = modules
        .iter()
        .find(|m| addr >= m.base && addr - m.base < m.span)?;
    let rel = addr - m.base;
    // Sorted by value: candidates are everything starting at or before `rel`; scan backward for
    // the first (nearest-starting) one whose size contains `rel`.
    let idx = m.syms.partition_point(|s| s.value <= rel);
    let sym = m.syms[..idx]
        .iter()
        .rev()
        .find(|s| rel < s.value + s.size)
        .map(|s| (s.name.as_ptr(), m.base + s.value));
    Some((m.name.as_ptr(), m.base, sym))
}

/// `int dladdr(const void* addr, Dl_info* info)` — the same-class companion native (the
/// module-introspection-blindness class of core 1223806): an address inside an Eclipse-loader
/// module resolves against the loader's module/symbol table; any other address forwards to host
/// glibc `dladdr` (host PCs keep their host symbolization). `Dl_info` is layout-identical
/// bionic/glibc (4 pointers). Nonzero = success, per the public contract.
///
/// # Safety
/// `info` must point to a writable `Dl_info` (null is tolerated: returns 0).
pub unsafe extern "C" fn eclipse_dladdr(addr: *const c_void, info: *mut libc::Dl_info) -> c_int {
    if info.is_null() {
        return 0;
    }
    let guard = MODULES.read().unwrap_or_else(|e| e.into_inner());
    if let Some((fname, fbase, sym)) = dladdr_lookup(&guard, addr as u64) {
        let (sname, saddr) = match sym {
            Some((n, a)) => (n, a as *mut c_void),
            None => (std::ptr::null(), std::ptr::null_mut()),
        };
        // SAFETY: 2026-06-12 — `info` is non-null and writable per the contract; the pointers
        // written are the registered record's owned buffers (alive while its image is mapped),
        // matching bionic's "valid until the library is unloaded" semantics.
        unsafe {
            (*info).dli_fname = fname;
            (*info).dli_fbase = fbase as *mut c_void;
            (*info).dli_sname = sname;
            (*info).dli_saddr = saddr;
        }
        return 1;
    }
    drop(guard);
    // SAFETY: 2026-06-12 — host glibc dladdr over the caller's pointers; `info` non-null checked.
    unsafe { libc::dladdr(addr, info) }
}

/// Unsafe-free walk wrappers for `link::tests` (link.rs is `#![forbid(unsafe_code)]`; the raw
/// callback FFI lives here, next to the natives it exercises). Test builds only.
#[cfg(test)]
pub(crate) mod walk_support {
    use super::{iterate_with_host, BionicDlPhdrInfo, ModuleRecord};
    use std::ffi::{c_int, c_void};

    /// One collected `dl_phdr_info` observation (the callback's view).
    pub(crate) struct PhdrSeen {
        pub(crate) addr: u64,
        pub(crate) name: String,
        pub(crate) phnum: u16,
        pub(crate) size: usize,
        /// `p_type` of the reported table's first entry — proves `dlpi_phdr` is dereferenceable
        /// (the consumer contract: unwinders index it). `u32::MAX` if null/empty.
        pub(crate) first_p_type: u32,
    }

    /// Collecting callback: records every module passed to it; returns 0 (walk everything).
    unsafe extern "C" fn collect_cb(
        info: *mut BionicDlPhdrInfo,
        size: usize,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: 2026-06-12 — the walk passes a valid, initialized info struct; `data` is the
        // `Vec<PhdrSeen>` the wrapper below handed in, exclusively borrowed for the walk.
        unsafe {
            let out = &mut *data.cast::<Vec<PhdrSeen>>();
            let name = if (*info).dlpi_name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr((*info).dlpi_name)
                    .to_string_lossy()
                    .into_owned()
            };
            let first_p_type = if (*info).dlpi_phnum > 0 && !(*info).dlpi_phdr.is_null() {
                (*(*info).dlpi_phdr).p_type
            } else {
                u32::MAX
            };
            out.push(PhdrSeen {
                addr: (*info).dlpi_addr,
                name,
                phnum: (*info).dlpi_phnum,
                size,
                first_p_type,
            });
        }
        0
    }

    /// Early-stop callback: returns 7 on the very first module (the rc-stop contract).
    unsafe extern "C" fn stop_first_cb(
        _info: *mut BionicDlPhdrInfo,
        _size: usize,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: 2026-06-12 — `data` is the wrapper's counter, exclusively borrowed.
        unsafe { *data.cast::<usize>() += 1 };
        7
    }

    /// Walk `modules` + the host link map, collecting every observation.
    pub(crate) fn collect(modules: &[ModuleRecord]) -> (c_int, Vec<PhdrSeen>) {
        let mut seen: Vec<PhdrSeen> = Vec::new();
        let rc = iterate_with_host(modules, 1, 0, collect_cb, (&raw mut seen).cast::<c_void>());
        (rc, seen)
    }

    /// Walk with a callback that stops (rc 7) on the first module; returns (walk rc, call count).
    pub(crate) fn stop_after_first(modules: &[ModuleRecord]) -> (c_int, usize) {
        let mut calls = 0usize;
        let rc = iterate_with_host(
            modules,
            1,
            0,
            stop_first_cb,
            (&raw mut calls).cast::<c_void>(),
        );
        (rc, calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn dl_phdr_info_layout_matches_glibc() {
        // THE ABI pin (2026-06-12): bionic's public link.h declares the same 8 fields in the
        // same order as glibc — this is what makes (a) the host-callback pass-through and (b)
        // registering the Eclipse native for a glibc-shaped import site sound. A drift here
        // would hand the engine's unwinder a scrambled info struct.
        assert_eq!(
            std::mem::size_of::<BionicDlPhdrInfo>(),
            std::mem::size_of::<libc::dl_phdr_info>()
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_addr),
            offset_of!(libc::dl_phdr_info, dlpi_addr)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_name),
            offset_of!(libc::dl_phdr_info, dlpi_name)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_phdr),
            offset_of!(libc::dl_phdr_info, dlpi_phdr)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_phnum),
            offset_of!(libc::dl_phdr_info, dlpi_phnum)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_adds),
            offset_of!(libc::dl_phdr_info, dlpi_adds)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_subs),
            offset_of!(libc::dl_phdr_info, dlpi_subs)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_tls_modid),
            offset_of!(libc::dl_phdr_info, dlpi_tls_modid)
        );
        assert_eq!(
            offset_of!(BionicDlPhdrInfo, dlpi_tls_data),
            offset_of!(libc::dl_phdr_info, dlpi_tls_data)
        );
        // The bionic LP64 layout in absolute terms (8+8+8+2(+6 pad)+8+8+8+8 = 64).
        assert_eq!(std::mem::size_of::<BionicDlPhdrInfo>(), 64);
        assert_eq!(offset_of!(BionicDlPhdrInfo, dlpi_phnum), 24);
        assert_eq!(offset_of!(BionicDlPhdrInfo, dlpi_adds), 32);
    }

    /// A minimal ELF-64 header + program-header table (just enough for
    /// [`ModuleRecord::for_image`]'s derivation — no parse/map step reads it).
    fn header_with_phdrs(phdrs: &[(u32, u64, u64, u64)]) -> Vec<u8> {
        // (p_type, p_offset, p_vaddr, p_filesz) per entry; table at 0x40.
        let mut buf = vec![0u8; 0x40 + phdrs.len() * PHDR_SIZE];
        buf[E_PHOFF_OFF..E_PHOFF_OFF + 8].copy_from_slice(&0x40u64.to_le_bytes());
        buf[E_PHENTSIZE_OFF..E_PHENTSIZE_OFF + 2]
            .copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        buf[E_PHNUM_OFF..E_PHNUM_OFF + 2].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
        for (i, &(p_type, p_offset, p_vaddr, p_filesz)) in phdrs.iter().enumerate() {
            let ph = 0x40 + i * PHDR_SIZE;
            buf[ph..ph + 4].copy_from_slice(&p_type.to_le_bytes());
            buf[ph + 8..ph + 16].copy_from_slice(&p_offset.to_le_bytes());
            buf[ph + 16..ph + 24].copy_from_slice(&p_vaddr.to_le_bytes());
            buf[ph + 32..ph + 40].copy_from_slice(&p_filesz.to_le_bytes());
        }
        buf
    }

    #[test]
    fn for_image_derives_phdr_addr_via_pt_phdr_then_pt_load() {
        let base = 0x10_0000u64;
        // PT_PHDR present: its p_vaddr wins.
        let bytes = header_with_phdrs(&[(PT_PHDR, 0x40, 0x40, 0), (PT_LOAD, 0, 0, 0x4000)]);
        let rec = ModuleRecord::for_image(Path::new("/tmp/a.so"), &bytes, &[], base, 0x4000)
            .expect("PT_PHDR derivation");
        assert_eq!(rec.phdr_addr(), base + 0x40);
        assert_eq!(rec.phnum(), 2);

        // No PT_PHDR: the containing PT_LOAD translates the file offset (vaddr 0x1000 - offset 0
        // → table vaddr 0x1040).
        let bytes = header_with_phdrs(&[(PT_LOAD, 0, 0x1000, 0x4000)]);
        let rec = ModuleRecord::for_image(Path::new("/tmp/b.so"), &bytes, &[], base, 0x8000)
            .expect("PT_LOAD derivation");
        assert_eq!(rec.phdr_addr(), base + 0x1040);
        assert_eq!(rec.phnum(), 1);

        // Neither covers the table: a typed Err, never a fabricated pointer.
        let bytes = header_with_phdrs(&[(PT_LOAD, 0x2000, 0x2000, 0x100)]);
        assert!(
            ModuleRecord::for_image(Path::new("/tmp/c.so"), &bytes, &[], base, 0x100).is_err(),
            "uncovered program headers must be a typed Err"
        );
    }

    /// Build a synthetic record (no real image needed for the pure-lookup tests).
    fn record(name: &str, base: u64, span: u64, syms: &[(&str, u64, u64)]) -> ModuleRecord {
        let mut syms: Vec<ModuleSymbol> = syms
            .iter()
            .map(|&(n, value, size)| ModuleSymbol {
                name: CString::new(n).unwrap(),
                value,
                size,
            })
            .collect();
        syms.sort_by_key(|s| s.value);
        ModuleRecord {
            name: CString::new(name).unwrap(),
            base,
            span,
            phdr_addr: base + 0x40,
            phnum: 1,
            syms,
        }
    }

    #[test]
    fn dladdr_lookup_finds_containing_module_and_symbol() {
        let mods = [record(
            "/tmp/mod.so",
            0x7f00_0000_0000,
            0x10000,
            &[
                ("alpha", 0x100, 0x40),
                ("beta", 0x200, 0x10),
                ("zero", 0x300, 0),
            ],
        )];
        // Inside alpha.
        let (fname, fbase, sym) =
            dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x120).expect("module hit");
        // SAFETY: the pointers come from the record's live CStrings.
        let fname = unsafe { std::ffi::CStr::from_ptr(fname) };
        assert_eq!(fname.to_str().unwrap(), "/tmp/mod.so");
        assert_eq!(fbase, 0x7f00_0000_0000);
        let (sname, saddr) = sym.expect("containing symbol");
        // SAFETY: as above.
        let sname = unsafe { std::ffi::CStr::from_ptr(sname) };
        assert_eq!(sname.to_str().unwrap(), "alpha");
        assert_eq!(saddr, 0x7f00_0000_0000 + 0x100);

        // Between symbols (alpha ends at 0x140): module hit, no symbol — bionic's containment rule.
        let (_, _, sym) = dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x150).expect("module hit");
        assert!(sym.is_none(), "non-contained address must yield no symbol");

        // At a zero-size symbol's exact value: never matches (bionic rule).
        let (_, _, sym) = dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x300).expect("module hit");
        assert!(sym.is_none(), "zero-size symbols never match");

        // Outside every module: None (the native then forwards to host dladdr).
        assert!(dladdr_lookup(&mods, 0x1000).is_none());
    }

    #[test]
    fn eclipse_dladdr_falls_back_to_host_for_host_pcs() {
        // A host-libc PC (glibc `toupper`) must resolve through the host fallback: the registry
        // holds no module containing it (engine modules are registered only by the engine-load
        // path, which unit tests never run).
        let mut info: libc::Dl_info = libc::Dl_info {
            dli_fname: std::ptr::null(),
            dli_fbase: std::ptr::null_mut(),
            dli_sname: std::ptr::null(),
            dli_saddr: std::ptr::null_mut(),
        };
        let addr = libc::toupper as *const c_void;
        // SAFETY: valid out-param; `addr` is a real host code address.
        let rc = unsafe { eclipse_dladdr(addr, &mut info) };
        assert_ne!(rc, 0, "host dladdr must resolve a glibc PC");
        assert!(!info.dli_fname.is_null(), "host hit names its module");
        // SAFETY: null info is the tolerated degenerate form.
        assert_eq!(unsafe { eclipse_dladdr(addr, std::ptr::null_mut()) }, 0);
    }

    #[test]
    fn describe_address_names_module_plus_offset() {
        // Exercise the registry round-trip on a synthetic record (register → describe →
        // unregister; unregistration makes this test order-independent with others).
        let base = 0x7abc_0000_0000u64;
        register_module(record("/tmp/stage/libattr-test.so", base, 0x2000, &[]));
        assert_eq!(
            describe_address(base + 0x123).as_deref(),
            Some("libattr-test.so+0x123")
        );
        assert!(unregister_module(base), "registered base must unregister");
        assert!(describe_address(base + 0x123).is_none());
        assert!(!unregister_module(base), "second unregister is a no-op");
    }
}
