#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::elf::{ElfError, ElfImage};
use super::map::{
    host_page_size, MapError, MapStats, MappedObject, PartialSymbolStats, SymbolRelocStats,
    TlsRelocStats,
};
use super::reloc::{self, Rela, SymbolResolver};
use super::resolve::{HostDlsymProvider, LoadedObjectProvider, Scope, ScopedResolver};
use super::tls::TlsLayout;

const R_X86_64_IRELATIVE: u32 = 37;

#[derive(Debug)]
pub enum LinkError {
    MissingDependency { soname: String, needed_by: String },

    Io { path: PathBuf, error: String },

    Parse { object: String, error: ElfError },

    Map { object: String, error: MapError },

    DynStrings { object: String, error: ElfError },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDependency { soname, needed_by } => write!(
                f,
                "dependency {soname:?} (needed by {needed_by:?}) not found on any search path"
            ),
            Self::Io { path, error } => write!(f, "reading {}: {error}", path.display()),
            Self::Parse { object, error } => write!(f, "decoding {object}: {error}"),
            Self::Map { object, error } => write!(f, "mapping {object}: {error}"),
            Self::DynStrings { object, error } => {
                write!(f, "reading dynamic strings of {object}: {error}")
            }
        }
    }
}

impl std::error::Error for LinkError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSymbol {
    pub object: String,

    pub name: String,

    pub sym_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDep {
    pub soname: String,

    pub needed_by: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelocStats {
    pub relative_applied: usize,

    pub relr_applied: usize,

    pub glob_dat_applied: usize,

    pub jump_slot_applied: usize,

    pub abs64_applied: usize,

    pub tpoff64_applied: usize,

    pub irelative_deferred: usize,
}

impl RelocStats {
    fn accumulate(&mut self, map: MapStats, sym: SymbolRelocStats, tls: TlsRelocStats) {
        self.relative_applied += map.relative_applied;
        self.relr_applied += map.relr_applied;
        self.glob_dat_applied += sym.glob_dat_applied;
        self.jump_slot_applied += sym.jump_slot_applied;
        self.abs64_applied += sym.abs64_applied;
        self.tpoff64_applied += tls.tpoff64_applied;
    }
}

pub struct LoadedObject {
    pub soname: String,

    pub path: PathBuf,

    pub bytes: Vec<u8>,

    pub mapped: MappedObject,

    pub map_stats: MapStats,

    pub sym_stats: SymbolRelocStats,

    pub tls_stats: TlsRelocStats,
}

impl LoadedObject {
    pub fn image(&self) -> Result<ElfImage<'_>, ElfError> {
        ElfImage::parse(&self.bytes)
    }

    pub fn load_base(&self) -> u64 {
        self.mapped.load_base()
    }
}

pub struct LoadedImageSet {
    pub objects: Vec<LoadedObject>,

    pub scope: Scope,

    pub tls_layout: TlsLayout,

    pub stats: RelocStats,

    pub unresolved: Vec<UnresolvedSymbol>,

    pub missing_deps: Vec<MissingDep>,

    pub relro_applied: usize,
}

impl LoadedImageSet {
    pub fn object(&self, soname: &str) -> Option<&LoadedObject> {
        self.objects.iter().find(|o| o.soname == soname)
    }

    pub fn relocate_object_symbols_partial(
        &mut self,
        soname: &str,
        scope: &Scope,
        page_size: u64,
    ) -> Result<PartialSymbolStats, LinkError> {
        let idx = self
            .objects
            .iter()
            .position(|o| o.soname == soname)
            .ok_or_else(|| LinkError::MissingDependency {
                soname: soname.to_string(),
                needed_by: "relocate_object_symbols_partial".to_string(),
            })?;

        let obj = &mut self.objects[idx];
        let LoadedObject {
            soname: obj_soname,
            bytes,
            mapped,
            ..
        } = obj;
        let img = ElfImage::parse(bytes).map_err(|error| LinkError::Parse {
            object: obj_soname.clone(),
            error,
        })?;
        mapped
            .relocate_symbols_partial(&img, scope, page_size)
            .map_err(|error| LinkError::Map {
                object: obj_soname.clone(),
                error,
            })
    }
}

pub struct Linker {
    search_paths: Vec<PathBuf>,
    host_fallback: bool,
    tolerate_missing_deps: bool,
}

impl Linker {
    pub fn new<I, P>(search_paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self {
            search_paths: search_paths.into_iter().map(Into::into).collect(),
            host_fallback: false,
            tolerate_missing_deps: false,
        }
    }

    pub fn with_host_fallback(mut self, enabled: bool) -> Self {
        self.host_fallback = enabled;
        self
    }

    pub fn with_tolerate_missing_deps(mut self, enabled: bool) -> Self {
        self.tolerate_missing_deps = enabled;
        self
    }

