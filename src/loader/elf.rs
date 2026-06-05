//! Pure-Rust x86-64 ELF (`ET_DYN`) decoder — the front half of Eclipse's own bionic loader.
//!
//! 2026-06-05: This module reads a 64-bit little-endian x86-64 shared object from a byte slice
//! (the mapped/loaded file bytes) and produces exactly the inputs the relocation core
//! ([`super::reloc`]) consumes: a list of [`reloc::Rela`] entries (from `.rela.dyn` + `.rela.plt`),
//! the raw `DT_RELR` `u64` table, the dynamic symbol table, the parsed [`DynInfo`], and the
//! `PT_LOAD` segment layout (for the later `mmap` step). It is the clean boundary partner of
//! `reloc.rs`: **`elf.rs` decodes the file format; `reloc.rs` applies relocations.**
//!
//! ## Clean-room provenance
//! Every structure offset, tag number, and constant below is from the **public** ELF-64 generic
//! ABI (System V gABI) and the **x86-64 psABI** — general format knowledge, no linker source was
//! read. The `Elf64_*` layouts are the standard ones (`elf.h`):
//! - `Elf64_Ehdr` = 64 bytes; `Elf64_Phdr` = 56 bytes; `Elf64_Dyn` = 16 bytes; `Elf64_Sym` = 24.
//!
//! ## Safety
//! `#![forbid(unsafe_code)]`. The input is a borrowed `&[u8]`; every multi-byte read is
//! bounds-checked into a typed [`ElfError`] (no panics, no UB), mirroring the existing
//! `apk`/`axml` byte parsers. Parsing the *bytes* of an ELF file is benign data parsing (like
//! reading a ZIP central directory) — it does not execute or map anything; `mmap`/exec is a
//! deliberately separate later step (AGENTS.md §5).
//!
//! ## Virtual-address → file-offset mapping
//! `.dynamic`, `.rela*`, `.symtab`, and `.strtab` are recorded in the dynamic section as
//! **virtual addresses** (a PIE object's image base is 0, so a vaddr is also its in-image
//! position only when a `PT_LOAD` maps it 1:1). They are not guaranteed to equal file offsets,
//! so [`ElfImage::vaddr_to_off`] walks the `PT_LOAD` table (`p_vaddr..p_vaddr+p_filesz` →
//! `p_offset`) to convert a vaddr to the file offset the decoder reads from the slice.

#![forbid(unsafe_code)]

use std::fmt;

// `reloc` is in scope for the rustdoc `[`reloc::*`]` intra-doc links; `Rela` is the applier's
// input type this decoder produces. (`#[allow(unused_imports)]`: `reloc` is used only by doc
// links, which the unused-import lint does not see — 2026-06-05.)
#[allow(unused_imports)]
use super::reloc;
use super::reloc::Rela;

// ---- ELF header constants (public System V gABI / elf.h) ---------------------------------------

/// `e_ident[EI_MAG0..4]` = `0x7f 'E' 'L' 'F'`.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `e_ident[EI_CLASS]` value `ELFCLASS64`.
const ELFCLASS64: u8 = 2;
/// `e_ident[EI_DATA]` value `ELFDATA2LSB` (little-endian).
const ELFDATA2LSB: u8 = 1;
/// `e_ident[EI_CLASS]` byte index.
const EI_CLASS: usize = 4;
/// `e_ident[EI_DATA]` byte index.
const EI_DATA: usize = 5;
/// `e_type` value `ET_DYN` (shared object / PIE).
const ET_DYN: u16 = 3;
/// `e_machine` value `EM_X86_64`.
const EM_X86_64: u16 = 62;
/// `Elf64_Ehdr` size in bytes.
const EHDR_SIZE: usize = 64;
/// `Elf64_Phdr` size in bytes (the canonical 64-bit program-header entry size).
const PHDR_SIZE: usize = 56;
/// `Elf64_Dyn` size in bytes (`d_tag` + `d_un`, two 8-byte words).
const DYN_SIZE: usize = 16;
/// `Elf64_Sym` size in bytes.
const SYM_SIZE: usize = 24;

// ---- Program-header types (`p_type`) -----------------------------------------------------------

/// `PT_LOAD`: a loadable segment.
const PT_LOAD: u32 = 1;
/// `PT_DYNAMIC`: the `.dynamic` array.
const PT_DYNAMIC: u32 = 2;
/// `PT_TLS`: the thread-local-storage template (for the later static-TLS step).
const PT_TLS: u32 = 7;
/// `PT_GNU_RELRO`: the read-only-after-relocation region.
const PT_GNU_RELRO: u32 = 0x6474_e552;

/// `p_flags` bit: segment is executable (`PF_X`).
pub const PF_X: u32 = 1;
/// `p_flags` bit: segment is writable (`PF_W`).
pub const PF_W: u32 = 2;
/// `p_flags` bit: segment is readable (`PF_R`).
pub const PF_R: u32 = 4;

// ---- Dynamic-section tags (`d_tag`) -------------------------------------------------------------

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_INIT: i64 = 12;
const DT_SONAME: i64 = 14;
const DT_BIND_NOW: i64 = 24;
const DT_INIT_ARRAY: i64 = 25;
const DT_FINI_ARRAY: i64 = 26;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_FINI_ARRAYSZ: i64 = 28;
const DT_FLAGS: i64 = 30;
const DT_PLTREL: i64 = 20;
const DT_JMPREL: i64 = 23;
/// `DT_RELR` — compressed relative relocations (table address).
const DT_RELR: i64 = 36;
/// `DT_RELRSZ` — `DT_RELR` table size in bytes.
const DT_RELRSZ: i64 = 35;
/// `DT_RELRENT` — `DT_RELR` entry size (always 8 for `Elf64`).
const DT_RELRENT: i64 = 37;
/// `DT_GNU_HASH` — the GNU-style symbol hash table.
const DT_GNU_HASH: i64 = 0x6fff_fef5;
/// `DT_FLAGS_1` — state flags (extended).
const DT_FLAGS_1: i64 = 0x6fff_fffb;

/// `DT_FLAGS` bit `DF_BIND_NOW`: process all relocations eagerly at load.
const DF_BIND_NOW: u64 = 0x8;
/// `DT_FLAGS_1` bit `DF_1_NOW`: same intent as `DF_BIND_NOW` (eager binding).
const DF_1_NOW: u64 = 0x1;

/// `r_info >> 32` — the dynamic-symbol index of an `Elf64_Rela`.
fn rela_sym(info: u64) -> u32 {
    (info >> 32) as u32
}
/// `r_info & 0xffff_ffff` — the relocation type of an `Elf64_Rela`.
fn rela_type(info: u64) -> u32 {
    (info & 0xffff_ffff) as u32
}

