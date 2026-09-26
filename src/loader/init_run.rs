use std::ffi::{c_char, c_int};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::bionic_env::BionicEnv;
use super::link::Linker;
use super::map::{host_page_size, MapError, MappedObject};
use super::resolve::{LoadedObjectProvider, Scope};

const INIT_ENTRY_SIZE: u64 = 8;

#[must_use]
pub fn init_array_count(init_arraysz: u64) -> usize {
    (init_arraysz / INIT_ENTRY_SIZE) as usize
}

#[must_use]
pub fn init_array_entry_offset(init_array_vaddr: u64, index: usize) -> u64 {
    init_array_vaddr + (index as u64) * INIT_ENTRY_SIZE
}

pub fn constructor_addresses(
    mapped: &MappedObject,
    init: Option<u64>,
    init_array: Option<(u64, u64)>,
) -> Result<Vec<u64>, MapError> {
    let mut addresses: Vec<u64> = init
        .map(|init| mapped.load_base().wrapping_add(init))
        .into_iter()
        .collect();
    if let Some((vaddr, size)) = init_array {
        for index in 0..init_array_count(size) {
            let offset = usize::try_from(init_array_entry_offset(vaddr, index))
                .map_err(|_| MapError::SpanOverflow("init_array entry offset as usize"))?;
            addresses.push(mapped.read_u64(offset)?);
        }
    }
    addresses.retain(|&addr| addr != 0 && addr != u64::MAX);
    Ok(addresses)
}

static CURRENT_INIT_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);

static CURRENT_INIT_ADDR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum InitRunError {
    NoApk,

    Apk(String),

    Extract(PathBuf, String),

    Link(String),

    NoInitArray,

    Setup(String),
}

impl std::fmt::Display for InitRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoApk => write!(
                f,
                "no Roblox APK (set {} to an APK file or to a directory holding {} and {})",
                crate::apk::DEV_APK_ENV,
                crate::apk::BASE_APK,
                crate::apk::NATIVE_SPLIT_APK
            ),
            Self::Apk(e) => write!(f, "APK read: {e}"),
            Self::Extract(dir, e) => write!(f, "extract native libs into {}: {e}", dir.display()),
            Self::Link(e) => write!(f, "map/relocate/resolve: {e}"),
            Self::NoInitArray => write!(f, "mapped libroblox.so has no DT_INIT_ARRAY"),
            Self::Setup(e) => write!(f, "harness setup: {e}"),
        }
    }
}

impl std::error::Error for InitRunError {}