    pub fn load(&self, root_path: impl AsRef<Path>) -> Result<LoadedImageSet, LinkError> {
        let page = host_page_size();
        let root_path = root_path.as_ref().to_path_buf();

        let mut objects: Vec<LoadedObject> = Vec::new();
        let mut loaded: HashMap<String, usize> = HashMap::new();

        let mut missing_deps: Vec<MissingDep> = Vec::new();

        let mut queue: Vec<PendingLoad> = vec![PendingLoad {
            path: root_path.clone(),
            requested: None,
        }];
        let mut head = 0usize;

        while head < queue.len() {
            let pending = queue[head].clone();
            head += 1;

            if let Some(req) = &pending.requested {
                if loaded.contains_key(req) {
                    continue;
                }
            }

            let (object, needed) = self.load_one(&pending, page)?;
            let soname = object.soname.clone();

            if loaded.contains_key(&soname) {
                continue;
            }
            let idx = objects.len();
            objects.push(object);
            loaded.insert(soname.clone(), idx);
            if let Some(req) = &pending.requested {
                loaded.entry(req.clone()).or_insert(idx);
            }

            for dep in &needed {
                if loaded.contains_key(dep) {
                    continue;
                }
                let Some(dep_path) = self.locate(dep) else {
                    if self.tolerate_missing_deps {
                        if !missing_deps.iter().any(|m| m.soname == *dep) {
                            missing_deps.push(MissingDep {
                                soname: dep.clone(),
                                needed_by: soname.clone(),
                            });
                        }
                        continue;
                    }
                    return Err(LinkError::MissingDependency {
                        soname: dep.clone(),
                        needed_by: soname.clone(),
                    });
                };
                queue.push(PendingLoad {
                    path: dep_path,
                    requested: Some(dep.clone()),
                });
            }
        }

        let mut scope = Scope::new();
        for obj in &objects {
            let img = obj.image().map_err(|error| LinkError::Parse {
                object: obj.soname.clone(),
                error,
            })?;
            scope.push(Box::new(LoadedObjectProvider::new(
                obj.load_base(),
                &img.dynsyms,
            )));
        }
        if self.host_fallback {
            scope.push(Box::new(HostDlsymProvider));
        }

        let mut tls_layout = TlsLayout::new();
        let mut own_tp_offset: HashMap<usize, i64> = HashMap::new();
        for (idx, obj) in objects.iter().enumerate() {
            let img = obj.image().map_err(|error| LinkError::Parse {
                object: obj.soname.clone(),
                error,
            })?;
            if let Some(tls) = img.tls {
                let tdata_off = img
                    .vaddr_to_off(tls.vaddr)
                    .map_err(|error| LinkError::Parse {
                        object: obj.soname.clone(),
                        error,
                    })?;
                let module =
                    MappedObjectTls::add(&mut tls_layout, &tls, &obj.bytes, tdata_off as u64, &img)
                        .map_err(|error| LinkError::Map {
                            object: obj.soname.clone(),
                            error,
                        })?;
                own_tp_offset.insert(idx, module.tp_offset);
            }
        }

        let mut stats = RelocStats::default();
        let mut unresolved: Vec<UnresolvedSymbol> = Vec::new();

        for idx in (0..objects.len()).rev() {
            let obj = &mut objects[idx];
            let soname = obj.soname.clone();
            let map_stats = obj.map_stats;
            let LoadedObject {
                bytes,
                mapped,
                sym_stats: obj_sym_stats,
                tls_stats: obj_tls_stats,
                ..
            } = obj;

            let img = ElfImage::parse(bytes).map_err(|error| LinkError::Parse {
                object: soname.clone(),
                error,
            })?;

            let relas = img.relocations().map_err(|error| LinkError::DynStrings {
                object: soname.clone(),
                error,
            })?;
            stats.irelative_deferred += relas
                .iter()
                .filter(|r| r.r_type == R_X86_64_IRELATIVE)
                .count();

            let object_unresolved = enumerate_unresolved_strong(&relas, &img, &scope, &soname);

            let (sym_stats, tls_stats) = if object_unresolved.is_empty() {
                let sym_stats = mapped
                    .relocate_symbols(&img, &scope, page)
                    .map_err(|error| LinkError::Map {
                        object: soname.clone(),
                        error,
                    })?;

                let inner = ScopedResolver::new(&scope, &img.dynsyms);
                let tls_stats = mapped
                    .relocate_tls(
                        &img,
                        &inner,
                        &tls_layout,
                        own_tp_offset.get(&idx).copied(),
                        page,
                    )
                    .map_err(|error| LinkError::Map {
                        object: soname.clone(),
                        error,
                    })?;
                (sym_stats, tls_stats)
            } else {
                unresolved.extend(object_unresolved);
                (SymbolRelocStats::default(), TlsRelocStats::default())
            };

            stats.accumulate(map_stats, sym_stats, tls_stats);
            *obj_sym_stats = sym_stats;
            *obj_tls_stats = tls_stats;
        }

        let mut relro_applied = 0usize;
        for obj in &objects {
            let img = obj.image().map_err(|error| LinkError::Parse {
                object: obj.soname.clone(),
                error,
            })?;
            if let Some(relro) = img.relro {
                obj.mapped
                    .apply_relro(&relro, page)
                    .map_err(|error| LinkError::Map {
                        object: obj.soname.clone(),
                        error,
                    })?;
                relro_applied += 1;
            }
        }

        Ok(LoadedImageSet {
            objects,
            scope,
            tls_layout,
            stats,
            unresolved,
            missing_deps,
            relro_applied,
        })
    }

    fn load_one(
        &self,
        pending: &PendingLoad,
        page: u64,
    ) -> Result<(LoadedObject, Vec<String>), LinkError> {
        let bytes = std::fs::read(&pending.path).map_err(|e| LinkError::Io {
            path: pending.path.clone(),
            error: e.to_string(),
        })?;

        let ident = pending.requested.clone().unwrap_or_else(|| {
            pending
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| pending.path.display().to_string())
        });

        let img = ElfImage::parse(&bytes).map_err(|error| LinkError::Parse {
            object: ident.clone(),
            error,
        })?;

        let soname = img
            .soname()
            .map_err(|error| LinkError::DynStrings {
                object: ident.clone(),
                error,
            })?
            .or_else(|| pending.requested.clone())
            .unwrap_or_else(|| ident.clone());

        let needed = img.needed().map_err(|error| LinkError::DynStrings {
            object: soname.clone(),
            error,
        })?;

        let (mapped, map_stats) =
            MappedObject::map_and_relocate(&img, &bytes, page).map_err(|error| LinkError::Map {
                object: soname.clone(),
                error,
            })?;

        drop(img);

        Ok((
            LoadedObject {
                soname,
                path: pending.path.clone(),
                bytes,
                mapped,
                map_stats,
                sym_stats: SymbolRelocStats::default(),
                tls_stats: TlsRelocStats::default(),
            },
            needed,
        ))
    }