/// Typed ELF-decode errors. Every fallible read returns one of these rather than panicking — the
/// same total-parser discipline as the `axml` reader (AGENTS.md §2.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfError {
    /// The image is shorter than required to read `[off, off+len)`. Carries the offset and the
    /// number of bytes that were needed.
    Truncated {
        /// File offset the read started at.
        offset: usize,
        /// Number of bytes the read required.
        need: usize,
    },
    /// `e_ident` magic was not `\x7fELF`.
    BadMagic,
    /// `e_ident[EI_CLASS]` was not `ELFCLASS64`. Carries the byte found.
    NotElf64(u8),
    /// `e_ident[EI_DATA]` was not `ELFDATA2LSB`. Carries the byte found.
    NotLittleEndian(u8),
    /// `e_type` was not `ET_DYN`. Carries the value found.
    NotSharedObject(u16),
    /// `e_machine` was not `EM_X86_64`. Carries the value found.
    NotX86_64(u16),
    /// `e_phentsize` did not equal [`PHDR_SIZE`]. Carries the value found.
    BadPhEntSize(u16),
    /// `DT_RELAENT` / `DT_SYMENT` / `DT_RELRENT` did not match the fixed `Elf64` entry size.
    /// Carries `(tag, found, expected)`.
    BadEntSize(i64, u64, u64),
    /// A virtual address from the dynamic section did not fall inside any `PT_LOAD` segment, so it
    /// cannot be converted to a file offset. Carries the offending vaddr.
    UnmappedVaddr(u64),
    /// A required dynamic entry was missing (e.g. `DT_RELA` present but no `DT_SYMTAB`). Carries a
    /// short static description of what was expected.
    MissingDynamic(&'static str),
}

impl fmt::Display for ElfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { offset, need } => {
                write!(
                    f,
                    "ELF image truncated: need {need} bytes at offset {offset}"
                )
            }
            Self::BadMagic => write!(f, "not an ELF file (bad magic)"),
            Self::NotElf64(c) => write!(f, "not ELFCLASS64 (e_ident[EI_CLASS]={c})"),
            Self::NotLittleEndian(d) => write!(f, "not ELFDATA2LSB (e_ident[EI_DATA]={d})"),
            Self::NotSharedObject(t) => write!(f, "not ET_DYN (e_type={t})"),
            Self::NotX86_64(m) => write!(f, "not EM_X86_64 (e_machine={m})"),
            Self::BadPhEntSize(s) => write!(f, "unexpected e_phentsize {s} (want {PHDR_SIZE})"),
            Self::BadEntSize(tag, found, want) => {
                write!(f, "dynamic tag {tag:#x} entry size {found} (want {want})")
            }
            Self::UnmappedVaddr(v) => {
                write!(
                    f,
                    "virtual address {v:#x} is not inside any PT_LOAD segment"
                )
            }
            Self::MissingDynamic(what) => write!(f, "missing required dynamic entry: {what}"),
        }
    }
}

impl std::error::Error for ElfError {}

/// A decoded `PT_LOAD` program-header entry — the layout the later `mmap` step consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSegment {
    /// `p_offset`: byte offset of the segment's data within the file.
    pub file_offset: u64,
    /// `p_vaddr`: virtual address the segment is mapped at (relative to the load base).
    pub vaddr: u64,
    /// `p_filesz`: number of bytes present in the file for this segment.
    pub file_size: u64,
    /// `p_memsz`: number of bytes the segment occupies in memory (`>= file_size`; the tail is
    /// zero-filled `.bss`).
    pub mem_size: u64,
    /// `p_flags`: `PF_R`/`PF_W`/`PF_X` permission bits.
    pub flags: u32,
    /// `p_align`: required alignment of the segment in memory and file.
    pub align: u64,
}

/// A decoded `PT_TLS` program header — the thread-local-storage template for the static-TLS step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsSegment {
    /// `p_vaddr`: virtual address of the `.tdata` initialization image.
    pub vaddr: u64,
    /// `p_filesz`: size of the initialized `.tdata` image.
    pub file_size: u64,
    /// `p_memsz`: total TLS block size (`.tdata` + zero-filled `.tbss`).
    pub mem_size: u64,
    /// `p_align`: required TLS-block alignment.
    pub align: u64,
}

/// A decoded `PT_GNU_RELRO` program header — the region made read-only after relocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelroSegment {
    /// `p_vaddr`: start of the read-only-after-reloc region.
    pub vaddr: u64,
    /// `p_memsz`: size of the region.
    pub mem_size: u64,
}

/// The parsed `.dynamic` section, normalized to the values the loader needs. Address fields are
/// recorded as **virtual addresses** exactly as the section stores them; use
/// [`ElfImage::vaddr_to_off`] to convert to file offsets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynInfo {
    /// `DT_RELA` (vaddr) / `DT_RELASZ` (bytes): the explicit-addend relocation table `.rela.dyn`.
    pub rela: Option<(u64, u64)>,
    /// `DT_JMPREL` (vaddr) / `DT_PLTRELSZ` (bytes): the PLT relocation table `.rela.plt`.
    pub jmprel: Option<(u64, u64)>,
    /// `DT_PLTREL`: the type of the PLT relocations (`DT_RELA` = 7 for x86-64).
    pub pltrel: Option<u64>,
    /// `DT_RELR` (vaddr) / `DT_RELRSZ` (bytes): the compressed relative-relocation bitmap.
    pub relr: Option<(u64, u64)>,
    /// `DT_SYMTAB` (vaddr): the dynamic symbol table base.
    pub symtab: Option<u64>,
    /// `DT_STRTAB` (vaddr) / `DT_STRSZ` (bytes): the dynamic string table.
    pub strtab: Option<(u64, u64)>,
    /// `DT_HASH` (vaddr): the SysV symbol hash table, if present.
    pub hash: Option<u64>,
    /// `DT_GNU_HASH` (vaddr): the GNU symbol hash table, if present.
    pub gnu_hash: Option<u64>,
    /// `DT_SONAME`: the string-table offset of this object's soname (resolve via [`DynInfo::soname`]).
    pub soname_off: Option<u64>,
    /// `DT_NEEDED`: string-table offsets of the libraries this object depends on.
    pub needed_offs: Vec<u64>,
    /// `DT_INIT`: the initialization function's vaddr, if present.
    pub init: Option<u64>,
    /// `DT_INIT_ARRAY` (vaddr) / `DT_INIT_ARRAYSZ` (bytes).
    pub init_array: Option<(u64, u64)>,
    /// `DT_FINI_ARRAY` (vaddr) / `DT_FINI_ARRAYSZ` (bytes).
    pub fini_array: Option<(u64, u64)>,
    /// `DT_FLAGS` raw value (if present).
    pub flags: Option<u64>,
    /// `DT_FLAGS_1` raw value (if present).
    pub flags_1: Option<u64>,
    /// `DT_BIND_NOW` tag was present (legacy eager-binding marker).
    pub bind_now_tag: bool,
}

