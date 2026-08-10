use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeError {
    Io(std::io::ErrorKind),

    Empty,

    Map(std::io::ErrorKind),

    Elf(String),

    Cache(&'static str),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(kind) => write!(f, "payload read failed: {kind}"),
            Self::Empty => write!(f, "payload file is empty"),
            Self::Map(kind) => write!(f, "payload mmap failed: {kind}"),
            Self::Elf(e) => write!(f, "payload ELF parse failed: {e}"),
            Self::Cache(why) => write!(f, "ld.so.cache unusable: {why}"),
        }
    }
}

impl std::error::Error for ProbeError {}

struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapped {
    fn open(path: &Path) -> Result<Self, ProbeError> {
        let file = std::fs::File::open(path).map_err(|e| ProbeError::Io(e.kind()))?;
        let len = file.metadata().map_err(|e| ProbeError::Io(e.kind()))?.len();
        let len = usize::try_from(len).map_err(|_| ProbeError::Empty)?;
        if len == 0 {
            return Err(ProbeError::Empty);
        }

        let ptr = unsafe {
            use std::os::fd::AsRawFd as _;
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(ProbeError::Map(std::io::Error::last_os_error().kind()));
        }
        Ok(Self { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

pub(crate) fn read_needed(libcef: &Path) -> Result<Vec<String>, ProbeError> {
    let mapped = Mapped::open(libcef)?;
    let image = crate::loader::elf::ElfImage::parse(mapped.bytes())
        .map_err(|e| ProbeError::Elf(e.to_string()))?;
    image.needed().map_err(|e| ProbeError::Elf(e.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FamilyPkgs {
    pub(crate) family: &'static str,
    pub(crate) apt: &'static str,
    pub(crate) dnf: &'static str,
    pub(crate) pacman: &'static str,
}

const BASE_GLIBC_SKIP: &[&str] = &[
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "ld-linux-x86-64.so.2",
    "libgcc_s.so.1",
];

const KNOWN_LIBS: &[(&str, FamilyPkgs)] = &[
    ("libnspr4.so", pkgs("nspr", "libnspr4", "nspr", "nspr")),
    ("libnss3.so", pkgs("nss", "libnss3", "nss", "nss")),
    ("libnssutil3.so", pkgs("nss", "libnss3", "nss", "nss")),
    ("libsmime3.so", pkgs("nss", "libnss3", "nss", "nss")),
    (
        "libatk-1.0.so.0",
        pkgs("atk", "libatk1.0-0", "atk", "at-spi2-core"),
    ),
    (
        "libatk-bridge-2.0.so.0",
        pkgs(
            "atk-bridge",
            "libatk-bridge2.0-0",
            "at-spi2-atk",
            "at-spi2-core",
        ),
    ),
    (
        "libatspi.so.0",
        pkgs("at-spi", "libatspi2.0-0", "at-spi2-core", "at-spi2-core"),
    ),
    (
        "libcups.so.2",
        pkgs("cups", "libcups2", "cups-libs", "libcups"),
    ),
    (
        "libgbm.so.1",
        pkgs("gbm/mesa", "libgbm1", "mesa-libgbm", "mesa"),
    ),
    (
        "libxkbcommon.so.0",
        pkgs("xkbcommon", "libxkbcommon0", "libxkbcommon", "libxkbcommon"),
    ),
    (
        "libcairo.so.2",
        pkgs("cairo", "libcairo2", "cairo", "cairo"),
    ),
    (
        "libpango-1.0.so.0",
        pkgs("pango", "libpango-1.0-0", "pango", "pango"),
    ),
    (
        "libasound.so.2",
        pkgs("alsa", "libasound2", "alsa-lib", "alsa-lib"),
    ),
    ("libX11.so.6", pkgs("x11", "libx11-6", "libX11", "libx11")),
    (
        "libXcomposite.so.1",
        pkgs(
            "x11-ext",
            "libxcomposite1",
            "libXcomposite",
            "libxcomposite",
        ),
    ),
    (
        "libXdamage.so.1",
        pkgs("x11-ext", "libxdamage1", "libXdamage", "libxdamage"),
    ),
    (
        "libXext.so.6",
        pkgs("x11-ext", "libxext6", "libXext", "libxext"),
    ),
    (
        "libXfixes.so.3",
        pkgs("x11-ext", "libxfixes3", "libXfixes", "libxfixes"),
    ),
    (
        "libXrandr.so.2",
        pkgs("x11-ext", "libxrandr2", "libXrandr", "libxrandr"),
    ),
    ("libxcb.so.1", pkgs("xcb", "libxcb1", "libxcb", "libxcb")),
    (
        "libglib-2.0.so.0",
        pkgs("glib", "libglib2.0-0", "glib2", "glib2"),
    ),
    (
        "libgobject-2.0.so.0",
        pkgs("glib", "libglib2.0-0", "glib2", "glib2"),
    ),
    (
        "libgio-2.0.so.0",
        pkgs("glib", "libglib2.0-0", "glib2", "glib2"),
    ),
    (
        "libdbus-1.so.3",
        pkgs("dbus", "libdbus-1-3", "dbus-libs", "dbus"),
    ),
    (
        "libexpat.so.1",
        pkgs("expat", "libexpat1", "expat", "expat"),
    ),
    (
        "libudev.so.1",
        pkgs("udev", "libudev1", "systemd-libs", "systemd-libs"),
    ),
];

const fn pkgs(
    family: &'static str,
    apt: &'static str,
    dnf: &'static str,
    pacman: &'static str,
) -> FamilyPkgs {
    FamilyPkgs {
        family,
        apt,
        dnf,
        pacman,
    }
}

pub(crate) fn classify(soname: &str) -> Option<&'static FamilyPkgs> {
    KNOWN_LIBS
        .iter()
        .find(|(name, _)| *name == soname)
        .map(|(_, f)| f)
}

fn is_base_glibc(soname: &str) -> bool {
    BASE_GLIBC_SKIP.contains(&soname)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Presence {
    Resolved,
    Missing,
    Inconclusive,
}

const FALLBACK_LIB_DIRS: &[&str] = &[
    "/lib64",
    "/usr/lib64",
    "/lib",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
];

pub(crate) struct ResolveCtx<'a> {
    pub(crate) helper_dir: &'a Path,

    pub(crate) ld_library_path: &'a [PathBuf],

    pub(crate) cache: Option<&'a BTreeSet<(String, String)>>,

    pub(crate) fallback_dirs: &'a [PathBuf],
}

fn is_x86_64_elf(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 20];
    if file.read_exact(&mut head).is_err() {
        return false;
    }
    head[..4] == *b"\x7fELF"
        && head[4] == 2
        && head[5] == 1
        && u16::from_le_bytes([head[18], head[19]]) == 62
}

pub(crate) fn resolve_soname(soname: &str, ctx: &ResolveCtx<'_>) -> Presence {
    if is_x86_64_elf(&ctx.helper_dir.join(soname)) {
        return Presence::Resolved;
    }
    for dir in ctx.ld_library_path {
        if is_x86_64_elf(&dir.join(soname)) {
            return Presence::Resolved;
        }
    }
    if let Some(cache) = ctx.cache {
        let start = (soname.to_string(), String::new());
        for (key, path) in cache.range(start..) {
            if key != soname {
                break;
            }
            if is_x86_64_elf(Path::new(path)) {
                return Presence::Resolved;
            }
        }
    }
    for dir in ctx.fallback_dirs {
        if is_x86_64_elf(&dir.join(soname)) {
            return Presence::Resolved;
        }
    }
    if ctx.cache.is_some() {
        Presence::Missing
    } else {
        Presence::Inconclusive
    }
}

const CACHE_MAGIC_NEW: &[u8] = b"glibc-ld.so.cache1.1";

const CACHE_HEADER_LEN: usize = 48;

const CACHE_ENTRY_LEN: usize = 24;

const CACHE_FLAGS_X86_64_LIBC6: i32 = 0x0303;

fn read_u32_le(bytes: &[u8], off: usize) -> Result<u32, ProbeError> {
    let end = off
        .checked_add(4)
        .ok_or(ProbeError::Cache("offset overflow"))?;
    let slice = bytes
        .get(off..end)
        .ok_or(ProbeError::Cache("truncated header/entry"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_cstr(bytes: &[u8], base: usize, off: usize) -> Result<String, ProbeError> {
    let start = base
        .checked_add(off)
        .ok_or(ProbeError::Cache("string offset overflow"))?;
    let tail = bytes
        .get(start..)
        .ok_or(ProbeError::Cache("string offset past EOF"))?;
    let len = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or(ProbeError::Cache("unterminated string"))?;
    std::str::from_utf8(&tail[..len])
        .map(str::to_string)
        .map_err(|_| ProbeError::Cache("non-UTF-8 string"))
}

pub(crate) fn parse_ld_so_cache(bytes: &[u8]) -> Result<BTreeSet<(String, String)>, ProbeError> {
    let base = bytes
        .windows(CACHE_MAGIC_NEW.len())
        .position(|w| w == CACHE_MAGIC_NEW)
        .ok_or(ProbeError::Cache("new-format magic not found"))?;
    let nlibs = read_u32_le(bytes, base + CACHE_MAGIC_NEW.len())? as usize;
    let entries_start = base
        .checked_add(CACHE_HEADER_LEN)
        .ok_or(ProbeError::Cache("header offset overflow"))?;
    let entries_len = nlibs
        .checked_mul(CACHE_ENTRY_LEN)
        .ok_or(ProbeError::Cache("entry table overflow"))?;
    if entries_start
        .checked_add(entries_len)
        .is_none_or(|end| end > bytes.len())
    {
        return Err(ProbeError::Cache("entry table past EOF"));
    }
    let mut out = BTreeSet::new();
    for i in 0..nlibs {
        let entry = entries_start + i * CACHE_ENTRY_LEN;

        let flags = read_u32_le(bytes, entry)? as i32;
        if flags != CACHE_FLAGS_X86_64_LIBC6 {
            continue;
        }
        let key = read_u32_le(bytes, entry + 4)? as usize;
        let value = read_u32_le(bytes, entry + 8)? as usize;
        let soname = read_cstr(bytes, base, key)?;
        let path = read_cstr(bytes, base, value)?;
        out.insert((soname, path));
    }
    Ok(out)
}

#[derive(Debug)]
pub(crate) struct MissingLib {
    pub(crate) soname: String,
    pub(crate) family_hint: Option<&'static FamilyPkgs>,
}

impl std::fmt::Display for MissingLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.family_hint {
            Some(p) => write!(
                f,
                "{} [{} — apt: {} / dnf: {} / pacman: {}]",
                self.soname, p.family, p.apt, p.dnf, p.pacman
            ),
            None => write!(f, "{} [package unknown — consult your distro]", self.soname),
        }
    }
}

#[derive(Debug)]
pub(crate) struct HostLibReport {
    pub(crate) total: usize,
    pub(crate) resolved: usize,
    pub(crate) missing: Vec<MissingLib>,
    pub(crate) inconclusive: usize,
}

impl HostLibReport {
    pub(crate) fn display_missing(&self) -> String {
        self.missing
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug)]
pub(crate) enum ProbeOutcome {
    Report(HostLibReport),

    PayloadMissing { libcef_path: PathBuf },

    Unavailable(String),
}

pub(crate) fn log_line(outcome: &ProbeOutcome) -> (bool, String) {
    match outcome {
        ProbeOutcome::Report(r) if r.missing.is_empty() => (
            false,
            format!(
                "webview host-lib probe: {}/{} direct DT_NEEDED of libcef.so resolved{}",
                r.resolved,
                r.total,
                if r.inconclusive > 0 {
                    format!(
                        " ({} inconclusive — no usable ld.so.cache; the spawn is the authority)",
                        r.inconclusive
                    )
                } else {
                    String::new()
                }
            ),
        ),
        ProbeOutcome::Report(r) => (
            true,
            format!(
                "webview host-lib probe: MISSING {} host librar{}: {} — the helper will likely \
                 fail to start; install the packages above",
                r.missing.len(),
                if r.missing.len() == 1 { "y" } else { "ies" },
                r.display_missing()
            ),
        ),
        ProbeOutcome::PayloadMissing { libcef_path } => (
            true,
            format!(
                "webview host-lib probe: the CEF payload (libcef.so) is not beside the helper \
                 binary at {} — run tools/webview-dist/package-webview.sh, or for a dev tree \
                 build crates/eclipse-webview with CEF_PATH set (cef-dll-sys copies the payload \
                 beside the binary)",
                libcef_path.display()
            ),
        ),
        ProbeOutcome::Unavailable(why) => (
            true,
            format!(
                "webview host-lib probe: unavailable ({why}) — proceeding; the spawn/handshake \
                 outcome is the authority"
            ),
        ),
    }
}

pub(crate) fn probe_needed(needed: &[String], ctx: &ResolveCtx<'_>) -> HostLibReport {
    let mut report = HostLibReport {
        total: 0,
        resolved: 0,
        missing: Vec::new(),
        inconclusive: 0,
    };
    for soname in needed {
        if is_base_glibc(soname) {
            continue;
        }
        report.total += 1;
        match resolve_soname(soname, ctx) {
            Presence::Resolved => report.resolved += 1,
            Presence::Inconclusive => report.inconclusive += 1,
            Presence::Missing => report.missing.push(MissingLib {
                soname: soname.clone(),
                family_hint: classify(soname),
            }),
        }
    }
    report
}

pub(crate) fn probe(helper_dir: &Path) -> ProbeOutcome {
    let libcef = helper_dir.join("libcef.so");
    if !libcef.is_file() {
        return ProbeOutcome::PayloadMissing {
            libcef_path: libcef,
        };
    }
    let needed = match read_needed(&libcef) {
        Ok(n) => n,
        Err(e) => return ProbeOutcome::Unavailable(e.to_string()),
    };
    let ld_library_path: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let cache = std::fs::read("/etc/ld.so.cache")
        .ok()
        .and_then(|bytes| parse_ld_so_cache(&bytes).ok());
    let fallback_dirs: Vec<PathBuf> = FALLBACK_LIB_DIRS.iter().map(PathBuf::from).collect();
    let ctx = ResolveCtx {
        helper_dir,
        ld_library_path: &ld_library_path,
        cache: cache.as_ref(),
        fallback_dirs: &fallback_dirs,
    };
    ProbeOutcome::Report(probe_needed(&needed, &ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_serves_apt_dnf_pacman_hints_for_every_plan_family() {
        for (soname, _) in KNOWN_LIBS {
            let p = classify(soname).expect("table row classifies");
            assert!(
                !p.family.is_empty()
                    && !p.apt.is_empty()
                    && !p.dnf.is_empty()
                    && !p.pacman.is_empty(),
                "{soname}: empty hint triple"
            );
        }

        for soname in [
            "libnss3.so",
            "libnspr4.so",
            "libatk-1.0.so.0",
            "libatspi.so.0",
            "libcups.so.2",
            "libgbm.so.1",
            "libxkbcommon.so.0",
            "libcairo.so.2",
            "libpango-1.0.so.0",
            "libasound.so.2",
            "libX11.so.6",
        ] {
            assert!(
                classify(soname).is_some(),
                "{soname} missing from the table"
            );
        }

        assert!(classify("libfuture-cef-dep.so.9").is_none());
        let generic = MissingLib {
            soname: "libfuture-cef-dep.so.9".into(),
            family_hint: None,
        };
        assert_eq!(
            generic.to_string(),
            "libfuture-cef-dep.so.9 [package unknown — consult your distro]"
        );

        let known = MissingLib {
            soname: "libnss3.so".into(),
            family_hint: classify("libnss3.so"),
        };
        let line = known.to_string();
        for needle in [
            "libnss3.so",
            "nss",
            "apt: libnss3",
            "dnf: nss",
            "pacman: nss",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line}");
        }
    }

    #[test]
    fn base_glibc_set_is_never_probed() {
        let needed: Vec<String> = BASE_GLIBC_SKIP.iter().map(|s| s.to_string()).collect();
        let ctx = ResolveCtx {
            helper_dir: Path::new("/nonexistent-eclipse-test"),
            ld_library_path: &[],
            cache: None,
            fallback_dirs: &[],
        };
        let report = probe_needed(&needed, &ctx);
        assert_eq!(report.total, 0);
        assert!(report.missing.is_empty());
        assert_eq!(report.inconclusive, 0);
    }

    fn synth_cache_flags(pairs: &[(&str, &str, i32)]) -> Vec<u8> {
        let mut strings: Vec<u8> = Vec::new();
        let mut offsets: Vec<(u32, u32, i32)> = Vec::new();
        let header_and_entries = CACHE_HEADER_LEN + pairs.len() * CACHE_ENTRY_LEN;
        for (soname, path, flags) in pairs {
            let key = (header_and_entries + strings.len()) as u32;
            strings.extend_from_slice(soname.as_bytes());
            strings.push(0);
            let value = (header_and_entries + strings.len()) as u32;
            strings.extend_from_slice(path.as_bytes());
            strings.push(0);
            offsets.push((key, value, *flags));
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(CACHE_MAGIC_NEW);
        buf.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        buf.extend_from_slice(&[0u8; CACHE_HEADER_LEN - 20 - 8]);
        for (key, value, flags) in offsets {
            buf.extend_from_slice(&flags.to_le_bytes());
            buf.extend_from_slice(&key.to_le_bytes());
            buf.extend_from_slice(&value.to_le_bytes());
            buf.extend_from_slice(&[0u8; 12]);
        }
        buf.extend_from_slice(&strings);
        buf
    }

    fn synth_cache(pairs: &[(&str, &str)]) -> Vec<u8> {
        let flagged: Vec<(&str, &str, i32)> = pairs
            .iter()
            .map(|(s, p)| (*s, *p, CACHE_FLAGS_X86_64_LIBC6))
            .collect();
        synth_cache_flags(&flagged)
    }

    #[test]
    fn parse_ld_so_cache_is_total_on_hostile_bytes() {
        let valid = synth_cache(&[("libnss3.so", "/usr/lib/libnss3.so")]);
        let set = parse_ld_so_cache(&valid).expect("valid cache parses");
        assert!(set.contains(&("libnss3.so".to_string(), "/usr/lib/libnss3.so".to_string())));
        let mut prefixed = b"ld.so-1.7.0-compat-header-bytes".to_vec();
        let base_shift = prefixed.len();

        prefixed.extend_from_slice(&valid);
        let set = parse_ld_so_cache(&prefixed).expect("compat-prefixed cache parses");
        assert!(set.contains(&("libnss3.so".to_string(), "/usr/lib/libnss3.so".to_string())));
        assert!(base_shift > 0);

        assert_eq!(
            parse_ld_so_cache(b"not a cache at all"),
            Err(ProbeError::Cache("new-format magic not found"))
        );

        let mut truncated = synth_cache(&[("liba.so", "/usr/lib/liba.so")]);
        truncated.truncate(CACHE_HEADER_LEN + 4);
        assert!(matches!(
            parse_ld_so_cache(&truncated),
            Err(ProbeError::Cache(_))
        ));

        let mut bad_offset = synth_cache(&[("liba.so", "/usr/lib/liba.so")]);
        let key_pos = CACHE_HEADER_LEN + 4;
        bad_offset[key_pos..key_pos + 4].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        assert!(matches!(
            parse_ld_so_cache(&bad_offset),
            Err(ProbeError::Cache(_))
        ));

        let mut huge_nlibs = synth_cache(&[("liba.so", "/usr/lib/liba.so")]);
        huge_nlibs[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            parse_ld_so_cache(&huge_nlibs),
            Err(ProbeError::Cache(_))
        ));
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-hostprobe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_elf(dir: &Path, name: &str, class: u8, machine: u16) {
        let mut head = vec![0u8; 20];
        head[..4].copy_from_slice(b"\x7fELF");
        head[4] = class;
        head[5] = 1;
        head[18..20].copy_from_slice(&machine.to_le_bytes());
        std::fs::write(dir.join(name), head).unwrap();
    }

    #[test]
    fn resolve_soname_prefers_helper_dir_then_env_then_cache_then_fallback_dirs() {
        let root = scratch("resolve");
        let helper_dir = root.join("helper");
        let env_dir = root.join("env");
        let cache_dir = root.join("cache");
        let fb_dir = root.join("fallback");
        for d in [&helper_dir, &env_dir, &cache_dir, &fb_dir] {
            std::fs::create_dir_all(d).unwrap();
        }

        let touch = |dir: &Path, name: &str| write_elf(dir, name, 2, 62);

        touch(&helper_dir, "libhelper.so");
        touch(&env_dir, "libenv.so");
        touch(&cache_dir, "libcache.so");
        touch(&fb_dir, "libfb.so");
        let mut cache = BTreeSet::new();
        cache.insert((
            "libcache.so".to_string(),
            cache_dir.join("libcache.so").display().to_string(),
        ));

        cache.insert((
            "libstale.so".to_string(),
            cache_dir.join("libstale.so").display().to_string(),
        ));
        let ld_path = [env_dir.clone()];
        let fallbacks = [fb_dir.clone()];
        let ctx = ResolveCtx {
            helper_dir: &helper_dir,
            ld_library_path: &ld_path,
            cache: Some(&cache),
            fallback_dirs: &fallbacks,
        };
        assert_eq!(resolve_soname("libhelper.so", &ctx), Presence::Resolved);
        assert_eq!(resolve_soname("libenv.so", &ctx), Presence::Resolved);
        assert_eq!(resolve_soname("libcache.so", &ctx), Presence::Resolved);
        assert_eq!(resolve_soname("libfb.so", &ctx), Presence::Resolved);
        assert_eq!(resolve_soname("libstale.so", &ctx), Presence::Missing);
        assert_eq!(resolve_soname("libabsent.so", &ctx), Presence::Missing);

        let ctx_no_cache = ResolveCtx {
            helper_dir: &helper_dir,
            ld_library_path: &ld_path,
            cache: None,
            fallback_dirs: &fallbacks,
        };
        assert_eq!(
            resolve_soname("libabsent.so", &ctx_no_cache),
            Presence::Inconclusive
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn multilib_32bit_rows_and_dir_copies_never_resolve_the_64bit_payload() {
        let cache_bytes = synth_cache_flags(&[
            ("libnss3.so", "/usr/lib32/libnss3.so", 0x0003),
            ("libnss3.so", "/usr/libx32/libnss3.so", 0x0803),
            (
                "libgood.so",
                "/usr/lib/libgood.so",
                CACHE_FLAGS_X86_64_LIBC6,
            ),
        ]);
        let set = parse_ld_so_cache(&cache_bytes).expect("multilib cache parses");
        assert!(
            !set.iter().any(|(s, _)| s == "libnss3.so"),
            "foreign-arch rows must be filtered: {set:?}"
        );
        assert!(set.contains(&("libgood.so".to_string(), "/usr/lib/libgood.so".to_string())));

        let root = scratch("multilib");
        let helper_dir = root.join("helper");
        let lib32_dir = root.join("lib32");
        for d in [&helper_dir, &lib32_dir] {
            std::fs::create_dir_all(d).unwrap();
        }
        write_elf(&lib32_dir, "libnss3.so", 1, 3);
        let mut cache = BTreeSet::new();

        cache.insert((
            "libnss3.so".to_string(),
            lib32_dir.join("libnss3.so").display().to_string(),
        ));
        let fallbacks = [lib32_dir.clone()];
        let ctx = ResolveCtx {
            helper_dir: &helper_dir,
            ld_library_path: &[],
            cache: Some(&cache),
            fallback_dirs: &fallbacks,
        };
        assert_eq!(resolve_soname("libnss3.so", &ctx), Presence::Missing);

        write_elf(&helper_dir, "libnss3.so", 2, 62);
        assert_eq!(resolve_soname("libnss3.so", &ctx), Presence::Resolved);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_verdict_never_blocks_the_spawn() {
        let missing_report = ProbeOutcome::Report(HostLibReport {
            total: 26,
            resolved: 25,
            missing: vec![MissingLib {
                soname: "libnss3.so".into(),
                family_hint: classify("libnss3.so"),
            }],
            inconclusive: 0,
        });
        let (warn, line) = log_line(&missing_report);
        assert!(warn);
        for needle in [
            "webview host-lib probe: ",
            "MISSING 1 host library",
            "libnss3.so",
            "apt: libnss3",
            "dnf: nss",
            "pacman: nss",
            "the helper will likely fail to start",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line}");
        }

        let inconclusive = ProbeOutcome::Report(HostLibReport {
            total: 26,
            resolved: 0,
            missing: Vec::new(),
            inconclusive: 26,
        });
        let (warn, line) = log_line(&inconclusive);
        assert!(!warn, "inconclusive never downgrades below proceed");
        assert!(line.contains("webview host-lib probe: 0/26"));
        assert!(line.contains("inconclusive"));

        let all_resolved = ProbeOutcome::Report(HostLibReport {
            total: 26,
            resolved: 26,
            missing: Vec::new(),
            inconclusive: 0,
        });
        let (warn, line) = log_line(&all_resolved);
        assert!(!warn);
        assert!(line.contains("webview host-lib probe: 26/26 direct DT_NEEDED"));

        let payload_missing = ProbeOutcome::PayloadMissing {
            libcef_path: PathBuf::from("/somewhere/libcef.so"),
        };
        let (warn, line) = log_line(&payload_missing);
        assert!(warn);
        for needle in [
            "webview host-lib probe: ",
            "/somewhere/libcef.so",
            "package-webview.sh",
            "CEF_PATH",
        ] {
            assert!(line.contains(needle), "missing {needle:?} in {line}");
        }

        let unavailable = ProbeOutcome::Unavailable("payload ELF parse failed: nope".into());
        let (warn, line) = log_line(&unavailable);
        assert!(warn);
        assert!(line.contains("webview host-lib probe: unavailable"));
        assert!(line.contains("the spawn/handshake outcome is the authority"));
    }

    #[test]
    fn probe_reports_payload_missing_for_an_empty_helper_dir() {
        let dir = scratch("payload-missing");
        match probe(&dir) {
            ProbeOutcome::PayloadMissing { libcef_path } => {
                assert_eq!(libcef_path, dir.join("libcef.so"));
            }
            other => panic!("expected PayloadMissing, got {other:?}"),
        }

        std::fs::write(dir.join("libcef.so"), b"definitely not an ELF").unwrap();
        assert!(matches!(probe(&dir), ProbeOutcome::Unavailable(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