    fn locate(&self, soname: &str) -> Option<PathBuf> {
        if soname.contains('/') {
            let p = PathBuf::from(soname);
            if p.exists() {
                return Some(p);
            }
        }
        for dir in &self.search_paths {
            let candidate = dir.join(soname);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}

#[derive(Clone)]
struct PendingLoad {
    path: PathBuf,
    requested: Option<String>,
}

fn enumerate_unresolved_strong(
    relas: &[Rela],
    img: &ElfImage<'_>,
    scope: &Scope,
    object_soname: &str,
) -> Vec<UnresolvedSymbol> {
    let resolver = ScopedResolver::new(scope, &img.dynsyms);
    let mut out = Vec::new();
    for r in relas {
        let is_symbol_reloc = matches!(
            r.r_type,
            reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
        );
        if !is_symbol_reloc {
            continue;
        }

        if resolver.resolve_symbol(r.sym_index).is_none() {
            let name = img
                .dynsyms
                .get(r.sym_index as usize)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            out.push(UnresolvedSymbol {
                object: object_soname.to_string(),
                name,
                sym_index: r.sym_index,
            });
        }
    }
    out
}

struct MappedObjectTls;

impl MappedObjectTls {
    fn add(
        layout: &mut TlsLayout,
        tls: &super::elf::TlsSegment,
        file: &[u8],
        tdata_off: u64,
        img: &ElfImage<'_>,
    ) -> Result<super::tls::TlsModule, MapError> {
        layout
            .add_module(tls, file, tdata_off, &img.dynsyms)
            .map_err(|e| MapError::SpanOverflow(tls_err_static(&e)))
    }
}

fn tls_err_static(e: &super::tls::TlsError) -> &'static str {
    use super::tls::TlsError;
    match e {
        TlsError::BadAlign(_) => "PT_TLS p_align not a power of two",
        TlsError::FileLargerThanMem(_, _) => "PT_TLS filesz exceeds memsz",
        TlsError::Overflow(_) => "static-TLS layout arithmetic overflow",
        TlsError::TdataOutOfFile(_) => "PT_TLS .tdata past file bytes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::elf::{PF_R, PF_W, PF_X};
    use crate::loader::reloc::R_X86_64_GLOB_DAT;
    use std::io::Write;

    const PAGE: u64 = 0x1000;
    const PH_OFF: usize = 0x40;
    const DYN_OFF: u64 = 0x200;
    const RELA_OFF: u64 = 0x400;
    const SYM_OFF: u64 = 0x600;
    const STR_OFF: u64 = 0x800;
    const GLOB_TARGET: u64 = 0xc00;
    const IMG_SIZE: usize = 0x2000;

    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const EI_CLASS: usize = 4;
    const EI_DATA: usize = 5;
    const ET_DYN: u16 = 3;
    const EM_X86_64: u16 = 62;
    const EHDR_SIZE: usize = 64;
    const PHDR_SIZE: usize = 56;
    const DYN_SIZE: usize = 16;
    const SYM_SIZE: usize = 24;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_NEEDED: i64 = 1;
    const DT_RELA: i64 = 7;
    const DT_RELASZ: i64 = 8;
    const DT_RELAENT: i64 = 9;
    const DT_STRTAB: i64 = 5;
    const DT_STRSZ: i64 = 10;
    const DT_SYMTAB: i64 = 6;
    const DT_SYMENT: i64 = 11;
    const DT_SONAME: i64 = 14;
    const RELA_ENT: u64 = 24;

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[allow(clippy::too_many_arguments)]
    fn put_phdr(
        buf: &mut [u8],
        idx: usize,
        p_type: u32,
        p_flags: u32,
        p_offset: u64,
        p_vaddr: u64,
        p_filesz: u64,
        p_memsz: u64,
        p_align: u64,
    ) {
        let ph = PH_OFF + idx * PHDR_SIZE;
        put_u32(buf, ph, p_type);
        put_u32(buf, ph + 4, p_flags);
        put_u64(buf, ph + 8, p_offset);
        put_u64(buf, ph + 16, p_vaddr);
        put_u64(buf, ph + 24, p_vaddr);
        put_u64(buf, ph + 32, p_filesz);
        put_u64(buf, ph + 40, p_memsz);
        put_u64(buf, ph + 48, p_align);
    }

    fn build_so(
        soname: &str,
        needed: &[&str],
        export: Option<&str>,
        import: Option<&str>,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; IMG_SIZE];

        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        buf[6] = 1;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u32(&mut buf, 20, 1);
        put_u64(&mut buf, 32, PH_OFF as u64);
        put_u16(&mut buf, 52, EHDR_SIZE as u16);
        put_u16(&mut buf, 54, PHDR_SIZE as u16);
        put_u16(&mut buf, 56, 2);

        put_phdr(
            &mut buf,
            0,
            PT_LOAD,
            PF_R | PF_W | PF_X,
            0,
            0,
            IMG_SIZE as u64,
            IMG_SIZE as u64,
            PAGE,
        );
        put_phdr(
            &mut buf,
            1,
            PT_DYNAMIC,
            PF_R | PF_W,
            DYN_OFF,
            DYN_OFF,
            0x100,
            0x100,
            8,
        );

        let mut strtab = vec![0u8];
        let name_off = |strtab: &mut Vec<u8>, s: &str| -> u64 {
            let off = strtab.len() as u64;
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
            off
        };
        let soname_off = name_off(&mut strtab, soname);
        let needed_offs: Vec<u64> = needed.iter().map(|n| name_off(&mut strtab, n)).collect();
        let export_off = export.map(|e| name_off(&mut strtab, e));
        let import_off = import.map(|i| name_off(&mut strtab, i));
        let strsz = strtab.len() as u64;
        buf[STR_OFF as usize..STR_OFF as usize + strtab.len()].copy_from_slice(&strtab);

        let mut sym_count = 1u64;
        let mut export_index = 0u32;
        let mut import_index = 0u32;
        if let Some(eo) = export_off {
            let s = SYM_OFF as usize + (sym_count as usize) * SYM_SIZE;
            put_u32(&mut buf, s, eo as u32);
            buf[s + 4] = (1 << 4) | 2;
            put_u16(&mut buf, s + 6, 1);
            put_u64(&mut buf, s + 8, 0x1500);
            export_index = sym_count as u32;
            sym_count += 1;
        }
        if let Some(io) = import_off {
            let s = SYM_OFF as usize + (sym_count as usize) * SYM_SIZE;
            put_u32(&mut buf, s, io as u32);
            buf[s + 4] = (1 << 4) | 2;
            put_u16(&mut buf, s + 6, 0);
            import_index = sym_count as u32;
            sym_count += 1;
        }
        let _ = export_index;

        let rela_count = if import.is_some() { 1 } else { 0 };
        if import.is_some() {
            put_u64(&mut buf, RELA_OFF as usize, GLOB_TARGET);
            let r_info = ((import_index as u64) << 32) | R_X86_64_GLOB_DAT as u64;
            put_u64(&mut buf, RELA_OFF as usize + 8, r_info);
            put_u64(&mut buf, RELA_OFF as usize + 16, 0);
        }

        let mut slot = 0usize;
        let mut d = |buf: &mut [u8], tag: i64, val: u64| {
            let off = DYN_OFF as usize + slot * DYN_SIZE;
            put_u64(buf, off, tag as u64);
            put_u64(buf, off + 8, val);
            slot += 1;
        };
        d(&mut buf, DT_SONAME, soname_off);
        for no in &needed_offs {
            d(&mut buf, DT_NEEDED, *no);
        }
        d(&mut buf, DT_SYMTAB, SYM_OFF);
        d(&mut buf, DT_SYMENT, SYM_SIZE as u64);
        d(&mut buf, DT_STRTAB, STR_OFF);
        d(&mut buf, DT_STRSZ, strsz);
        if rela_count > 0 {
            d(&mut buf, DT_RELA, RELA_OFF);
            d(&mut buf, DT_RELASZ, RELA_ENT * rela_count);
            d(&mut buf, DT_RELAENT, RELA_ENT);
        }
        d(&mut buf, DT_NULL, 0);

        assert!(SYM_OFF as usize + (sym_count as usize) * SYM_SIZE <= STR_OFF as usize);
        buf
    }

    fn write_so(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create fixture .so");
        f.write_all(bytes).expect("write fixture .so");
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "eclipse-link-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn root_with_one_dep_resolves_cross_object_symbol() {
        let dir = temp_dir("crossobj");
        let root = build_so("root.so", &["dep.so"], None, Some("shared_fn"));
        let dep = build_so("dep.so", &[], Some("shared_fn"), None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "dep.so", &dep);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&root_path).expect("link succeeds");

        assert_eq!(set.objects.len(), 2, "root + dep");
        assert_eq!(set.objects[0].soname, "root.so");
        assert_eq!(set.objects[1].soname, "dep.so");
        assert!(set.unresolved.is_empty(), "no unresolved strong symbols");
        assert_eq!(set.stats.glob_dat_applied, 1);

        let dep_base = set.object("dep.so").unwrap().load_base();
        let root_obj = &set.objects[0];
        let got = read_word(root_obj, GLOB_TARGET);
        assert_eq!(got, dep_base.wrapping_add(0x1500));

        std::fs::remove_dir_all(&dir).ok();
    }

    fn read_word(obj: &LoadedObject, vaddr: u64) -> u64 {
        obj.mapped
            .read_u64(vaddr as usize)
            .expect("GOT slot is within the mapped region")
    }

    #[test]
    fn diamond_dedups_shared_dependency() {
        let dir = temp_dir("diamond");
        let a = build_so("A.so", &["B.so", "C.so"], None, None);
        let b = build_so("B.so", &["D.so"], None, None);
        let c = build_so("C.so", &["D.so"], None, None);
        let d = build_so("D.so", &[], Some("d_sym"), None);
        let a_path = write_so(&dir, "A.so", &a);
        write_so(&dir, "B.so", &b);
        write_so(&dir, "C.so", &c);
        write_so(&dir, "D.so", &d);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&a_path).expect("link succeeds");

        assert_eq!(set.objects.len(), 4, "A,B,C,D — D deduped to one");
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        assert_eq!(sonames.iter().filter(|s| **s == "D.so").count(), 1);