impl DynInfo {
    /// True if the object requests eager binding (`BIND_NOW`): the legacy `DT_BIND_NOW` tag, the
    /// `DF_BIND_NOW` bit in `DT_FLAGS`, or the `DF_1_NOW` bit in `DT_FLAGS_1`. Under this, the
    /// loader must apply `.rela.plt` at load time (which [`reloc::apply_rela`] does — see its docs).
    pub fn bind_now(&self) -> bool {
        self.bind_now_tag
            || self.flags.is_some_and(|f| f & DF_BIND_NOW != 0)
            || self.flags_1.is_some_and(|f| f & DF_1_NOW != 0)
    }
}

/// One decoded dynamic symbol (`Elf64_Sym`), with its name materialized from the string table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynSym {
    /// The symbol name, resolved from `st_name` against `DT_STRTAB` (empty string for `st_name==0`).
    pub name: String,
    /// `st_value`: for a defined symbol, its virtual address; for a TLS symbol, its offset within
    /// the module's TLS block (what [`reloc::SymbolResolver::resolve_tls_offset`] needs).
    pub value: u64,
    /// `st_size`: the symbol's size in bytes.
    pub size: u64,
    /// `st_info >> 4`: the symbol binding (`STB_LOCAL`/`STB_GLOBAL`/`STB_WEAK`).
    pub bind: u8,
    /// `st_info & 0xf`: the symbol type (`STT_OBJECT`/`STT_FUNC`/`STT_TLS`/…).
    pub sym_type: u8,
    /// `st_shndx`: the section index (`SHN_UNDEF` = 0 marks an undefined/imported symbol).
    pub shndx: u16,
}

/// The fully decoded x86-64 `ET_DYN` image. Borrows the input bytes for the lifetime of the
/// decode so name/string lookups stay zero-copy where possible.
#[derive(Debug, Clone)]
pub struct ElfImage<'a> {
    bytes: &'a [u8],
    /// All `PT_LOAD` segments, in program-header order (the `mmap` layout).
    pub loads: Vec<LoadSegment>,
    /// The `PT_DYNAMIC` segment's `(vaddr, file_size)`, if present.
    pub dynamic: Option<(u64, u64)>,
    /// The `PT_TLS` template, if present (for the static-TLS step).
    pub tls: Option<TlsSegment>,
    /// The `PT_GNU_RELRO` region, if present.
    pub relro: Option<RelroSegment>,
    /// The parsed `.dynamic` section.
    pub dyn_info: DynInfo,
    /// The decoded dynamic symbol table (parallel to relocation `sym_index`es).
    pub dynsyms: Vec<DynSym>,
}

