use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use super::elf::DynSym;

#[repr(C)]
pub struct BionicDlPhdrInfo {
    pub dlpi_addr: u64,

    pub dlpi_name: *const c_char,

    pub dlpi_phdr: *const libc::Elf64_Phdr,

    pub dlpi_phnum: u16,

    pub dlpi_adds: libc::c_ulonglong,

    pub dlpi_subs: libc::c_ulonglong,

    pub dlpi_tls_modid: usize,

    pub dlpi_tls_data: *mut c_void,
}

pub type DlIteratePhdrCb =
    unsafe extern "C" fn(info: *mut BionicDlPhdrInfo, size: usize, data: *mut c_void) -> c_int;

extern "C" {

    #[link_name = "dl_iterate_phdr"]
    fn host_dl_iterate_phdr(callback: Option<DlIteratePhdrCb>, data: *mut c_void) -> c_int;
}

struct ModuleSymbol {
    name: CString,

    value: u64,

    size: u64,
}

pub struct ModuleRecord {
    name: CString,

    base: u64,

    span: u64,

    phdr_addr: u64,

    phnum: u16,

    syms: Vec<ModuleSymbol>,
}

static MODULES: RwLock<Vec<ModuleRecord>> = RwLock::new(Vec::new());

static MODULE_ADDS: AtomicU64 = AtomicU64::new(0);

static MODULE_SUBS: AtomicU64 = AtomicU64::new(0);

const E_PHOFF_OFF: usize = 32;
const E_PHENTSIZE_OFF: usize = 54;
const E_PHNUM_OFF: usize = 56;
const PHDR_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;

impl ModuleRecord {
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

    #[must_use]
    pub fn base(&self) -> u64 {
        self.base
    }

    #[must_use]
    pub fn phdr_addr(&self) -> u64 {
        self.phdr_addr
    }

    #[must_use]
    pub fn phnum(&self) -> u16 {
        self.phnum
    }
}

pub fn register_module(record: ModuleRecord) {
    let mut guard = MODULES.write().unwrap_or_else(|e| e.into_inner());
    guard.push(record);
    MODULE_ADDS.fetch_add(1, Ordering::Relaxed);
}

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

#[must_use]
pub fn registered_module_count() -> usize {
    MODULES.read().unwrap_or_else(|e| e.into_inner()).len()
}

#[must_use]
pub fn describe_address(addr: u64) -> Option<String> {
    let guard = MODULES.read().unwrap_or_else(|e| e.into_inner());
    let m = guard
        .iter()
        .find(|m| addr >= m.base && addr - m.base < m.span)?;

    let name = m.name.to_string_lossy();
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    Some(format!("{short}+{:#x}", addr - m.base))
}

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

            dlpi_tls_modid: 0,
            dlpi_tls_data: std::ptr::null_mut(),
        };

        let rc = unsafe { callback(&mut info, std::mem::size_of::<BionicDlPhdrInfo>(), data) };
        if rc != 0 {
            return rc;
        }
    }

    unsafe { host_dl_iterate_phdr(Some(callback), data) }
}

pub unsafe extern "C" fn eclipse_dl_iterate_phdr(
    callback: Option<DlIteratePhdrCb>,
    data: *mut c_void,
) -> c_int {
    let Some(callback) = callback else {
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

#[allow(clippy::type_complexity)]
fn dladdr_lookup(
    modules: &[ModuleRecord],
    addr: u64,
) -> Option<(*const c_char, u64, Option<(*const c_char, u64)>)> {
    let m = modules
        .iter()
        .find(|m| addr >= m.base && addr - m.base < m.span)?;
    let rel = addr - m.base;

    let idx = m.syms.partition_point(|s| s.value <= rel);
    let sym = m.syms[..idx]
        .iter()
        .rev()
        .find(|s| rel < s.value + s.size)
        .map(|s| (s.name.as_ptr(), m.base + s.value));
    Some((m.name.as_ptr(), m.base, sym))
}

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

        unsafe {
            (*info).dli_fname = fname;
            (*info).dli_fbase = fbase as *mut c_void;
            (*info).dli_sname = sname;
            (*info).dli_saddr = saddr;
        }
        return 1;
    }
    drop(guard);

    unsafe { libc::dladdr(addr, info) }
}

#[cfg(test)]
pub(crate) mod walk_support {
    use super::{iterate_with_host, BionicDlPhdrInfo, ModuleRecord};
    use std::ffi::{c_int, c_void};