        assert_eq!(sonames, vec!["A.so", "B.so", "C.so", "D.so"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_dependency_is_typed_error() {
        let dir = temp_dir("missing");
        let root = build_so("root.so", &["nonexistent.so"], None, None);
        let root_path = write_so(&dir, "root.so", &root);

        let linker = Linker::new([dir.clone()]);
        match linker.load(&root_path) {
            Err(LinkError::MissingDependency { soname, needed_by }) => {
                assert_eq!(soname, "nonexistent.so");
                assert_eq!(needed_by, "root.so");
            }
            Err(other) => panic!("expected MissingDependency, got {other:?}"),
            Ok(_) => panic!("expected MissingDependency error, but load succeeded"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_order_is_deterministic_bfs() {
        let dir = temp_dir("order");
        let root = build_so("root.so", &["X.so", "Y.so"], None, None);
        let x = build_so("X.so", &["Z.so"], None, None);
        let y = build_so("Y.so", &[], None, None);
        let z = build_so("Z.so", &[], None, None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "X.so", &x);
        write_so(&dir, "Y.so", &y);
        write_so(&dir, "Z.so", &z);

        let linker = Linker::new([dir.clone()]);
        for _ in 0..5 {
            let set = linker.load(&root_path).expect("link succeeds");
            let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
            assert_eq!(sonames, vec!["root.so", "X.so", "Y.so", "Z.so"]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cycle_does_not_re_enter() {
        let dir = temp_dir("cycle");
        let p = build_so("P.so", &["Q.so"], None, None);
        let q = build_so("Q.so", &["P.so"], None, None);
        let p_path = write_so(&dir, "P.so", &p);
        write_so(&dir, "Q.so", &q);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&p_path).expect("link terminates on a cycle");
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        assert_eq!(sonames, vec!["P.so", "Q.so"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unresolved_strong_is_recorded_not_fabricated() {
        let dir = temp_dir("unresolved");
        let root = build_so("root.so", &[], None, Some("missing_sym"));
        let root_path = write_so(&dir, "root.so", &root);

        let linker = Linker::new([dir.clone()]);
        let set = linker
            .load(&root_path)
            .expect("load still succeeds; gap recorded");
        assert_eq!(set.unresolved.len(), 1);
        assert_eq!(set.unresolved[0].object, "root.so");
        assert_eq!(set.unresolved[0].name, "missing_sym");

        assert_eq!(set.stats.glob_dat_applied, 0);
        assert_eq!(read_word(&set.objects[0], GLOB_TARGET), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tolerate_missing_deps_records_instead_of_erroring() {
        let dir = temp_dir("tolerate");

        let root = build_so(
            "root.so",
            &["env_a.so", "env_b.so"],
            None,
            Some("missing_sym"),
        );
        let root_path = write_so(&dir, "root.so", &root);

        let strict = Linker::new([dir.clone()]);
        match strict.load(&root_path) {
            Err(LinkError::MissingDependency { soname, needed_by }) => {
                assert!(soname == "env_a.so" || soname == "env_b.so");
                assert_eq!(needed_by, "root.so");
            }
            Err(other) => panic!("strict mode: expected MissingDependency, got {other:?}"),
            Ok(_) => panic!("strict mode must error on a missing dep, but load succeeded"),
        }

        let tolerant = Linker::new([dir.clone()])
            .with_host_fallback(false)
            .with_tolerate_missing_deps(true);
        let set = tolerant
            .load(&root_path)
            .expect("root-only load succeeds despite absent deps");
        assert_eq!(set.objects.len(), 1, "only the root mapped");
        assert_eq!(set.objects[0].soname, "root.so");

        assert_eq!(set.missing_deps.len(), 2, "{:?}", set.missing_deps);
        let names: Vec<&str> = set.missing_deps.iter().map(|m| m.soname.as_str()).collect();
        assert!(names.contains(&"env_a.so") && names.contains(&"env_b.so"));
        for m in &set.missing_deps {
            assert_eq!(m.needed_by, "root.so");
        }

        assert_eq!(set.unresolved.len(), 1);
        assert_eq!(set.unresolved[0].name, "missing_sym");
        assert_eq!(set.stats.glob_dat_applied, 0);
        assert_eq!(read_word(&set.objects[0], GLOB_TARGET), 0);

        assert!(set.objects[0].map_stats.segments_mapped > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_unmaps_whole_graph_without_leak() {
        let dir = temp_dir("dropleak");
        let root = build_so("root.so", &["dep.so"], None, Some("shared_fn"));
        let dep = build_so("dep.so", &[], Some("shared_fn"), None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "dep.so", &dep);

        let linker = Linker::new([dir.clone()]);
        for _ in 0..128 {
            let set = linker.load(&root_path).expect("link");
            assert_eq!(set.objects.len(), 2);
            drop(set);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    const HOST_LIB_DIRS: &[&str] = &[
        "/usr/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/lib64",
        "/usr/lib64",
    ];

    fn find_libm() -> Option<PathBuf> {
        for d in HOST_LIB_DIRS {
            let p = Path::new(d).join("libm.so.6");
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    #[test]
    fn real_libm_graph_links_and_relocates() {
        let Some(libm_path) = find_libm() else {
            eprintln!("real_libm_graph_links_and_relocates: no host libm.so.6; skipping");
            return;
        };
        let search: Vec<PathBuf> = HOST_LIB_DIRS
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect();

        let linker = Linker::new(search).with_host_fallback(false);
        let set = linker
            .load(&libm_path)
            .unwrap_or_else(|e| panic!("link libm graph: {e}"));

        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        eprintln!(
            "real_libm_graph: objects={sonames:?} stats={:?} unresolved={}",
            set.stats,
            set.unresolved.len()
        );
        assert_eq!(set.objects[0].soname, "libm.so.6", "root is libm");
        assert!(
            set.object("libc.so.6").is_some(),
            "libc.so.6 loaded as a dep"
        );

        let ld_count = sonames.iter().filter(|s| s.starts_with("ld-linux")).count();
        assert_eq!(ld_count, 1, "ld-linux deduped to one object: {sonames:?}");

        for obj in &set.objects {
            assert!(obj.load_base() != 0, "{}: real base", obj.soname);
            assert!(
                obj.map_stats.segments_mapped > 0,
                "{}: segments",
                obj.soname
            );
        }

        let libm = &set.objects[0];
        let libm_unresolved: Vec<&UnresolvedSymbol> = set
            .unresolved
            .iter()
            .filter(|u| u.object == "libm.so.6")
            .collect();
        assert!(
            libm_unresolved.is_empty(),
            "libm must have no unresolved strong symbols: {libm_unresolved:?}"
        );

        let libm_image = libm.image().expect("parse mapped libm");
        let libm_relas = libm_image.relocations().expect("decode libm relocations");
        let expected_symbol_relocs = libm_relas
            .iter()
            .filter(|rela| {
                matches!(
                    rela.r_type,
                    reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
                )
            })
            .count();
        assert!(
            expected_symbol_relocs > 0,
            "host libm must exercise symbol relocation"
        );
        assert_eq!(
            libm.sym_stats.total_applied(),
            expected_symbol_relocs,
            "every libm symbol relocation applies: {:?}",
            libm.sym_stats
        );

        let expected_tpoff64 = libm_relas
            .iter()
            .filter(|rela| rela.r_type == reloc::R_X86_64_TPOFF64)
            .count();
        assert_eq!(
            libm.tls_stats.tpoff64_applied, expected_tpoff64,
            "every libm TPOFF64 applies via the multi-module TLS layout"
        );

        let errno_off = set.tls_layout.tp_offset_of("errno");
        assert!(
            errno_off.is_some_and(|v| v < 0),
            "errno tp-relative offset must be negative (variant-II): {errno_off:?}"
        );

        let expected_irelative = set
            .objects
            .iter()
            .map(|object| {
                let image = object.image().expect("parse linked object");
                image
                    .relocations()
                    .expect("decode linked object relocations")
                    .iter()
                    .filter(|rela| rela.r_type == R_X86_64_IRELATIVE)
                    .count()
            })
            .sum::<usize>();
        assert_eq!(
            set.stats.irelative_deferred, expected_irelative,
            "every graph IRELATIVE is recorded as deferred: {:?}",
            set.stats
        );

        drop(set);
    }

    fn find_roblox_apk() -> Option<PathBuf> {
        std::env::var_os("ECLIPSE_ROBLOX_APK")
            .map(PathBuf::from)
            .into_iter()
            .chain(std::env::var_os("HOME").map(|home| {
                Path::new(&home).join("eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk")
            }))
            .find(|p| p.exists())
    }

    #[test]
    fn real_libroblox_maps_base_relocates_and_honors_relro_root_only() {
        let Some(apk_path) = find_roblox_apk() else {
            eprintln!("real_libroblox_maps_...: no Roblox APK; skipping");
            return;
        };

        let mut apk = crate::apk::Apk::open(&apk_path).expect("open Roblox APK");
        let so_bytes = apk
            .read_entry("lib/x86_64/libroblox.so")
            .expect("read lib/x86_64/libroblox.so from APK");

        let dir = temp_dir("libroblox");
        let so_path = dir.join("libroblox.so");
        std::fs::write(&so_path, &so_bytes).expect("stage libroblox.so");

        let linker = Linker::new(Vec::<PathBuf>::new())
            .with_host_fallback(false)
            .with_tolerate_missing_deps(true);

        let t0 = std::time::Instant::now();
        let set = linker
            .load(&so_path)
            .unwrap_or_else(|e| panic!("root-only map+base-relocate of libroblox: {e}"));
        let elapsed = t0.elapsed();

        assert_eq!(set.objects.len(), 1, "root-only: only libroblox is mapped");
        let obj = &set.objects[0];
        assert_eq!(obj.soname, "libroblox.so");

        let img = obj.image().expect("re-parse libroblox image");
        assert_eq!(
            obj.map_stats.segments_mapped, 3,
            "libroblox has 3 PT_LOAD segments"
        );
        let base = obj.load_base();
        let span = obj.mapped.span() as u64;

        assert!(
            (0x70a_0000..=0x70c_0000).contains(&span),
            "libroblox mapped span ≈ 112.7 MiB, got {span:#x}"
        );

        for seg in &img.loads {
            if seg.mem_size > seg.file_size && seg.mem_size > 0 {
                let last = (seg.vaddr + seg.mem_size - 1) as usize;
                let last_aligned = last & !7;
                let w = obj.mapped.read_u64(last_aligned).expect("read bss tail");

                assert_eq!(
                    w, 0,
                    "bss tail of segment vaddr={:#x} must be zero",
                    seg.vaddr
                );
            }
        }

        assert_eq!(
            set.stats.relative_applied, 527_208,
            "libroblox RELATIVE relocs applied"
        );
        assert_eq!(
            obj.map_stats.relative_applied, 527_208,
            "per-object RELATIVE count matches"
        );

        let relas = img.relocations().expect("decode relocations");
        let relatives: Vec<&Rela> = relas
            .iter()
            .filter(|r| r.r_type == reloc::R_X86_64_RELATIVE)
            .collect();
        assert_eq!(relatives.len(), 527_208, "decoded RELATIVE count");
        let mut addends_in_range = 0usize;
        let mut sampled = 0usize;
        let mut sample_values_in_range = 0usize;
        for (i, r) in relatives.iter().enumerate() {
            if (r.addend as u64) < span {
                addends_in_range += 1;
            }

            if i % 64 == 0 {
                let off = r.offset as usize;
                if off + 8 <= set.objects[0].mapped.span() {
                    let v = obj.mapped.read_u64(off).expect("read relocated slot");
                    sampled += 1;
                    if (base..base + span).contains(&v) {
                        sample_values_in_range += 1;
                    }
                }
            }
        }
        assert_eq!(
            addends_in_range, 527_208,
            "every RELATIVE addend points within the object [0, span)"
        );
        assert!(
            sampled > 8_000,
            "expected a large RELATIVE sample, got {sampled}"
        );
        assert_eq!(
            sample_values_in_range, sampled,
            "every sampled relocated slot value lands in [base, base+span)"
        );

        assert!(
            img.relro.is_some(),
            "libroblox declares PT_GNU_RELRO (the doc-confirmed region)"
        );
        assert_eq!(
            set.relro_applied, 1,
            "the one PT_GNU_RELRO region was hardened read-only after relocation"
        );

        let count_type = |t: u32| relas.iter().filter(|r| r.r_type == t).count();
        let glob_dat = count_type(reloc::R_X86_64_GLOB_DAT);
        let abs64 = count_type(reloc::R_X86_64_64);
        let jump_slot = count_type(reloc::R_X86_64_JUMP_SLOT);
        assert_eq!(glob_dat, 67, "libroblox GLOB_DAT count");
        assert_eq!(abs64, 22, "libroblox R_X86_64_64 count");
        assert_eq!(jump_slot, 546, "libroblox JUMP_SLOT count");
        assert_eq!(glob_dat + abs64 + jump_slot, 635, "total symbol relocs");

        assert_eq!(
            set.stats.glob_dat_applied + set.stats.jump_slot_applied + set.stats.abs64_applied,
            0,
            "no symbol reloc applied in root-only mode (deps absent)"
        );
        assert!(
            !set.unresolved.is_empty(),
            "the symbol relocs are recorded as deferred/unresolved (not faked)"
        );
        for u in &set.unresolved {
            assert_eq!(u.object, "libroblox.so");
        }
        eprintln!(
            "libroblox deferred symbol relocs: {} recorded unresolved (of 635: {glob_dat} GLOB_DAT + {abs64} ABS64 + {jump_slot} JUMP_SLOT)",
            set.unresolved.len()
        );

        let und_imports = img
            .dynsyms
            .iter()
            .filter(|s| s.shndx == 0 && !s.name.is_empty())
            .count();

        assert!(
            und_imports >= 584,
            "libroblox UND import surface ≥ 584 (the bionic-env symbols), got {und_imports}"
        );

        assert_eq!(
            set.missing_deps.len(),
            10,
            "all 10 bionic DT_NEEDED recorded as missing (env-provided): {:?}",
            set.missing_deps
        );
        for dep in [
            "libc.so",
            "libm.so",
            "libdl.so",
            "liblog.so",
            "libandroid.so",
            "libEGL.so",
            "libGLESv2.so",
            "libOpenSLES.so",
            "libOpenMAXAL.so",
            "libmediandk.so",
        ] {
            assert!(
                set.missing_deps.iter().any(|m| m.soname == dep),
                "missing-dep surface must include {dep}: {:?}",
                set.missing_deps
            );
        }

        eprintln!(
            "real_libroblox root-only: span={span:#x} (~{} MiB) segments={} RELATIVE_applied={} (all in-range; {sampled} slots sampled) RELR_applied={} RELRO_applied={} symbol_relocs_deferred=635 unresolved_recorded={} UND_imports={und_imports} missing_deps={} reloc_wall_time={:?}",
            span / (1024 * 1024),
            obj.map_stats.segments_mapped,
            set.stats.relative_applied,
            set.stats.relr_applied,
            set.relro_applied,
            set.unresolved.len(),
            set.missing_deps.len(),
            elapsed,
        );

        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_libroblox_bionic_env_resolves_categorizes_and_partially_applies() {
        use crate::loader::bionic_env::{categorize_imports, BionicEnv};

        let Some(apk_path) = find_roblox_apk() else {
            eprintln!("real_libroblox_bionic_env_...: no Roblox APK; skipping");
            return;
        };

        let mut apk = crate::apk::Apk::open(&apk_path).expect("open Roblox APK");
        let so_bytes = apk
            .read_entry("lib/x86_64/libroblox.so")
            .expect("read lib/x86_64/libroblox.so from APK");
        let dir = temp_dir("libroblox-bionic-env");
        let so_path = dir.join("libroblox.so");
        std::fs::write(&so_path, &so_bytes).expect("stage libroblox.so");

        let linker = Linker::new(Vec::<PathBuf>::new())
            .with_host_fallback(false)
            .with_tolerate_missing_deps(true);
        let mut set = linker
            .load(&so_path)
            .unwrap_or_else(|e| panic!("root-only map+base-relocate of libroblox: {e}"));
        assert_eq!(set.objects.len(), 1, "root-only: only libroblox is mapped");
        let page = host_page_size();

        let base = set.objects[0].load_base();
        let img = set.objects[0].image().expect("re-parse libroblox");
        let dynsyms = img.dynsyms.clone();

        let all_relas = img.relocations().expect("decode libroblox relocations");
        drop(img);

        let bionic = BionicEnv::with_host_baseline(true, false);
        eprintln!(
            "BionicEnv: host_libc_present={} eclipse_natives={} missing_gl={:?}",
            bionic.host_libc_present(),
            bionic.eclipse_natives_present(),
            bionic.missing_gl()
        );

        let mut full_scope = Scope::new();
        full_scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
        for p in bionic.into_providers() {
            full_scope.push(p);
        }

        let report = categorize_imports(&all_relas, &dynsyms, &full_scope);

        eprintln!(
            "\n=== libroblox UND import categorization (total={}) ===",
            report.total
        );
        eprintln!("{:<14} {:>9} {:>11}", "category", "resolved", "unresolved");
        let mut total_resolved = 0usize;
        let mut total_unresolved = 0usize;
        for (cat, (res, unres)) in &report.category_counts {
            eprintln!("{cat:<14} {res:>9} {unres:>11}");
            total_resolved += res;
            total_unresolved += unres;
        }
        eprintln!(
            "{:<14} {:>9} {:>11}",
            "TOTAL", total_resolved, total_unresolved
        );
        eprintln!(
            "host-resolved (baseline): {} | work-list (Eclipse-native): {}",
            report.resolved_count(),
            report.unresolved_count()
        );

        eprintln!("\n--- Eclipse-bionic-native WORK-LIST (88 unresolved-strong, by category) ---");
        let worklist: std::collections::BTreeSet<&str> =
            report.host_unresolved.iter().map(String::as_str).collect();
        for (cat, names) in &report.by_category {
            let in_wl: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|n| worklist.contains(n))
                .collect();
            if !in_wl.is_empty() {
                eprintln!("[{cat}] ({}) {}", in_wl.len(), in_wl.join(", "));
            }
        }

        assert!(
            report.total >= 584,
            "libroblox UND import surface ≥ 584, got {}",
            report.total
        );

        for cat in ["ndk-android", "media-ndk", "audio", "liblog"] {
            if let Some((res, _unres)) = report.category_counts.get(cat) {
                assert_eq!(
                    *res, 0,
                    "category {cat} has no host equivalent → 0 host-resolved"
                );
            }
        }

        let stats = set
            .relocate_object_symbols_partial("libroblox.so", &full_scope, page)
            .expect("partial symbol relocation of libroblox");
        eprintln!(
            "\npartial symbol apply: applied_nonnull={} applied_weak_zero={} unresolved_strong={} deferred={} (work-list names={})",
            stats.applied_nonnull,
            stats.applied_weak_zero,
            stats.unresolved_strong,
            stats.deferred,
            stats.unresolved.len(),
        );

        let obj = &set.objects[0];
        let resolver_scope = &full_scope;

        let mut checked_nonnull = 0usize;
        for r in &all_relas {
            let is_sym = matches!(
                r.r_type,
                reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
            );
            if !is_sym {
                continue;
            }

            let name = dynsyms
                .get(r.sym_index as usize)
                .map(|s| s.name.as_str())
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let resolved = resolver_scope.resolve(name).map(|s| s.addr);
            if let Some(addr) = resolved {
                if addr != 0 {
                    let off = r.offset as usize;
                    if off + 8 <= obj.mapped.span() {
                        let slot = obj.mapped.read_u64(off).expect("read GOT slot");
                        assert_ne!(
                            slot, 0,
                            "applied GOT/PLT slot for resolved symbol {name} must be non-null"
                        );
                        checked_nonnull += 1;
                    }
                }
            }
        }
        eprintln!("verified {checked_nonnull} applied GOT/PLT slots hold a non-null host address");

        assert!(
            stats.applied_nonnull > 0,
            "the host MUST resolve a non-trivial subset (libc/m/pthread) — pipeline proof"
        );
        assert!(
            checked_nonnull > 0,
            "at least one applied slot was verified non-null"
        );

        let mut apply_worklist = stats.unresolved.clone();
        apply_worklist.sort();
        let mut cat_worklist = report.host_unresolved.clone();
        cat_worklist.sort();
        assert_eq!(
            apply_worklist, cat_worklist,
            "the partial apply's unresolved-strong set must equal the categorization work-list"
        );

        assert!(
            !cat_worklist.is_empty(),
            "the Eclipse-bionic-native work-list must be non-empty (NDK/media/audio/log)"
        );

        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_libroblox_eclipse_natives_fully_resolve_all_imports() {
        use crate::loader::bionic_env::{categorize_imports, BionicEnv};
        use crate::loader::native_provider::EclipseNativeProvider;
        use crate::loader::resolve::{HostDlsymProvider, SymbolProvider};

        let Some(apk_path) = find_roblox_apk() else {
            eprintln!("real_libroblox_eclipse_natives_resolve_liblox_libc_ndk_media_and_audio: no Roblox APK; skipping");
            return;
        };

        let mut apk = crate::apk::Apk::open(&apk_path).expect("open Roblox APK");
        let so_bytes = apk
            .read_entry("lib/x86_64/libroblox.so")
            .expect("read lib/x86_64/libroblox.so from APK");
        let dir = temp_dir("libroblox-eclipse-natives");
        let so_path = dir.join("libroblox.so");
        std::fs::write(&so_path, &so_bytes).expect("stage libroblox.so");

        let linker = Linker::new(Vec::<PathBuf>::new())
            .with_host_fallback(false)
            .with_tolerate_missing_deps(true);
        let mut set = linker
            .load(&so_path)
            .unwrap_or_else(|e| panic!("root-only map+base-relocate of libroblox: {e}"));
        let page = host_page_size();

        let base = set.objects[0].load_base();
        let img = set.objects[0].image().expect("re-parse libroblox");
        let dynsyms = img.dynsyms.clone();
        let all_relas = img.relocations().expect("decode libroblox relocations");
        drop(img);

        let mut baseline_scope = Scope::new();
        baseline_scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
        for p in BionicEnv::with_host_baseline(true, false).into_providers() {
            baseline_scope.push(p);
        }
        let baseline = categorize_imports(&all_relas, &dynsyms, &baseline_scope);

        let mut eclipse_scope = Scope::new();
        eclipse_scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
        for p in BionicEnv::with_host_baseline(true, true).into_providers() {
            eclipse_scope.push(p);
        }
        let with_eclipse = categorize_imports(&all_relas, &dynsyms, &eclipse_scope);

        eprintln!(
            "work-list: host-baseline={} | with-Eclipse-natives={}",
            baseline.unresolved_count(),
            with_eclipse.unresolved_count()
        );

        assert_eq!(
            baseline.unresolved_count(),
            88,
            "host-baseline work-list is the documented 88"
        );

        assert_eq!(
            with_eclipse.unresolved_count(),
            0,
            "Eclipse natives shrink the work-list 88 -> 0 (FULL resolution; the variadic liblog C shim closed the last 2)"
        );

        let eclipse_only = EclipseNativeProvider::with_bionic_natives();
        let host_only = HostDlsymProvider;
        let newly_resolved: std::collections::BTreeSet<&str> = baseline
            .host_unresolved
            .iter()
            .map(String::as_str)
            .filter(|n| !with_eclipse.host_unresolved.iter().any(|m| m == n))
            .collect();
        eprintln!(
            "Eclipse-native newly-resolved ({}): {:?}",
            newly_resolved.len(),
            newly_resolved
        );
        assert_eq!(
            newly_resolved.len(),
            88,
            "exactly 88 imports move from work-list to Eclipse-resolved (FULL resolution)"
        );

        for variadic in ["__android_log_print", "__android_log_assert"] {
            assert!(
                newly_resolved.contains(variadic),
                "{variadic} (variadic liblog) resolves to the Eclipse C-shim address"
            );
        }

        for ndk in [
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
        ] {
            assert!(
                newly_resolved.contains(ndk),
                "{ndk} (ndk-android) must resolve to Eclipse"
            );
        }

        for media in [
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
        ] {
            assert!(
                newly_resolved.contains(media),
                "{media} (media-ndk) must resolve to Eclipse"
            );
        }
        for audio in [
            "slCreateEngine",
            "SL_IID_ANDROIDCONFIGURATION",
            "SL_IID_ANDROIDSIMPLEBUFFERQUEUE",
            "SL_IID_BUFFERQUEUE",
            "SL_IID_ENGINE",
            "SL_IID_PLAY",
            "SL_IID_RECORD",
            "SL_IID_VOLUME",
        ] {
            assert!(
                newly_resolved.contains(audio),
                "{audio} (audio) must resolve to Eclipse"
            );
        }
        for name in &newly_resolved {
            let e = eclipse_only
                .resolve(name)
                .unwrap_or_else(|| panic!("Eclipse provider must own {name}"));
            assert!(e.addr != 0, "Eclipse native {name} address is non-null");

            let scoped = eclipse_scope
                .resolve(name)
                .unwrap_or_else(|| panic!("full Eclipse scope resolves {name}"));
            assert_eq!(
                scoped.addr, e.addr,
                "{name} must resolve to the Eclipse-native address (Eclipse tier wins over host)"
            );

            assert!(
                host_only.resolve(name).is_none(),
                "{name} is a bionic-only name the host glibc does not export"
            );
        }

        for shadowed in [
            "dl_iterate_phdr",
            "dladdr",
            "sigaltstack",
            "getaddrinfo",
            "freeaddrinfo",
            "gai_strerror",
            "getnameinfo",
        ] {
            let e = eclipse_only
                .resolve(shadowed)
                .unwrap_or_else(|| panic!("Eclipse provider must own {shadowed}"));
            let scoped = eclipse_scope
                .resolve(shadowed)
                .unwrap_or_else(|| panic!("full Eclipse scope resolves {shadowed}"));
            assert_eq!(
                scoped.addr, e.addr,
                "{shadowed} must resolve to the ECLIPSE address, never fall through to host \
                 glibc (core 1223806: the host walk is blind to Eclipse-mapped modules)"
            );
            assert!(
                host_only.resolve(shadowed).is_some(),
                "{shadowed} is host-shadowed (glibc exports it) — the pin above is load-bearing"
            );
        }

        let stats = set
            .relocate_object_symbols_partial("libroblox.so", &eclipse_scope, page)
            .expect("partial symbol relocation with Eclipse natives");
        eprintln!(
            "Eclipse-native partial apply: applied_nonnull={} applied_weak_zero={} unresolved_strong={} (work-list={})",
            stats.applied_nonnull, stats.applied_weak_zero, stats.unresolved_strong, stats.unresolved.len(),
        );

        assert_eq!(
            stats.unresolved.len(),
            0,
            "applied work-list is 0 (FULL resolution — no unresolved-strong symbols remain)"
        );
        assert_eq!(
            stats.unresolved_strong, 0,
            "no unresolved-strong symbol relocations remain"
        );

        assert_eq!(
            stats.applied_nonnull, 623,
            "FULL resolution fills 623 GOT/PLT slots (621 + the 2 variadic liblog shim slots)"
        );
        assert_eq!(stats.applied_weak_zero, 12, "12 legal weak-undef → 0");

        let obj = &set.objects[0];
        let mut checked_eclipse_slots = 0usize;
        for r in &all_relas {
            if !matches!(
                r.r_type,
                reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
            ) {
                continue;
            }
            let name = dynsyms
                .get(r.sym_index as usize)
                .map(|s| s.name.as_str())
                .unwrap_or("");
            if !newly_resolved.contains(name) {
                continue;
            }
            let expected = eclipse_only.resolve(name).expect("eclipse addr").addr;
            let off = r.offset as usize;
            if off + 8 <= obj.mapped.span() {
                let slot = obj.mapped.read_u64(off).expect("read GOT slot");

                let want = if r.r_type == reloc::R_X86_64_64 {
                    expected.wrapping_add(r.addend as u64)
                } else {
                    expected
                };
                assert_eq!(
                    slot, want,
                    "GOT slot for Eclipse native {name} must hold the Eclipse address"
                );
                checked_eclipse_slots += 1;
            }
        }
        eprintln!("verified {checked_eclipse_slots} GOT slots hold Eclipse-native addresses");
        assert!(
            checked_eclipse_slots > 0,
            "at least one Eclipse-native GOT slot was verified"
        );

        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_boot_path_loadlibrary_libs_fully_resolve() {
        use crate::loader::bionic_env::BionicEnv;

        let Some(apk_path) = find_roblox_apk() else {
            eprintln!("real_boot_path_loadlibrary_libs_fully_resolve: no Roblox APK; skipping");
            return;
        };

        let mut apk = crate::apk::Apk::open(&apk_path).expect("open Roblox APK");
        let filenames = apk.native_lib_filenames("x86_64");
        let boot_path_libs: Vec<&String> = filenames
            .iter()
            .filter(|f| {
                *f == "libbacktrace-native.so"
                    || *f == "libsurface_util_jni.so"
                    || f.starts_with("libzstd-jni")
            })
            .collect();
        assert!(
            boot_path_libs
                .iter()
                .any(|f| *f == "libbacktrace-native.so"),
            "the APK must carry libbacktrace-native.so (the lib whose loadLibrary proved fatal)"
        );

        let dir = temp_dir("boot-path-loadlibrary");
        for filename in boot_path_libs {
            let entry = format!("lib/x86_64/{filename}");
            let so_bytes = apk
                .read_entry(&entry)
                .unwrap_or_else(|e| panic!("read {entry} from APK: {e}"));
            let so_path = dir.join(filename);
            std::fs::write(&so_path, &so_bytes).expect("stage boot-path lib");

            let linker = Linker::new(Vec::<PathBuf>::new())
                .with_host_fallback(false)
                .with_tolerate_missing_deps(true);
            let mut set = linker
                .load(&so_path)
                .unwrap_or_else(|e| panic!("root-only map+base-relocate of {filename}: {e}"));
            let page = host_page_size();
            let base = set.objects[0].load_base();
            let soname = set.objects[0].soname.clone();
            let dynsyms = {
                let img = set.objects[0].image().expect("re-parse boot-path lib");
                img.dynsyms.clone()
            };
            let mut scope = Scope::new();
            scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
            for p in BionicEnv::with_host_baseline(true, true).into_providers() {
                scope.push(p);
            }
            let stats = set
                .relocate_object_symbols_partial(&soname, &scope, page)
                .unwrap_or_else(|e| panic!("partial symbol relocation of {filename}: {e}"));
            eprintln!(
                "{filename}: applied_nonnull={} weak_zero={} unresolved_strong={} ({:?})",
                stats.applied_nonnull,
                stats.applied_weak_zero,
                stats.unresolved_strong,
                stats.unresolved
            );

            let boot_global_resolvable = [
                "deflate",
                "deflateEnd",
                "deflateInit2_",
                "deflateInit_",
                "inflate",
                "inflateEnd",
                "inflateInit_",
                "zError",
            ];
            let leaked: Vec<&String> = stats
                .unresolved
                .iter()
                .filter(|n| !boot_global_resolvable.contains(&n.as_str()))
                .collect();
            assert!(
                leaked.is_empty(),
                "{filename} has unresolved strong import(s) the BOOT cannot resolve either: \
                 {leaked:?} — its System.loadLibrary would fall through to the apkenv shim \
                 linker (fatal NULL _r_debug_ptr write, core 866509). Provide the missing \
                 native(s) in the Eclipse provider tier."
            );
            drop(set);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn module_registry_enumerates_loader_mapped_and_host_modules() {
        use crate::loader::module_registry::{walk_support, ModuleRecord};

        let dir = temp_dir("phdrwalk");
        let so = build_so("phdrwalk.so", &[], Some("walk_export"), None);
        let so_path = write_so(&dir, "phdrwalk.so", &so);
        let linker = Linker::new(Vec::<PathBuf>::new());
        let set = linker.load(&so_path).expect("map the fixture");
        let obj = &set.objects[0];
        let img = obj.image().expect("re-parse the fixture");
        let rec = ModuleRecord::for_image(
            &obj.path,
            &obj.bytes,
            &img.dynsyms,
            obj.load_base(),
            obj.mapped.span() as u64,
        )
        .expect("derive the registry record");

        assert_eq!(rec.phdr_addr(), obj.load_base() + 0x40);
        assert_eq!(rec.phnum(), 2);

        let records = [rec];
        let (rc, seen) = walk_support::collect(&records);
        assert_eq!(rc, 0, "a never-stopping walk returns 0");
        let ours = seen
            .iter()
            .find(|s| s.addr == obj.load_base())
            .expect("the loader-mapped module must be enumerated");
        assert_eq!(ours.phnum, 2, "dlpi_phnum is the image's phdr count");
        assert!(
            ours.name.ends_with("phdrwalk.so"),
            "dlpi_name is the loaded path (got {})",
            ours.name
        );
        assert_eq!(
            ours.size,
            std::mem::size_of::<crate::loader::module_registry::BionicDlPhdrInfo>(),
            "the size argument versions the full bionic API-30+ struct"
        );
        assert_eq!(
            ours.first_p_type, 1,
            "dlpi_phdr points at the MAPPED phdr table (entry 0 is the fixture's PT_LOAD)"
        );
        assert!(
            seen.iter().any(|s| s.addr != obj.load_base()),
            "the host delegation must enumerate at least one host module"
        );

        let (rc, calls) = walk_support::stop_after_first(&records);
        assert_eq!(rc, 7, "the callback's nonzero rc is the walk's return");
        assert_eq!(calls, 1, "the walk stops at the first nonzero rc");

        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn self_cycle_terminates_loading_once() {
        let dir = temp_dir("selfcycle");
        let a = build_so("A.so", &["A.so"], None, None);
        let a_path = write_so(&dir, "A.so", &a);
        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&a_path).expect("self-cycle terminates");
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        assert_eq!(sonames, vec!["A.so"], "A loaded exactly once");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deep_dependency_chain_terminates_without_stack_overflow() {
        let dir = temp_dir("deepchain");
        const DEPTH: usize = 64;
        let mut root_path = None;
        for i in 0..DEPTH {
            let name = format!("L{i}.so");
            let next = format!("L{}.so", i + 1);
            let needed: Vec<&str> = if i + 1 < DEPTH {
                vec![next.as_str()]
            } else {
                vec![]
            };
            let so = build_so(&name, &needed, None, None);
            let p = write_so(&dir, &name, &so);
            if i == 0 {
                root_path = Some(p);
            }
        }
        let linker = Linker::new([dir.clone()]);
        let set = linker
            .load(root_path.as_ref().unwrap())
            .expect("deep chain links");
        assert_eq!(
            set.objects.len(),
            DEPTH,
            "every link in the chain loaded once"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_dependency_object_is_typed_parse_error() {
        let dir = temp_dir("malformeddep");
        let root = build_so("root.so", &["dep.so"], None, None);
        let root_path = write_so(&dir, "root.so", &root);

        write_so(&dir, "dep.so", &vec![0xABu8; 512]);
        let linker = Linker::new([dir.clone()]);
        match linker.load(&root_path) {
            Err(LinkError::Parse { object, .. }) => assert_eq!(object, "dep.so"),
            Err(other) => panic!("expected Parse error for the malformed dep, got {other:?}"),
            Ok(_) => panic!("a garbage dep must not link successfully"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_with_bad_segment_layout_is_typed_map_error() {
        let dir = temp_dir("badseg");
        let root = build_so("root.so", &["dep.so"], None, None);
        let root_path = write_so(&dir, "root.so", &root);

        let mut dep = build_so("dep.so", &[], None, None);

        let memsz = u64::from_le_bytes(dep[PH_OFF + 40..PH_OFF + 48].try_into().unwrap());
        put_u64(&mut dep, PH_OFF + 32, memsz + PAGE);
        write_so(&dir, "dep.so", &dep);
        let linker = Linker::new([dir.clone()]);
        match linker.load(&root_path) {
            Err(LinkError::Map { object, error }) => {
                assert_eq!(object, "dep.so");
                assert!(
                    matches!(error, MapError::FileSizeExceedsMemSize(_, _)),
                    "expected FileSizeExceedsMemSize, got {error}"
                );
            }
            Err(other) => panic!("expected Map error for the bad-layout dep, got {other:?}"),
            Ok(_) => panic!("a filesz>memsz dep must not map successfully"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