/// Read a little-endian `u16` at `off`, bounds-checked.
fn read_u16(bytes: &[u8], off: usize) -> Result<u16, ElfError> {
    let end = off.checked_add(2).ok_or(ElfError::Truncated {
        offset: off,
        need: 2,
    })?;
    let s = bytes.get(off..end).ok_or(ElfError::Truncated {
        offset: off,
        need: 2,
    })?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

/// Read a little-endian `u32` at `off`, bounds-checked.
fn read_u32(bytes: &[u8], off: usize) -> Result<u32, ElfError> {
    let end = off.checked_add(4).ok_or(ElfError::Truncated {
        offset: off,
        need: 4,
    })?;
    let s = bytes.get(off..end).ok_or(ElfError::Truncated {
        offset: off,
        need: 4,
    })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a little-endian `u64` at `off`, bounds-checked.
fn read_u64(bytes: &[u8], off: usize) -> Result<u64, ElfError> {
    let end = off.checked_add(8).ok_or(ElfError::Truncated {
        offset: off,
        need: 8,
    })?;
    let s = bytes.get(off..end).ok_or(ElfError::Truncated {
        offset: off,
        need: 8,
    })?;
    Ok(u64::from_le_bytes(s.try_into().expect("8-byte slice")))
}

impl<'a> ElfImage<'a> {
    /// Decode the ELF header, program headers, `.dynamic`, and dynamic symbol table from `bytes`.
    ///
    /// Returns a typed [`ElfError`] for any malformed/out-of-bounds/unsupported input. Validates
    /// that the file is a little-endian 64-bit x86-64 shared object before reading anything past
    /// the identification bytes.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        Self::parse_header(bytes)?;

        let e_phoff = read_u64(bytes, 32)? as usize;
        let e_phentsize = read_u16(bytes, 54)?;
        let e_phnum = read_u16(bytes, 56)? as usize;
        if e_phentsize as usize != PHDR_SIZE {
            return Err(ElfError::BadPhEntSize(e_phentsize));
        }

        let mut loads = Vec::new();
        let mut dynamic = None;
        let mut tls = None;
        let mut relro = None;

        for i in 0..e_phnum {
            let ph = e_phoff
                .checked_add(i.checked_mul(PHDR_SIZE).ok_or(ElfError::Truncated {
                    offset: e_phoff,
                    need: PHDR_SIZE,
                })?)
                .ok_or(ElfError::Truncated {
                    offset: e_phoff,
                    need: PHDR_SIZE,
                })?;
            // Elf64_Phdr: p_type@0(u32) p_flags@4(u32) p_offset@8 p_vaddr@16 p_paddr@24
            //             p_filesz@32 p_memsz@40 p_align@48 (u64 each).
            let p_type = read_u32(bytes, ph)?;
            let p_flags = read_u32(bytes, ph + 4)?;
            let p_offset = read_u64(bytes, ph + 8)?;
            let p_vaddr = read_u64(bytes, ph + 16)?;
            let p_filesz = read_u64(bytes, ph + 32)?;
            let p_memsz = read_u64(bytes, ph + 40)?;
            let p_align = read_u64(bytes, ph + 48)?;

            match p_type {
                PT_LOAD => loads.push(LoadSegment {
                    file_offset: p_offset,
                    vaddr: p_vaddr,
                    file_size: p_filesz,
                    mem_size: p_memsz,
                    flags: p_flags,
                    align: p_align,
                }),
                PT_DYNAMIC => dynamic = Some((p_vaddr, p_filesz)),
                PT_TLS => {
                    tls = Some(TlsSegment {
                        vaddr: p_vaddr,
                        file_size: p_filesz,
                        mem_size: p_memsz,
                        align: p_align,
                    })
                }
                PT_GNU_RELRO => {
                    relro = Some(RelroSegment {
                        vaddr: p_vaddr,
                        mem_size: p_memsz,
                    })
                }
                _ => {}
            }
        }

        // Build a partially-formed image so vaddr_to_off (which only needs `loads`) can be used to
        // read the dynamic/symbol tables.
        let mut img = ElfImage {
            bytes,
            loads,
            dynamic,
            tls,
            relro,
            dyn_info: DynInfo::default(),
            dynsyms: Vec::new(),
        };

        if let Some((dyn_vaddr, dyn_size)) = img.dynamic {
            img.dyn_info = img.parse_dynamic(dyn_vaddr, dyn_size)?;
            img.dynsyms = img.parse_dynsyms()?;
        }

        Ok(img)
    }

    /// Validate `e_ident` + `e_type` + `e_machine`. Separated so a caller can cheaply reject a
    /// non-x86-64-DYN file, and so the magic/class/data checks run before any structured read.
    fn parse_header(bytes: &[u8]) -> Result<(), ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::Truncated {
                offset: 0,
                need: EHDR_SIZE,
            });
        }
        if bytes[0..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        if bytes[EI_CLASS] != ELFCLASS64 {
            return Err(ElfError::NotElf64(bytes[EI_CLASS]));
        }
        if bytes[EI_DATA] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian(bytes[EI_DATA]));
        }
        let e_type = read_u16(bytes, 16)?;
        if e_type != ET_DYN {
            return Err(ElfError::NotSharedObject(e_type));
        }
        let e_machine = read_u16(bytes, 18)?;
        if e_machine != EM_X86_64 {
            return Err(ElfError::NotX86_64(e_machine));
        }
        Ok(())
    }

    /// Convert a virtual address into a file offset by locating the `PT_LOAD` segment whose
    /// `[p_vaddr, p_vaddr + p_filesz)` range contains it. Returns [`ElfError::UnmappedVaddr`] if no
    /// loaded segment covers the address (e.g. it lies in `.bss`, which has no file bytes).
    pub fn vaddr_to_off(&self, vaddr: u64) -> Result<usize, ElfError> {
        for seg in &self.loads {
            let end = seg.vaddr.saturating_add(seg.file_size);
            if vaddr >= seg.vaddr && vaddr < end {
                let delta = vaddr - seg.vaddr;
                let off = seg
                    .file_offset
                    .checked_add(delta)
                    .ok_or(ElfError::UnmappedVaddr(vaddr))?;
                return usize::try_from(off).map_err(|_| ElfError::UnmappedVaddr(vaddr));
            }
        }
        Err(ElfError::UnmappedVaddr(vaddr))
    }

    /// Parse the `.dynamic` array (`Elf64_Dyn { d_tag: i64, d_un: u64 }` entries) into [`DynInfo`].
    /// Stops at the terminating `DT_NULL`. The section is located by its `PT_DYNAMIC` vaddr.
    fn parse_dynamic(&self, dyn_vaddr: u64, dyn_size: u64) -> Result<DynInfo, ElfError> {
        let base = self.vaddr_to_off(dyn_vaddr)?;
        let count = (dyn_size as usize) / DYN_SIZE;
        let mut info = DynInfo::default();

        for i in 0..count {
            let off = base + i * DYN_SIZE;
            let tag = read_u64(self.bytes, off)? as i64;
            let val = read_u64(self.bytes, off + 8)?;
            match tag {
                DT_NULL => break,
                DT_RELA => info.rela.get_or_insert((0, 0)).0 = val,
                DT_RELASZ => info.rela.get_or_insert((0, 0)).1 = val,
                // Entry-size tags: reject a mismatched stride, otherwise fall through to `_`
                // (the size is fixed for Elf64, so a matching value needs no storing).
                DT_RELAENT if val != reloc_ent_size() => {
                    return Err(ElfError::BadEntSize(tag, val, reloc_ent_size()));
                }
                DT_RELRENT if val != 8 => return Err(ElfError::BadEntSize(tag, val, 8)),
                DT_SYMENT if val != SYM_SIZE as u64 => {
                    return Err(ElfError::BadEntSize(tag, val, SYM_SIZE as u64));
                }
                DT_JMPREL => info.jmprel.get_or_insert((0, 0)).0 = val,
                DT_PLTRELSZ => info.jmprel.get_or_insert((0, 0)).1 = val,
                DT_PLTREL => info.pltrel = Some(val),
                DT_RELR => info.relr.get_or_insert((0, 0)).0 = val,
                DT_RELRSZ => info.relr.get_or_insert((0, 0)).1 = val,
                DT_SYMTAB => info.symtab = Some(val),
                DT_STRTAB => info.strtab.get_or_insert((0, 0)).0 = val,
                DT_STRSZ => info.strtab.get_or_insert((0, 0)).1 = val,
                DT_HASH => info.hash = Some(val),
                DT_GNU_HASH => info.gnu_hash = Some(val),
                DT_SONAME => info.soname_off = Some(val),
                DT_NEEDED => info.needed_offs.push(val),
                DT_INIT => info.init = Some(val),
                DT_INIT_ARRAY => info.init_array.get_or_insert((0, 0)).0 = val,
                DT_INIT_ARRAYSZ => info.init_array.get_or_insert((0, 0)).1 = val,
                DT_FINI_ARRAY => info.fini_array.get_or_insert((0, 0)).0 = val,
                DT_FINI_ARRAYSZ => info.fini_array.get_or_insert((0, 0)).1 = val,
                DT_FLAGS => info.flags = Some(val),
                DT_FLAGS_1 => info.flags_1 = Some(val),
                DT_BIND_NOW => info.bind_now_tag = true,
                _ => {}
            }
        }
        Ok(info)
    }

    /// Read a NUL-terminated string from `DT_STRTAB` at the given byte offset.
    ///
    /// The string table is located by its vaddr; `str_off` indexes into it. Returns an empty
    /// string for offset 0 (the conventional "no name"). An out-of-range / unterminated offset
    /// yields a typed [`ElfError`] rather than reading past the table.
    fn str_at(&self, str_off: u64) -> Result<String, ElfError> {
        let (str_vaddr, str_sz) = self
            .dyn_info
            .strtab
            .ok_or(ElfError::MissingDynamic("DT_STRTAB for a string lookup"))?;
        let base = self.vaddr_to_off(str_vaddr)?;
        let idx = usize::try_from(str_off).map_err(|_| ElfError::UnmappedVaddr(str_off))?;
        let limit = usize::try_from(str_sz).unwrap_or(usize::MAX);
        let start = base.checked_add(idx).ok_or(ElfError::Truncated {
            offset: base,
            need: idx,
        })?;
        // Scan to the NUL, but never past the declared table size or the buffer end.
        let table_end = base.saturating_add(limit).min(self.bytes.len());
        let region = self
            .bytes
            .get(start..table_end)
            .ok_or(ElfError::Truncated {
                offset: start,
                need: 1,
            })?;
        let nul = region
            .iter()
            .position(|&b| b == 0)
            .ok_or(ElfError::Truncated {
                offset: start,
                need: 1,
            })?;
        Ok(String::from_utf8_lossy(&region[..nul]).into_owned())
    }

    /// Resolve `DT_SONAME` to a string, if present.
    pub fn soname(&self) -> Result<Option<String>, ElfError> {
        match self.dyn_info.soname_off {
            Some(off) => Ok(Some(self.str_at(off)?)),
            None => Ok(None),
        }
    }

    /// Resolve the `DT_NEEDED` dependency names.
    pub fn needed(&self) -> Result<Vec<String>, ElfError> {
        self.dyn_info
            .needed_offs
            .iter()
            .map(|&off| self.str_at(off))
            .collect()
    }

    /// Parse the dynamic symbol table (`DT_SYMTAB`), materializing each name from `DT_STRTAB`.
    ///
    /// The bionic/glibc dynamic symtab is not self-describing in size; its count is conventionally
    /// derived from the hash table's chain count, which we do not parse here. We instead read
    /// symbols until `DT_STRTAB` (which immediately follows `DT_SYMTAB` in the standard layout) or
    /// the end of the symtab's containing `PT_LOAD` segment, whichever comes first — both safe,
    /// bounds-checked stopping points. This yields the full table for the resolver/`TPOFF64` use
    /// without reading hash internals.
    fn parse_dynsyms(&self) -> Result<Vec<DynSym>, ElfError> {
        let Some(sym_vaddr) = self.dyn_info.symtab else {
            // A DYN object with relocations but no symtab is malformed if .rela references symbols;
            // an object with only RELATIVE/RELR relocs legitimately may omit it.
            if self.dyn_info.rela.is_some() || self.dyn_info.jmprel.is_some() {
                return Err(ElfError::MissingDynamic("DT_SYMTAB"));
            }
            return Ok(Vec::new());
        };
        let base = self.vaddr_to_off(sym_vaddr)?;

        // Upper bound on the symtab's byte extent: prefer the end of the PT_LOAD segment that
        // contains it; the string table (which follows) further caps how far we read.
        let seg_end = self
            .loads
            .iter()
            .find(|s| sym_vaddr >= s.vaddr && sym_vaddr < s.vaddr.saturating_add(s.file_size))
            .map(|s| self.vaddr_to_off(s.vaddr).map(|o| o + s.file_size as usize))
            .transpose()?
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());

        // Cap at DT_STRTAB if it sits after the symtab in the same region (standard layout).
        let cap = match self.dyn_info.strtab {
            Some((str_vaddr, _)) if str_vaddr > sym_vaddr => {
                self.vaddr_to_off(str_vaddr)?.min(seg_end)
            }
            _ => seg_end,
        };

        let mut syms = Vec::new();
        let mut off = base;
        while off + SYM_SIZE <= cap {
            // Elf64_Sym: st_name@0(u32) st_info@4(u8) st_other@5(u8) st_shndx@6(u16)
            //            st_value@8(u64) st_size@16(u64).
            let st_name = read_u32(self.bytes, off)?;
            let st_info = self.bytes[off + 4];
            let st_shndx = read_u16(self.bytes, off + 6)?;
            let st_value = read_u64(self.bytes, off + 8)?;
            let st_size = read_u64(self.bytes, off + 16)?;
            let name = self.str_at(st_name as u64).unwrap_or_default();
            syms.push(DynSym {
                name,
                value: st_value,
                size: st_size,
                bind: st_info >> 4,
                sym_type: st_info & 0xf,
                shndx: st_shndx,
            });
            off += SYM_SIZE;
        }
        Ok(syms)
    }

    /// Decode `.rela.dyn` (`DT_RELA`) **and** `.rela.plt` (`DT_JMPREL`) into the flat list of
    /// [`reloc::Rela`] entries the applier consumes. `.rela.plt` is appended after `.rela.dyn` so a
    /// `BIND_NOW` caller applies both in one pass (see [`DynInfo::bind_now`] / [`reloc::apply_rela`]).
    pub fn relocations(&self) -> Result<Vec<Rela>, ElfError> {
        let mut out = Vec::new();
        if let Some((vaddr, size)) = self.dyn_info.rela {
            self.read_rela_table(vaddr, size, &mut out)?;
        }
        if let Some((vaddr, size)) = self.dyn_info.jmprel {
            self.read_rela_table(vaddr, size, &mut out)?;
        }
        Ok(out)
    }

    /// Read one `Elf64_Rela` table (`r_offset@0`, `r_info@8`, `r_addend@16`) at `vaddr`/`size`,
    /// appending decoded [`reloc::Rela`] entries.
    fn read_rela_table(&self, vaddr: u64, size: u64, out: &mut Vec<Rela>) -> Result<(), ElfError> {
        let base = self.vaddr_to_off(vaddr)?;
        let count = (size as usize) / reloc_ent_size() as usize;
        for i in 0..count {
            let off = base + i * reloc_ent_size() as usize;
            let r_offset = read_u64(self.bytes, off)?;
            let r_info = read_u64(self.bytes, off + 8)?;
            let r_addend = read_u64(self.bytes, off + 16)? as i64;
            out.push(Rela {
                offset: r_offset,
                sym_index: rela_sym(r_info),
                r_type: rela_type(r_info),
                addend: r_addend,
            });
        }
        Ok(())
    }

    /// Read the raw `DT_RELR` table as a slice of `u64` words (the form [`reloc::apply_relr`] takes).
    /// Returns an empty `Vec` if the object has no `DT_RELR`.
    pub fn relr(&self) -> Result<Vec<u64>, ElfError> {
        let Some((vaddr, size)) = self.dyn_info.relr else {
            return Ok(Vec::new());
        };
        let base = self.vaddr_to_off(vaddr)?;
        let count = (size as usize) / 8;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            out.push(read_u64(self.bytes, base + i * 8)?);
        }
        Ok(out)
    }
}