pub fn run_libroblox_init(lib_dir: &Path) -> Result<usize, InitRunError> {
    let mut log = std::io::stderr();
    let _ = writeln!(
        log,
        "eclipse __run-libroblox-init: isolated DT_INIT_ARRAY execution harness (dev-host)"
    );

    let paths = crate::apk::ApkSetPaths::from_env()
        .map_err(|e| InitRunError::Apk(e.to_string()))?
        .ok_or(InitRunError::NoApk)?;
    let mut apks = crate::apk::ApkSet::open(paths).map_err(|e| InitRunError::Apk(e.to_string()))?;
    let _ = writeln!(
        log,
        "APK: {} (native libs: {})",
        apks.base_path().display(),
        apks.native_libs_path().display()
    );

    super::ndk_registry::set_apk_path(apks.base_path().to_path_buf());

    let so_path = extract_engine(apks.native_libs_mut(), lib_dir)?;
    let _ = writeln!(log, "libroblox.so: {}", so_path.display());

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
            "WARNING: {} unresolved-strong import(s) remain ({} reloc(s); work-list: {:?}); \
             constructors may jump through null GOT slots",
            sym_stats.unresolved.len(),
            sym_stats.unresolved_strong,
            sym_stats.unresolved
        );
    }

    let obj = &set.objects[0];
    let (init, init_array_vaddr, init_arraysz, exec_ok, exec_detail) = {
        let img = obj.image().map_err(|e| InitRunError::Link(e.to_string()))?;

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
        (img.dyn_info.init, va, sz, exec_ok, exec_detail)
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

    install_crash_handler().map_err(InitRunError::Setup)?;

    let entries = constructor_addresses(&obj.mapped, init, Some((init_array_vaddr, init_arraysz)))
        .map_err(|e| InitRunError::Setup(format!("read constructors: {e}")))?;
    let count = entries.len();
    let _ = writeln!(
        log,
        "calling {count} constructors (argc=1, argv=[\"libroblox\",NULL], envp=[NULL]) …"
    );
    let _ = log.flush();

    let arg0 = b"libroblox\0";
    let mut argv: [*mut c_char; 2] = [arg0.as_ptr() as *mut c_char, std::ptr::null_mut()];
    let mut envp: [*mut c_char; 1] = [std::ptr::null_mut()];
    let argc: c_int = 1;

    let mut completed = 0usize;
    for (i, &addr) in entries.iter().enumerate() {
        let offset = addr.wrapping_sub(base);
        let _ = writeln!(log, "init[{i}/{count}] @ base+{offset:#x} (abs {addr:#x})");
        let _ = log.flush();

        CURRENT_INIT_INDEX.store(i, Ordering::SeqCst);
        CURRENT_INIT_ADDR.store(addr, Ordering::SeqCst);

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

    unsafe { libc::_exit(0) };
}

fn extract_engine(apk: &mut crate::apk::Apk, lib_dir: &Path) -> Result<PathBuf, InitRunError> {
    apk.extract_native_libs("x86_64", lib_dir)
        .map_err(|e| InitRunError::Extract(lib_dir.to_path_buf(), e.to_string()))?;
    Ok(lib_dir.join("libroblox.so"))
}

fn proc_maps_is_exec(addr: u64) -> Option<bool> {
    let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
    for line in maps.lines() {
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

fn install_crash_handler() -> Result<(), String> {
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

extern "C" fn crash_handler(signo: c_int, info: *mut libc::siginfo_t, _ctx: *mut std::ffi::c_void) {
    let fault_addr: u64 = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_addr() as u64 }
    };
    let idx = CURRENT_INIT_INDEX.load(Ordering::SeqCst);
    let ctor_addr = CURRENT_INIT_ADDR.load(Ordering::SeqCst);

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

    unsafe {
        libc::write(2, buf.as_ptr() as *const std::ffi::c_void, n);
        libc::_exit(128 + signo);
    }
}

pub(super) fn write_bytes(buf: &mut [u8], n: &mut usize, src: &[u8]) {
    for &b in src {
        if *n < buf.len() {
            buf[*n] = b;
            *n += 1;
        }
    }
}

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

    use std::io::Cursor;

    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    use super::super::link::tests::temp_dir;

    #[test]
    fn extraction_failure_names_the_destination_lib_dir() {
        let root = temp_dir("init-extract-failure");
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("lib/x86_64/libroblox.so", SimpleFileOptions::default())
            .expect("start the engine entry");
        writer
            .write_all(b"\x7fELF")
            .expect("write the engine entry");
        let apk_path = root.join("fixture.apk");
        std::fs::write(&apk_path, writer.finish().expect("finish").into_inner())
            .expect("write the fixture APK");
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write the blocking file");
        let lib_dir = blocker.join("native-libs");

        let mut apk = crate::apk::Apk::open(&apk_path).expect("open the fixture APK");
        let err = extract_engine(&mut apk, &lib_dir).expect_err("the lib dir is not creatable");

        assert!(
            matches!(&err, InitRunError::Extract(dir, _) if *dir == lib_dir),
            "{err}"
        );
        assert!(
            err.to_string().contains(&lib_dir.display().to_string()),
            "{err}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn init_array_count_divides_by_entry_size() {
        assert_eq!(init_array_count(27_416), 3_427);
        assert_eq!(init_array_count(0), 0);
        assert_eq!(init_array_count(8), 1);
        assert_eq!(init_array_count(16), 2);

        assert_eq!(init_array_count(15), 1);
    }

    #[test]
    fn init_array_entry_offset_strides_by_eight() {
        assert_eq!(init_array_entry_offset(0x1000, 0), 0x1000);
        assert_eq!(init_array_entry_offset(0x1000, 1), 0x1008);
        assert_eq!(init_array_entry_offset(0x1000, 2), 0x1010);

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
        let mut buf = [0u8; 4];
        let mut n = 0;
        write_bytes(&mut buf, &mut n, b"abcdefgh");
        assert_eq!(n, 4);
        assert_eq!(&buf, b"abcd");
    }
}