    pub(crate) struct PhdrSeen {
        pub(crate) addr: u64,
        pub(crate) name: String,
        pub(crate) phnum: u16,
        pub(crate) size: usize,

        pub(crate) first_p_type: u32,
    }

    unsafe extern "C" fn collect_cb(
        info: *mut BionicDlPhdrInfo,
        size: usize,
        data: *mut c_void,
    ) -> c_int {
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

    unsafe extern "C" fn stop_first_cb(
        _info: *mut BionicDlPhdrInfo,
        _size: usize,
        data: *mut c_void,
    ) -> c_int {
        unsafe { *data.cast::<usize>() += 1 };
        7
    }

    pub(crate) fn collect(modules: &[ModuleRecord]) -> (c_int, Vec<PhdrSeen>) {
        let mut seen: Vec<PhdrSeen> = Vec::new();
        let rc = iterate_with_host(modules, 1, 0, collect_cb, (&raw mut seen).cast::<c_void>());
        (rc, seen)
    }

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

        assert_eq!(std::mem::size_of::<BionicDlPhdrInfo>(), 64);
        assert_eq!(offset_of!(BionicDlPhdrInfo, dlpi_phnum), 24);
        assert_eq!(offset_of!(BionicDlPhdrInfo, dlpi_adds), 32);
    }

    fn header_with_phdrs(phdrs: &[(u32, u64, u64, u64)]) -> Vec<u8> {
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

        let bytes = header_with_phdrs(&[(PT_PHDR, 0x40, 0x40, 0), (PT_LOAD, 0, 0, 0x4000)]);
        let rec = ModuleRecord::for_image(Path::new("/tmp/a.so"), &bytes, &[], base, 0x4000)
            .expect("PT_PHDR derivation");
        assert_eq!(rec.phdr_addr(), base + 0x40);
        assert_eq!(rec.phnum(), 2);

        let bytes = header_with_phdrs(&[(PT_LOAD, 0, 0x1000, 0x4000)]);
        let rec = ModuleRecord::for_image(Path::new("/tmp/b.so"), &bytes, &[], base, 0x8000)
            .expect("PT_LOAD derivation");
        assert_eq!(rec.phdr_addr(), base + 0x1040);
        assert_eq!(rec.phnum(), 1);

        let bytes = header_with_phdrs(&[(PT_LOAD, 0x2000, 0x2000, 0x100)]);
        assert!(
            ModuleRecord::for_image(Path::new("/tmp/c.so"), &bytes, &[], base, 0x100).is_err(),
            "uncovered program headers must be a typed Err"
        );
    }

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

        let (fname, fbase, sym) =
            dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x120).expect("module hit");

        let fname = unsafe { std::ffi::CStr::from_ptr(fname) };
        assert_eq!(fname.to_str().unwrap(), "/tmp/mod.so");
        assert_eq!(fbase, 0x7f00_0000_0000);
        let (sname, saddr) = sym.expect("containing symbol");

        let sname = unsafe { std::ffi::CStr::from_ptr(sname) };
        assert_eq!(sname.to_str().unwrap(), "alpha");
        assert_eq!(saddr, 0x7f00_0000_0000 + 0x100);

        let (_, _, sym) = dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x150).expect("module hit");
        assert!(sym.is_none(), "non-contained address must yield no symbol");

        let (_, _, sym) = dladdr_lookup(&mods, 0x7f00_0000_0000 + 0x300).expect("module hit");
        assert!(sym.is_none(), "zero-size symbols never match");

        assert!(dladdr_lookup(&mods, 0x1000).is_none());
    }

    #[test]
    fn eclipse_dladdr_falls_back_to_host_for_host_pcs() {
        let mut info: libc::Dl_info = libc::Dl_info {
            dli_fname: std::ptr::null(),
            dli_fbase: std::ptr::null_mut(),
            dli_sname: std::ptr::null(),
            dli_saddr: std::ptr::null_mut(),
        };
        let addr = libc::toupper as *const c_void;

        let rc = unsafe { eclipse_dladdr(addr, &mut info) };
        assert_ne!(rc, 0, "host dladdr must resolve a glibc PC");
        assert!(!info.dli_fname.is_null(), "host hit names its module");

        assert_eq!(unsafe { eclipse_dladdr(addr, std::ptr::null_mut()) }, 0);
    }

    #[test]
    fn describe_address_names_module_plus_offset() {
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