/// `Elf64_Rela` entry size (24 bytes). Matches the `reloc` core's expected `.rela` stride and the
/// `DT_RELAENT` value libraries record.
fn reloc_ent_size() -> u64 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::reloc::{apply_rela, SliceImage, SymbolResolver, R_X86_64_RELATIVE};

    // ---- Minimal in-memory ELF fixture builder -------------------------------------------------
    //
    // Builds a valid little-endian x86-64 ET_DYN image entirely in a Vec<u8>, with a 1:1
    // vaddr==file-offset PT_LOAD so vaddr_to_off is the identity inside it (the common PIE case).
    // Layout (all within one 0x4000-byte PT_LOAD):
    //   0x0000  Elf64_Ehdr (64 bytes)
    //   0x0040  program headers (3 × 56: PT_LOAD, PT_DYNAMIC, PT_TLS)
    //   0x0200  .dynamic
    //   0x0400  .rela.dyn (one RELATIVE entry)
    //   0x0500  .relr (one address word)
    //   0x0600  .dynsym (2 syms: null + one named)
    //   0x0700  .dynstr
    //   0x0800  the relocation target word
    //   0x0900  the RELR target word

    const PH_OFF: usize = 0x40;
    const DYN_OFF: u64 = 0x200;
    const RELA_OFF: u64 = 0x400;
    const RELR_OFF: u64 = 0x500;
    const SYM_OFF: u64 = 0x600;
    const STR_OFF: u64 = 0x700;
    const RELA_TARGET: u64 = 0x800;
    const RELR_TARGET: u64 = 0x900;
    const IMG_SIZE: usize = 0x4000;

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Write one Elf64_Phdr at program-header index `idx`.
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
        put_u64(buf, ph + 24, p_vaddr); // p_paddr = p_vaddr
        put_u64(buf, ph + 32, p_filesz);
        put_u64(buf, ph + 40, p_memsz);
        put_u64(buf, ph + 48, p_align);
    }

    /// Append a `(tag, val)` dynamic entry at dyn slot `slot`.
    fn put_dyn(buf: &mut [u8], slot: usize, tag: i64, val: u64) {
        let off = DYN_OFF as usize + slot * DYN_SIZE;
        put_u64(buf, off, tag as u64);
        put_u64(buf, off + 8, val);
    }

    /// Build the fixture image. Returns the raw bytes.
    fn build_fixture() -> Vec<u8> {
        let mut buf = vec![0u8; IMG_SIZE];

        // Elf64_Ehdr.
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        buf[6] = 1; // EI_VERSION
        put_u16(&mut buf, 16, ET_DYN); // e_type
        put_u16(&mut buf, 18, EM_X86_64); // e_machine
        put_u32(&mut buf, 20, 1); // e_version
        put_u64(&mut buf, 32, PH_OFF as u64); // e_phoff
        put_u16(&mut buf, 52, EHDR_SIZE as u16); // e_ehsize
        put_u16(&mut buf, 54, PHDR_SIZE as u16); // e_phentsize
        put_u16(&mut buf, 56, 3); // e_phnum

        // Program headers: PT_LOAD covers the whole image 1:1, then PT_DYNAMIC, then PT_TLS.
        put_phdr(
            &mut buf,
            0,
            PT_LOAD,
            PF_R | PF_W,
            0,
            0,
            IMG_SIZE as u64,
            IMG_SIZE as u64,
            0x1000,
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
        put_phdr(&mut buf, 2, PT_TLS, PF_R, 0xa00, 0xa00, 0x20, 0x40, 0x10);

        // .dynamic entries.
        let mut slot = 0;
        let dyn_entry = |buf: &mut [u8], slot: &mut usize, tag: i64, val: u64| {
            put_dyn(buf, *slot, tag, val);
            *slot += 1;
        };
        dyn_entry(&mut buf, &mut slot, DT_RELA, RELA_OFF);
        dyn_entry(&mut buf, &mut slot, DT_RELASZ, reloc_ent_size()); // one entry
        dyn_entry(&mut buf, &mut slot, DT_RELAENT, reloc_ent_size());
        dyn_entry(&mut buf, &mut slot, DT_RELR, RELR_OFF);
        dyn_entry(&mut buf, &mut slot, DT_RELRSZ, 8); // one word
        dyn_entry(&mut buf, &mut slot, DT_RELRENT, 8);
        dyn_entry(&mut buf, &mut slot, DT_SYMTAB, SYM_OFF);
        dyn_entry(&mut buf, &mut slot, DT_SYMENT, SYM_SIZE as u64);
        dyn_entry(&mut buf, &mut slot, DT_STRTAB, STR_OFF);
        dyn_entry(&mut buf, &mut slot, DT_STRSZ, 0x40);
        dyn_entry(&mut buf, &mut slot, DT_NEEDED, 1); // "libc.so.6" at strtab+1
        dyn_entry(&mut buf, &mut slot, DT_SONAME, 11); // "libfix.so" at strtab+11
        dyn_entry(&mut buf, &mut slot, DT_FLAGS, DF_BIND_NOW);
        dyn_entry(&mut buf, &mut slot, DT_NULL, 0);

        // .rela.dyn: one R_X86_64_RELATIVE at RELA_TARGET, addend 0x1234, sym 0.
        put_u64(&mut buf, RELA_OFF as usize, RELA_TARGET); // r_offset
        put_u64(&mut buf, RELA_OFF as usize + 8, R_X86_64_RELATIVE as u64); // r_info (sym 0, type 8)
        put_u64(&mut buf, RELA_OFF as usize + 16, 0x1234); // r_addend

        // .relr: one even address word naming RELR_TARGET.
        put_u64(&mut buf, RELR_OFF as usize, RELR_TARGET);

        // .dynsym: sym[0] = null; sym[1] = "sym1" (st_name=21), value 0x2000, GLOBAL FUNC, shndx 1.
        // sym[0] is all zeros (null symbol) by buffer init. "sym1" lives at strtab offset 21 below.
        let s1 = SYM_OFF as usize + SYM_SIZE;
        put_u32(&mut buf, s1, 21); // st_name → "sym1"
        buf[s1 + 4] = (1 << 4) | 2; // st_info: STB_GLOBAL(1) << 4 | STT_FUNC(2)
        put_u16(&mut buf, s1 + 6, 1); // st_shndx = 1 (defined)
        put_u64(&mut buf, s1 + 8, 0x2000); // st_value
        put_u64(&mut buf, s1 + 16, 0x40); // st_size

        // .dynstr: \0 "libc.so.6"\0 "libfix.so"\0 "sym1"\0
        let st = STR_OFF as usize;
        buf[st] = 0;
        buf[st + 1..st + 1 + 9].copy_from_slice(b"libc.so.6");
        buf[st + 10] = 0;
        buf[st + 11..st + 11 + 9].copy_from_slice(b"libfix.so");
        buf[st + 20] = 0;
        buf[st + 21..st + 21 + 4].copy_from_slice(b"sym1");
        buf[st + 25] = 0;

        // Seed the RELR target with an in-object offset so *(p) += base is observable.
        put_u64(&mut buf, RELR_TARGET as usize, 0x40);

        buf
    }

    #[test]
    fn fixture_header_fields_decode() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).expect("fixture parses");
        // PT_LOAD / PT_DYNAMIC / PT_TLS all decoded.
        assert_eq!(img.loads.len(), 1);
        assert_eq!(img.loads[0].vaddr, 0);
        assert_eq!(img.loads[0].mem_size, IMG_SIZE as u64);
        assert_eq!(img.loads[0].flags, PF_R | PF_W);
        assert_eq!(img.dynamic, Some((DYN_OFF, 0x100)));
        let tls = img.tls.expect("PT_TLS decoded");
        assert_eq!(tls.vaddr, 0xa00);
        assert_eq!(tls.file_size, 0x20);
        assert_eq!(tls.mem_size, 0x40);
        assert_eq!(tls.align, 0x10);
    }

    #[test]
    fn fixture_dynamic_fields_decode() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let d = &img.dyn_info;
        assert_eq!(d.rela, Some((RELA_OFF, reloc_ent_size())));
        assert_eq!(d.relr, Some((RELR_OFF, 8)));
        assert_eq!(d.symtab, Some(SYM_OFF));
        assert_eq!(d.strtab, Some((STR_OFF, 0x40)));
        assert_eq!(d.flags, Some(DF_BIND_NOW));
        assert!(d.bind_now(), "DF_BIND_NOW must be detected");
        assert_eq!(img.soname().unwrap().as_deref(), Some("libfix.so"));
        assert_eq!(img.needed().unwrap(), vec!["libc.so.6".to_string()]);
    }

    #[test]
    fn fixture_vaddr_to_off_is_identity_in_1to1_load() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        // With a 1:1 PT_LOAD, vaddr == file offset.
        assert_eq!(img.vaddr_to_off(DYN_OFF).unwrap(), DYN_OFF as usize);
        assert_eq!(img.vaddr_to_off(RELA_TARGET).unwrap(), RELA_TARGET as usize);
        // A vaddr beyond the load segment's file bytes is unmapped.
        assert!(matches!(
            img.vaddr_to_off(IMG_SIZE as u64 + 8),
            Err(ElfError::UnmappedVaddr(_))
        ));
    }

    #[test]
    fn fixture_dynsyms_decode_with_names() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        // sym[0] null + sym[1] "sym1"; the symtab region runs up to DT_STRTAB.
        assert!(img.dynsyms.len() >= 2);
        assert_eq!(img.dynsyms[0].name, "");
        assert_eq!(img.dynsyms[1].name, "sym1");
        assert_eq!(img.dynsyms[1].value, 0x2000);
        assert_eq!(img.dynsyms[1].bind, 1); // STB_GLOBAL
        assert_eq!(img.dynsyms[1].sym_type, 2); // STT_FUNC
        assert_eq!(img.dynsyms[1].shndx, 1);
    }

    #[test]
    fn fixture_rela_roundtrips_into_reloc_core() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let relas = img.relocations().unwrap();
        assert_eq!(relas.len(), 1);
        let r = relas[0];
        assert_eq!(r.offset, RELA_TARGET);
        assert_eq!(r.r_type, R_X86_64_RELATIVE);
        assert_eq!(r.sym_index, 0);
        assert_eq!(r.addend, 0x1234);
    }

    #[test]
    fn fixture_relr_decodes_to_words() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let words = img.relr().unwrap();
        assert_eq!(words, vec![RELR_TARGET]);
    }

    // ---- Negative / malformed-input cases (must be typed errors, never panics) ----------------

    #[test]
    fn bad_magic_is_typed_err() {
        let mut buf = build_fixture();
        buf[1] = b'X';
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::BadMagic);
    }

    #[test]
    fn wrong_class_is_typed_err() {
        let mut buf = build_fixture();
        buf[EI_CLASS] = 1; // ELFCLASS32
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::NotElf64(1));
    }

    #[test]
    fn wrong_endianness_is_typed_err() {
        let mut buf = build_fixture();
        buf[EI_DATA] = 2; // ELFDATA2MSB
        assert_eq!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::NotLittleEndian(2)
        );
    }

    #[test]
    fn wrong_machine_is_typed_err() {
        let mut buf = build_fixture();
        put_u16(&mut buf, 18, 183); // EM_AARCH64
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::NotX86_64(183));
    }

    #[test]
    fn not_dyn_is_typed_err() {
        let mut buf = build_fixture();
        put_u16(&mut buf, 16, 2); // ET_EXEC
        assert_eq!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::NotSharedObject(2)
        );
    }

    #[test]
    fn truncated_header_is_typed_err() {
        let buf = vec![0x7f, b'E', b'L', b'F', 2, 1]; // 6 bytes, far short of an Ehdr
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::Truncated {
                offset: 0,
                need: EHDR_SIZE
            }
        ));
    }

    #[test]
    fn truncated_after_header_does_not_panic() {
        // Valid header claiming program headers past EOF → must be a typed Truncated, not a panic.
        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u64(&mut buf, 32, 0x1000); // e_phoff far past EOF
        put_u16(&mut buf, 54, PHDR_SIZE as u16);
        put_u16(&mut buf, 56, 4); // 4 program headers that do not exist
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::Truncated { .. }
        ));
    }

    #[test]
    fn bad_relaent_is_typed_err() {
        let mut buf = build_fixture();
        // Corrupt DT_RELAENT (slot 2 in the fixture's dynamic) to a bad size.
        put_dyn(&mut buf, 2, DT_RELAENT, 16);
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::BadEntSize(DT_RELAENT, 16, 24)
        ));
    }

    // ---- Integration: elf.rs decode → reloc.rs apply (the two halves compose) ------------------

    /// A resolver that resolves nothing — the fixture's only relocation is RELATIVE (no symbol).
    struct NoSyms;
    impl SymbolResolver for NoSyms {
        fn resolve_symbol(&self, _i: u32) -> Option<u64> {
            None
        }
        fn resolve_tls_offset(&self, _i: u32) -> Option<u64> {
            None
        }
    }

    #[test]
    fn decode_then_apply_through_reloc_core() {
        // Decode the fixture's .rela, then apply it via reloc.rs over a SliceImage at a realistic
        // PIE base. This proves elf.rs's decoded Rela is exactly reloc.rs's Rela input — the two
        // halves compose with no glue type. 2026-06-05.
        const BASE: u64 = 0x5555_5000_0000;
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let relas = img.relocations().unwrap();

        // The image being relocated is a copy of the file bytes laid out 1:1 (the fixture's
        // PT_LOAD is identity), sized to the whole image so RELA_TARGET (0x800) is in range.
        let mut loaded = buf.clone();
        let mut slice_img = SliceImage::new(BASE, 0, &mut loaded);
        apply_rela(&mut slice_img, &NoSyms, &relas).expect("RELATIVE applies");

        // *(base + RELA_TARGET) was a RELATIVE: result = base + addend (0x1234).
        let got = u64::from_le_bytes(
            loaded[RELA_TARGET as usize..RELA_TARGET as usize + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(got, BASE + 0x1234);
    }

    // ---- Real-file test: parse an actual host .so as DATA (skips cleanly if none present) -------

    #[test]
    fn real_shared_object_decodes_sanely() {
        // 2026-06-05: parse a real host shared object's BYTES as data (benign, like the zip/axml
        // byte readers) to validate the decoder against a toolchain-produced ELF. Try several
        // common locations; SKIP (not fail) if none exist — never fabricate, never fail spuriously.
        const CANDIDATES: &[&str] = &[
            "/usr/lib/libm.so.6",
            "/usr/lib/x86_64-linux-gnu/libm.so.6",
            "/lib/x86_64-linux-gnu/libm.so.6",
        ];
        let Some(path) = CANDIDATES.iter().find(|p| std::path::Path::new(p).exists()) else {
            eprintln!(
                "real_shared_object_decodes_sanely: no host .so found in {CANDIDATES:?}; skipping"
            );
            return;
        };
        let bytes = std::fs::read(path).expect("read host .so bytes");
        let img = ElfImage::parse(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"));

        // ELFCLASS64 + EM_X86_64 are guaranteed by a successful parse (parse_header enforces them).
        assert!(!img.loads.is_empty(), "{path}: expected >=1 PT_LOAD");
        assert!(img.dynamic.is_some(), "{path}: expected a PT_DYNAMIC");
        assert!(
            !img.dynsyms.is_empty(),
            "{path}: expected a non-empty .dynsym"
        );

        // A readable DT_SONAME or at least one DT_NEEDED proves the string-table walk works.
        let soname = img.soname().expect("soname decode");
        let needed = img.needed().expect("needed decode");
        assert!(
            soname.is_some() || !needed.is_empty(),
            "{path}: expected a DT_SONAME or DT_NEEDED"
        );

        // The relocation + RELR decoders must run without error on a real object.
        let relas = img.relocations().expect("relocations decode");
        let relr = img.relr().expect("relr decode");
        eprintln!(
            "real_shared_object_decodes_sanely: {path} — loads={} dynsyms={} relas={} relr_words={} soname={:?} needed={} bind_now={}",
            img.loads.len(),
            img.dynsyms.len(),
            relas.len(),
            relr.len(),
            soname,
            needed.len(),
            img.dyn_info.bind_now(),
        );
    }

    // 2026-06-05: Re-parse the REAL Roblox x86-64 engine `libroblox.so` through this decoder,
    // reading the entry's bytes via Eclipse's own `apk` reader (benign data parse — no exec/mmap),
    // and assert the headline facts from `docs/libroblox-characterization.md`. SKIPs cleanly (never
    // fails/fabricates) when the session APK is absent, mirroring `real_shared_object_decodes_sanely`.
    // The APK path is the documented session location (AGENTS.md §5) or `ECLIPSE_ROBLOX_APK`.
    #[test]
    fn real_libroblox_engine_decodes_headline_facts() {
        let candidates: Vec<std::path::PathBuf> = std::env::var_os("ECLIPSE_ROBLOX_APK")
            .map(std::path::PathBuf::from)
            .into_iter()
            .chain(std::env::var_os("HOME").map(|home| {
                std::path::Path::new(&home)
                    .join("eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk")
            }))
            .collect();
        let Some(apk_path) = candidates.iter().find(|p| p.exists()) else {
            eprintln!(
                "real_libroblox_engine_decodes_headline_facts: no Roblox APK in {candidates:?}; skipping"
            );
            return;
        };

        let mut apk = crate::apk::Apk::open(apk_path).expect("open Roblox APK");
        let bytes = apk
            .read_entry("lib/x86_64/libroblox.so")
            .expect("read lib/x86_64/libroblox.so from APK");
        let img = ElfImage::parse(&bytes).expect("parse libroblox.so as ELF");

        // Class/machine/type: an `ElfImage::parse` success enforces ELFCLASS64 + EM_X86_64 + ET_DYN
        // (parse_header), so reaching here proves all three. Headline structural facts:
        assert!(!img.loads.is_empty(), "libroblox.so: expected PT_LOAD>0");
        assert_eq!(
            img.soname().expect("soname decode").as_deref(),
            Some("libroblox.so"),
            "libroblox.so: SONAME mismatch"
        );
        let needed = img.needed().expect("needed decode");
        assert!(
            !needed.is_empty(),
            "libroblox.so: expected a non-empty DT_NEEDED graph"
        );
        // The bionic dependency surface the env must provide (subset assert — robust to ordering).
        for dep in [
            "libc.so",
            "libm.so",
            "liblog.so",
            "libGLESv2.so",
            "libEGL.so",
        ] {
            assert!(
                needed.iter().any(|n| n == dep),
                "libroblox.so: DT_NEEDED missing {dep}"
            );
        }
        // libroblox uses BIND_NOW eager binding and has init constructors but NO PT_TLS — the facts
        // that shape the runtime tail (no %fs binding needed; eager PLT; run DT_INIT_ARRAY).
        assert!(img.dyn_info.bind_now(), "libroblox.so: expected BIND_NOW");
        assert!(img.tls.is_none(), "libroblox.so: expected NO PT_TLS");
        assert!(
            img.relro.is_some(),
            "libroblox.so: expected PT_GNU_RELRO (relro mprotect)"
        );

        // Diagnostic, documenting the APS2-packing gap (elf.rs sees only the std .rela.plt JUMP_SLOTs
        // until the Android-packed DT_ANDROID_RELA decoder lands — see the characterization doc).
        let relas = img.relocations().expect("relocations decode");
        eprintln!(
            "real_libroblox_engine_decodes_headline_facts: loads={} needed={} init_arraysz={:?} std_relocs_seen={} (packed .rela.dyn NOT yet decoded)",
            img.loads.len(),
            needed.len(),
            img.dyn_info.init_array.map(|(_, sz)| sz),
            relas.len(),
        );
    }
}
