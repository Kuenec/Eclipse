#![forbid(unsafe_code)]

use std::fmt;

#[allow(unused_imports)]
use super::reloc;
use super::reloc::Rela;

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

const PT_TLS: u32 = 7;

const PT_GNU_RELRO: u32 = 0x6474_e552;

pub const PF_X: u32 = 1;

pub const PF_W: u32 = 2;

pub const PF_R: u32 = 4;

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

const DT_RELR: i64 = 36;

const DT_RELRSZ: i64 = 35;

const DT_RELRENT: i64 = 37;

const DT_ANDROID_RELA: i64 = 0x6000_0011;

const DT_ANDROID_RELASZ: i64 = 0x6000_0012;

const DT_ANDROID_RELR: i64 = 0x6fff_e000;

const DT_ANDROID_RELRSZ: i64 = 0x6fff_e001;

const DT_ANDROID_RELRENT: i64 = 0x6fff_e003;

const APS2_MAGIC: [u8; 4] = [b'A', b'P', b'S', b'2'];

const RELOCATION_GROUPED_BY_INFO_FLAG: i64 = 1;

const RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG: i64 = 2;

const RELOCATION_GROUPED_BY_ADDEND_FLAG: i64 = 4;

const RELOCATION_GROUP_HAS_ADDEND_FLAG: i64 = 8;

const DT_GNU_HASH: i64 = 0x6fff_fef5;

const DT_FLAGS_1: i64 = 0x6fff_fffb;

const DF_BIND_NOW: u64 = 0x8;

const DF_1_NOW: u64 = 0x1;

fn rela_sym(info: u64) -> u32 {
    (info >> 32) as u32
}

fn rela_type(info: u64) -> u32 {
    (info & 0xffff_ffff) as u32
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfError {
    Truncated { offset: usize, need: usize },

    BadMagic,

    NotElf64(u8),

    NotLittleEndian(u8),

    NotSharedObject(u16),

    NotX86_64(u16),

    BadPhEntSize(u16),

    BadEntSize(i64, u64, u64),

    UnmappedVaddr(u64),

    MissingDynamic(&'static str),

    BadAndroidMagic(usize),

    BadSleb128(usize),

    BadAndroidReloc(u64, u64),
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
            Self::BadAndroidMagic(off) => {
                write!(
                    f,
                    "Android-packed relocations: bad APS2 magic at offset {off}"
                )
            }
            Self::BadSleb128(off) => {
                write!(
                    f,
                    "Android-packed relocations: malformed SLEB128 at offset {off}"
                )
            }
            Self::BadAndroidReloc(declared, produced) => {
                write!(
                    f,
                    "Android-packed relocations: declared {declared} relocs, decoded {produced}"
                )
            }
        }
    }
}

impl std::error::Error for ElfError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSegment {
    pub file_offset: u64,

    pub vaddr: u64,

    pub file_size: u64,

    pub mem_size: u64,

    pub flags: u32,

    pub align: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsSegment {
    pub vaddr: u64,

    pub file_size: u64,

    pub mem_size: u64,

    pub align: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelroSegment {
    pub vaddr: u64,

    pub mem_size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynInfo {
    pub rela: Option<(u64, u64)>,

    pub jmprel: Option<(u64, u64)>,

    pub pltrel: Option<u64>,

    pub relr: Option<(u64, u64)>,

    pub android_rela: Option<(u64, u64)>,

    pub android_relr: Option<(u64, u64)>,

    pub symtab: Option<u64>,

    pub strtab: Option<(u64, u64)>,

    pub hash: Option<u64>,

    pub gnu_hash: Option<u64>,

    pub soname_off: Option<u64>,

    pub needed_offs: Vec<u64>,

    pub init: Option<u64>,

    pub init_array: Option<(u64, u64)>,

    pub fini_array: Option<(u64, u64)>,

    pub flags: Option<u64>,

    pub flags_1: Option<u64>,

    pub bind_now_tag: bool,
}

impl DynInfo {
    pub fn bind_now(&self) -> bool {
        self.bind_now_tag
            || self.flags.is_some_and(|f| f & DF_BIND_NOW != 0)
            || self.flags_1.is_some_and(|f| f & DF_1_NOW != 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynSym {
    pub name: String,

    pub value: u64,

    pub size: u64,

    pub bind: u8,

    pub sym_type: u8,

    pub shndx: u16,
}

#[derive(Debug, Clone)]
pub struct ElfImage<'a> {
    bytes: &'a [u8],

    pub loads: Vec<LoadSegment>,

    pub dynamic: Option<(u64, u64)>,

    pub tls: Option<TlsSegment>,

    pub relro: Option<RelroSegment>,

    pub dyn_info: DynInfo,

    pub dynsyms: Vec<DynSym>,
}

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

fn read_sleb128(bytes: &[u8], cursor: &mut usize) -> Result<i64, ElfError> {
    let start = *cursor;
    let mut result: i64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*cursor).ok_or(ElfError::Truncated {
            offset: *cursor,
            need: 1,
        })?;
        *cursor += 1;

        if shift >= 64 {
            return Err(ElfError::BadSleb128(start));
        }
        result |= i64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                result |= -1i64 << shift;
            }
            return Ok(result);
        }
    }
}

impl<'a> ElfImage<'a> {
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

                DT_RELAENT if val != reloc_ent_size() => {
                    return Err(ElfError::BadEntSize(tag, val, reloc_ent_size()));
                }
                DT_RELRENT if val != 8 => return Err(ElfError::BadEntSize(tag, val, 8)),
                DT_ANDROID_RELRENT if val != 8 => return Err(ElfError::BadEntSize(tag, val, 8)),
                DT_SYMENT if val != SYM_SIZE as u64 => {
                    return Err(ElfError::BadEntSize(tag, val, SYM_SIZE as u64));
                }
                DT_JMPREL => info.jmprel.get_or_insert((0, 0)).0 = val,
                DT_PLTRELSZ => info.jmprel.get_or_insert((0, 0)).1 = val,
                DT_PLTREL => info.pltrel = Some(val),
                DT_RELR => info.relr.get_or_insert((0, 0)).0 = val,
                DT_RELRSZ => info.relr.get_or_insert((0, 0)).1 = val,
                DT_ANDROID_RELA => info.android_rela.get_or_insert((0, 0)).0 = val,
                DT_ANDROID_RELASZ => info.android_rela.get_or_insert((0, 0)).1 = val,
                DT_ANDROID_RELR => info.android_relr.get_or_insert((0, 0)).0 = val,
                DT_ANDROID_RELRSZ => info.android_relr.get_or_insert((0, 0)).1 = val,
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

    pub fn soname(&self) -> Result<Option<String>, ElfError> {
        match self.dyn_info.soname_off {
            Some(off) => Ok(Some(self.str_at(off)?)),
            None => Ok(None),
        }
    }

    pub fn needed(&self) -> Result<Vec<String>, ElfError> {
        self.dyn_info
            .needed_offs
            .iter()
            .map(|&off| self.str_at(off))
            .collect()
    }

    fn parse_dynsyms(&self) -> Result<Vec<DynSym>, ElfError> {
        let Some(sym_vaddr) = self.dyn_info.symtab else {
            if self.dyn_info.rela.is_some() || self.dyn_info.jmprel.is_some() {
                return Err(ElfError::MissingDynamic("DT_SYMTAB"));
            }
            return Ok(Vec::new());
        };
        let base = self.vaddr_to_off(sym_vaddr)?;

        let seg_end = self
            .loads
            .iter()
            .find(|s| sym_vaddr >= s.vaddr && sym_vaddr < s.vaddr.saturating_add(s.file_size))
            .map(|s| self.vaddr_to_off(s.vaddr).map(|o| o + s.file_size as usize))
            .transpose()?
            .unwrap_or(self.bytes.len())
            .min(self.bytes.len());

        let cap = match self.dyn_info.strtab {
            Some((str_vaddr, _)) if str_vaddr > sym_vaddr => {
                self.vaddr_to_off(str_vaddr)?.min(seg_end)
            }
            _ => seg_end,
        };

        let mut syms = Vec::new();
        let mut off = base;
        while off + SYM_SIZE <= cap {
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

    pub fn relocations(&self) -> Result<Vec<Rela>, ElfError> {
        let mut out = Vec::new();
        if let Some((vaddr, size)) = self.dyn_info.rela {
            self.read_rela_table(vaddr, size, &mut out)?;
        }
        if let Some((vaddr, size)) = self.dyn_info.android_rela {
            self.decode_android_packed_rela(vaddr, size, &mut out)?;
        }
        if let Some((vaddr, size)) = self.dyn_info.jmprel {
            self.read_rela_table(vaddr, size, &mut out)?;
        }
        Ok(out)
    }

    fn decode_android_packed_rela(
        &self,
        vaddr: u64,
        size: u64,
        out: &mut Vec<Rela>,
    ) -> Result<(), ElfError> {
        let base = self.vaddr_to_off(vaddr)?;

        let end = base
            .saturating_add(usize::try_from(size).unwrap_or(usize::MAX))
            .min(self.bytes.len());
        let section = self.bytes.get(base..end).ok_or(ElfError::Truncated {
            offset: base,
            need: usize::try_from(size).unwrap_or(usize::MAX),
        })?;

        if section.get(0..4) != Some(&APS2_MAGIC[..]) {
            return Err(ElfError::BadAndroidMagic(base));
        }

        let mut cur = 4usize;
        let reloc_count = read_sleb128(section, &mut cur)?;
        let reloc_count =
            u64::try_from(reloc_count).map_err(|_| ElfError::BadAndroidReloc(0, 0))?;

        let mut offset = read_sleb128(section, &mut cur)?;
        let mut addend: i64 = 0;
        let mut produced: u64 = 0;

        while produced < reloc_count {
            let group_size = read_sleb128(section, &mut cur)?;
            let group_size = u64::try_from(group_size)
                .map_err(|_| ElfError::BadAndroidReloc(reloc_count, produced))?;
            let group_flags = read_sleb128(section, &mut cur)?;

            let grouped_by_offset = group_flags & RELOCATION_GROUPED_BY_OFFSET_DELTA_FLAG != 0;
            let grouped_by_info = group_flags & RELOCATION_GROUPED_BY_INFO_FLAG != 0;
            let grouped_by_addend = group_flags & RELOCATION_GROUPED_BY_ADDEND_FLAG != 0;
            let group_has_addend = group_flags & RELOCATION_GROUP_HAS_ADDEND_FLAG != 0;

            if produced
                .checked_add(group_size)
                .is_none_or(|t| t > reloc_count)
            {
                return Err(ElfError::BadAndroidReloc(reloc_count, produced));
            }

            let group_offset_delta = if grouped_by_offset {
                read_sleb128(section, &mut cur)?
            } else {
                0
            };
            let group_info = if grouped_by_info {
                Some(read_sleb128(section, &mut cur)?)
            } else {
                None
            };
            if group_has_addend && grouped_by_addend {
                addend = addend.wrapping_add(read_sleb128(section, &mut cur)?);
            }

            for _ in 0..group_size {
                offset = if grouped_by_offset {
                    offset.wrapping_add(group_offset_delta)
                } else {
                    offset.wrapping_add(read_sleb128(section, &mut cur)?)
                };
                let info = match group_info {
                    Some(i) => i,
                    None => read_sleb128(section, &mut cur)?,
                };
                if group_has_addend {
                    if !grouped_by_addend {
                        addend = addend.wrapping_add(read_sleb128(section, &mut cur)?);
                    }
                } else {
                    addend = 0;
                }
                let info = info as u64;
                out.push(Rela {
                    offset: offset as u64,
                    sym_index: rela_sym(info),
                    r_type: rela_type(info),
                    addend,
                });
            }
            produced += group_size;
        }
        Ok(())
    }

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

    pub fn relr(&self) -> Result<Vec<u64>, ElfError> {
        let Some((vaddr, size)) = self.dyn_info.relr.or(self.dyn_info.android_relr) else {
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

fn reloc_ent_size() -> u64 {
    24
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::reloc::{apply_rela, SliceImage, SymbolResolver, R_X86_64_RELATIVE};

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

    fn put_dyn(buf: &mut [u8], slot: usize, tag: i64, val: u64) {
        let off = DYN_OFF as usize + slot * DYN_SIZE;
        put_u64(buf, off, tag as u64);
        put_u64(buf, off + 8, val);
    }

    fn build_fixture() -> Vec<u8> {
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
        put_u16(&mut buf, 56, 3);

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

        let mut slot = 0;
        let dyn_entry = |buf: &mut [u8], slot: &mut usize, tag: i64, val: u64| {
            put_dyn(buf, *slot, tag, val);
            *slot += 1;
        };
        dyn_entry(&mut buf, &mut slot, DT_RELA, RELA_OFF);
        dyn_entry(&mut buf, &mut slot, DT_RELASZ, reloc_ent_size());
        dyn_entry(&mut buf, &mut slot, DT_RELAENT, reloc_ent_size());
        dyn_entry(&mut buf, &mut slot, DT_RELR, RELR_OFF);
        dyn_entry(&mut buf, &mut slot, DT_RELRSZ, 8);
        dyn_entry(&mut buf, &mut slot, DT_RELRENT, 8);
        dyn_entry(&mut buf, &mut slot, DT_SYMTAB, SYM_OFF);
        dyn_entry(&mut buf, &mut slot, DT_SYMENT, SYM_SIZE as u64);
        dyn_entry(&mut buf, &mut slot, DT_STRTAB, STR_OFF);
        dyn_entry(&mut buf, &mut slot, DT_STRSZ, 0x40);
        dyn_entry(&mut buf, &mut slot, DT_NEEDED, 1);
        dyn_entry(&mut buf, &mut slot, DT_SONAME, 11);
        dyn_entry(&mut buf, &mut slot, DT_FLAGS, DF_BIND_NOW);
        dyn_entry(&mut buf, &mut slot, DT_NULL, 0);

        put_u64(&mut buf, RELA_OFF as usize, RELA_TARGET);
        put_u64(&mut buf, RELA_OFF as usize + 8, R_X86_64_RELATIVE as u64);
        put_u64(&mut buf, RELA_OFF as usize + 16, 0x1234);

        put_u64(&mut buf, RELR_OFF as usize, RELR_TARGET);

        let s1 = SYM_OFF as usize + SYM_SIZE;
        put_u32(&mut buf, s1, 21);
        buf[s1 + 4] = (1 << 4) | 2;
        put_u16(&mut buf, s1 + 6, 1);
        put_u64(&mut buf, s1 + 8, 0x2000);
        put_u64(&mut buf, s1 + 16, 0x40);

        let st = STR_OFF as usize;
        buf[st] = 0;
        buf[st + 1..st + 1 + 9].copy_from_slice(b"libc.so.6");
        buf[st + 10] = 0;
        buf[st + 11..st + 11 + 9].copy_from_slice(b"libfix.so");
        buf[st + 20] = 0;
        buf[st + 21..st + 21 + 4].copy_from_slice(b"sym1");
        buf[st + 25] = 0;

        put_u64(&mut buf, RELR_TARGET as usize, 0x40);

        buf
    }

    #[test]
    fn fixture_header_fields_decode() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).expect("fixture parses");

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

        assert_eq!(img.vaddr_to_off(DYN_OFF).unwrap(), DYN_OFF as usize);
        assert_eq!(img.vaddr_to_off(RELA_TARGET).unwrap(), RELA_TARGET as usize);

        assert!(matches!(
            img.vaddr_to_off(IMG_SIZE as u64 + 8),
            Err(ElfError::UnmappedVaddr(_))
        ));
    }

    #[test]
    fn fixture_dynsyms_decode_with_names() {
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();

        assert!(img.dynsyms.len() >= 2);
        assert_eq!(img.dynsyms[0].name, "");
        assert_eq!(img.dynsyms[1].name, "sym1");
        assert_eq!(img.dynsyms[1].value, 0x2000);
        assert_eq!(img.dynsyms[1].bind, 1);
        assert_eq!(img.dynsyms[1].sym_type, 2);
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

    #[test]
    fn bad_magic_is_typed_err() {
        let mut buf = build_fixture();
        buf[1] = b'X';
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::BadMagic);
    }

    #[test]
    fn wrong_class_is_typed_err() {
        let mut buf = build_fixture();
        buf[EI_CLASS] = 1;
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::NotElf64(1));
    }

    #[test]
    fn wrong_endianness_is_typed_err() {
        let mut buf = build_fixture();
        buf[EI_DATA] = 2;
        assert_eq!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::NotLittleEndian(2)
        );
    }

    #[test]
    fn wrong_machine_is_typed_err() {
        let mut buf = build_fixture();
        put_u16(&mut buf, 18, 183);
        assert_eq!(ElfImage::parse(&buf).unwrap_err(), ElfError::NotX86_64(183));
    }

    #[test]
    fn not_dyn_is_typed_err() {
        let mut buf = build_fixture();
        put_u16(&mut buf, 16, 2);
        assert_eq!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::NotSharedObject(2)
        );
    }

    #[test]
    fn truncated_header_is_typed_err() {
        let buf = vec![0x7f, b'E', b'L', b'F', 2, 1];
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
        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u64(&mut buf, 32, 0x1000);
        put_u16(&mut buf, 54, PHDR_SIZE as u16);
        put_u16(&mut buf, 56, 4);
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::Truncated { .. }
        ));
    }

    #[test]
    fn bad_relaent_is_typed_err() {
        let mut buf = build_fixture();

        put_dyn(&mut buf, 2, DT_RELAENT, 16);
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::BadEntSize(DT_RELAENT, 16, 24)
        ));
    }

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
        const BASE: u64 = 0x5555_5000_0000;
        let buf = build_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let relas = img.relocations().unwrap();

        let mut loaded = buf.clone();
        let mut slice_img = SliceImage::new(BASE, 0, &mut loaded);
        apply_rela(&mut slice_img, &NoSyms, &relas).expect("RELATIVE applies");

        let got = u64::from_le_bytes(
            loaded[RELA_TARGET as usize..RELA_TARGET as usize + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(got, BASE + 0x1234);
    }

    #[test]
    fn real_shared_object_decodes_sanely() {
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

        assert!(!img.loads.is_empty(), "{path}: expected >=1 PT_LOAD");
        assert!(img.dynamic.is_some(), "{path}: expected a PT_DYNAMIC");
        assert!(
            !img.dynsyms.is_empty(),
            "{path}: expected a non-empty .dynsym"
        );

        let soname = img.soname().expect("soname decode");
        let needed = img.needed().expect("needed decode");
        assert!(
            soname.is_some() || !needed.is_empty(),
            "{path}: expected a DT_SONAME or DT_NEEDED"
        );

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

    fn enc_sleb128(out: &mut Vec<u8>, mut value: i64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            let sign_bit = byte & 0x40 != 0;
            if (value == 0 && !sign_bit) || (value == -1 && sign_bit) {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    const GROUPED_BY_INFO: i64 = 1;
    const GROUPED_BY_OFFSET_DELTA: i64 = 2;
    const GROUPED_BY_ADDEND: i64 = 4;
    const GROUP_HAS_ADDEND: i64 = 8;

    fn build_aps2_image(aps2_stream: &[u8]) -> (Vec<u8>, u64) {
        const APS2_VADDR: u64 = 0x400;
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

        buf[APS2_VADDR as usize..APS2_VADDR as usize + aps2_stream.len()]
            .copy_from_slice(aps2_stream);

        let mut slot = 0;
        let mut dyn_entry = |buf: &mut [u8], tag: i64, val: u64| {
            put_dyn(buf, slot, tag, val);
            slot += 1;
        };
        dyn_entry(&mut buf, DT_ANDROID_RELA, APS2_VADDR);
        dyn_entry(&mut buf, DT_ANDROID_RELASZ, aps2_stream.len() as u64);
        dyn_entry(&mut buf, DT_STRTAB, STR_OFF);
        dyn_entry(&mut buf, DT_STRSZ, 0x40);
        dyn_entry(&mut buf, DT_NULL, 0);

        (buf, APS2_VADDR)
    }

    fn decode_aps2(stream: &[u8]) -> Vec<Rela> {
        let (buf, _) = build_aps2_image(stream);
        let img = ElfImage::parse(&buf).expect("aps2 fixture parses");
        img.relocations().expect("aps2 decodes")
    }

    #[test]
    fn aps2_single_relative_group() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 1);
        enc_sleb128(&mut s, 0x1000);
        enc_sleb128(&mut s, 1);
        enc_sleb128(&mut s, GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO);
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, R_X86_64_RELATIVE as i64);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 1);
        assert_eq!(
            relas[0],
            Rela {
                offset: 0x1008,
                sym_index: 0,
                r_type: R_X86_64_RELATIVE,
                addend: 0,
            }
        );
    }

    #[test]
    fn aps2_grouped_by_offset_and_info_runs_offsets() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 3);
        enc_sleb128(&mut s, 0x2000);
        enc_sleb128(&mut s, 3);
        enc_sleb128(&mut s, GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO);
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, R_X86_64_RELATIVE as i64);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 3);
        for (i, want_off) in [0x2008u64, 0x2010, 0x2018].into_iter().enumerate() {
            assert_eq!(relas[i].offset, want_off);
            assert_eq!(relas[i].r_type, R_X86_64_RELATIVE);
            assert_eq!(relas[i].sym_index, 0);
            assert_eq!(relas[i].addend, 0);
        }
    }

    #[test]
    fn aps2_group_with_addend_accumulates() {
        let info = ((5u64 << 32) | u64::from(reloc::R_X86_64_GLOB_DAT)) as i64;
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, 0);
        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, GROUPED_BY_INFO | GROUP_HAS_ADDEND);
        enc_sleb128(&mut s, info);

        enc_sleb128(&mut s, 0x10);
        enc_sleb128(&mut s, 0x100);

        enc_sleb128(&mut s, 0x10);
        enc_sleb128(&mut s, 0x40);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 2);
        assert_eq!(
            relas[0],
            Rela {
                offset: 0x10,
                sym_index: 5,
                r_type: reloc::R_X86_64_GLOB_DAT,
                addend: 0x100,
            }
        );
        assert_eq!(
            relas[1],
            Rela {
                offset: 0x20,
                sym_index: 5,
                r_type: reloc::R_X86_64_GLOB_DAT,
                addend: 0x140,
            }
        );
    }

    #[test]
    fn aps2_grouped_by_addend_reads_one_delta_for_group() {
        let info = ((9u64 << 32) | u64::from(R_X86_64_RELATIVE)) as i64;
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, 0x100);
        enc_sleb128(&mut s, 2);
        enc_sleb128(
            &mut s,
            GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO | GROUP_HAS_ADDEND | GROUPED_BY_ADDEND,
        );
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, info);
        enc_sleb128(&mut s, 0x2000);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 2);
        assert_eq!(relas[0].offset, 0x108);
        assert_eq!(relas[1].offset, 0x110);
        assert_eq!(relas[0].addend, 0x2000);
        assert_eq!(relas[1].addend, 0x2000);
        assert_eq!(relas[0].sym_index, 9);
    }

    #[test]
    fn aps2_mixed_groups_carry_offset_and_addend() {
        let glob = ((3u64 << 32) | u64::from(reloc::R_X86_64_GLOB_DAT)) as i64;
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 3);
        enc_sleb128(&mut s, 0x4000);

        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO);
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, R_X86_64_RELATIVE as i64);

        enc_sleb128(&mut s, 1);
        enc_sleb128(&mut s, GROUPED_BY_INFO | GROUP_HAS_ADDEND);
        enc_sleb128(&mut s, glob);
        enc_sleb128(&mut s, 0x20);
        enc_sleb128(&mut s, 0x77);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 3);

        assert_eq!(relas[0].offset, 0x4008);
        assert_eq!(relas[0].r_type, R_X86_64_RELATIVE);
        assert_eq!(relas[0].addend, 0);
        assert_eq!(relas[1].offset, 0x4010);
        assert_eq!(relas[1].addend, 0);

        assert_eq!(relas[2].offset, 0x4030);
        assert_eq!(relas[2].r_type, reloc::R_X86_64_GLOB_DAT);
        assert_eq!(relas[2].sym_index, 3);
        assert_eq!(relas[2].addend, 0x77);
    }

    #[test]
    fn aps2_per_reloc_info_not_grouped() {
        let r0 = R_X86_64_RELATIVE as i64;
        let r1 = ((7u64 << 32) | u64::from(R_X86_64_RELATIVE)) as i64;
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, 0);
        enc_sleb128(&mut s, 2);
        enc_sleb128(&mut s, GROUPED_BY_OFFSET_DELTA);
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, r0);
        enc_sleb128(&mut s, r1);

        let relas = decode_aps2(&s);
        assert_eq!(relas.len(), 2);
        assert_eq!(relas[0].sym_index, 0);
        assert_eq!(relas[1].sym_index, 7);
        assert_eq!(relas[0].offset, 0x8);
        assert_eq!(relas[1].offset, 0x10);
    }

    #[test]
    fn aps2_truncated_stream_is_typed_err() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 5);
        enc_sleb128(&mut s, 0);

        let (buf, _) = build_aps2_image(&s);
        let img = ElfImage::parse(&buf).unwrap();
        let err = img.relocations().unwrap_err();
        assert!(
            matches!(err, ElfError::Truncated { .. }),
            "expected Truncated, got {err:?}"
        );
    }

    #[test]
    fn aps2_bad_magic_is_typed_err() {
        let mut s = vec![b'A', b'P', b'S', b'1'];
        enc_sleb128(&mut s, 1);
        enc_sleb128(&mut s, 0);
        let (buf, _) = build_aps2_image(&s);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            img.relocations().unwrap_err(),
            ElfError::BadAndroidMagic(_)
        ));
    }

    #[test]
    fn aps2_overshooting_group_is_typed_err() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, 1);
        enc_sleb128(&mut s, 0);
        enc_sleb128(&mut s, 4);
        enc_sleb128(&mut s, GROUPED_BY_OFFSET_DELTA | GROUPED_BY_INFO);
        enc_sleb128(&mut s, 0x8);
        enc_sleb128(&mut s, R_X86_64_RELATIVE as i64);
        let (buf, _) = build_aps2_image(&s);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            img.relocations().unwrap_err(),
            ElfError::BadAndroidReloc(1, 0)
        ));
    }

    #[test]
    fn read_sleb128_negative_and_multibyte() {
        for v in [
            0i64,
            1,
            -1,
            63,
            64,
            -64,
            -65,
            0x7f,
            -0x80,
            12345,
            -12345,
            i64::MIN,
            i64::MAX,
        ] {
            let mut enc = Vec::new();
            enc_sleb128(&mut enc, v);
            let mut cur = 0usize;
            let got = read_sleb128(&enc, &mut cur).expect("decode");
            assert_eq!(got, v, "round-trip {v}");
            assert_eq!(cur, enc.len(), "consumed all bytes for {v}");
        }
    }

    #[test]
    fn read_sleb128_truncated_is_typed_err() {
        let bytes = [0x80u8];
        let mut cur = 0usize;
        assert!(matches!(
            read_sleb128(&bytes, &mut cur),
            Err(ElfError::Truncated { .. })
        ));
    }

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

        assert!(img.dyn_info.bind_now(), "libroblox.so: expected BIND_NOW");
        assert!(img.tls.is_none(), "libroblox.so: expected NO PT_TLS");
        assert!(
            img.relro.is_some(),
            "libroblox.so: expected PT_GNU_RELRO (relro mprotect)"
        );

        assert!(
            img.dyn_info.android_rela.is_some(),
            "libroblox.so: expected DT_ANDROID_RELA (APS2-packed .rela.dyn)"
        );
        assert!(
            img.dyn_info.rela.is_none(),
            "libroblox.so: expected NO standard DT_RELA (packing is APS2-only)"
        );

        let (av, asz) = img.dyn_info.android_rela.unwrap();
        let mut packed = Vec::new();
        img.decode_android_packed_rela(av, asz, &mut packed)
            .expect("APS2 decode");
        assert_eq!(
            packed.len(),
            527_297,
            "libroblox.so: APS2 decoded reloc count"
        );
        let count_type = |t: u32| packed.iter().filter(|r| r.r_type == t).count();
        assert_eq!(
            count_type(R_X86_64_RELATIVE),
            527_208,
            "libroblox.so: APS2 RELATIVE count"
        );
        assert_eq!(
            count_type(reloc::R_X86_64_GLOB_DAT),
            67,
            "libroblox.so: APS2 GLOB_DAT count"
        );
        assert_eq!(
            count_type(reloc::R_X86_64_64),
            22,
            "libroblox.so: APS2 R_X86_64_64 count"
        );

        let relas = img.relocations().expect("relocations decode");
        assert_eq!(
            relas.len(),
            527_843,
            "libroblox.so: total relocations (APS2 + .rela.plt)"
        );
        assert_eq!(
            relas
                .iter()
                .filter(|r| r.r_type == reloc::R_X86_64_JUMP_SLOT)
                .count(),
            546,
            "libroblox.so: std .rela.plt JUMP_SLOT count"
        );
        eprintln!(
            "real_libroblox_engine_decodes_headline_facts: loads={} needed={} init_arraysz={:?} APS2_decoded={} (RELATIVE 527208 + GLOB_DAT 67 + 64×22) + std_relocs={} → total {}",
            img.loads.len(),
            needed.len(),
            img.dyn_info.init_array.map(|(_, sz)| sz),
            packed.len(),
            relas.len() - packed.len(),
            relas.len(),
        );
    }

    fn header_only(e_phoff: u64, e_phnum: u16, e_phentsize: u16) -> Vec<u8> {
        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u64(&mut buf, 32, e_phoff);
        put_u16(&mut buf, 54, e_phentsize);
        put_u16(&mut buf, 56, e_phnum);
        buf
    }

    #[test]
    fn truncated_mid_phdr_is_typed_err_not_panic() {
        let mut buf = header_only(PH_OFF as u64, 2, PHDR_SIZE as u16);
        buf.resize(PH_OFF + PHDR_SIZE + PHDR_SIZE / 2, 0);
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::Truncated { .. }
        ));
    }

    #[test]
    fn phnum_times_entsize_overflow_is_typed_err() {
        let buf = header_only(u64::MAX - 10, u16::MAX, PHDR_SIZE as u16);

        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::Truncated { .. }
        ));
    }

    #[test]
    fn bad_phentsize_is_typed_err() {
        let mut buf = header_only(PH_OFF as u64, 1, 40);
        buf.resize(PH_OFF + 64, 0);
        assert!(matches!(
            ElfImage::parse(&buf).unwrap_err(),
            ElfError::BadPhEntSize(40)
        ));
    }

    #[test]
    fn dynamic_strtab_vaddr_past_eof_is_typed_err() {
        let mut buf = build_fixture();
        put_dyn(&mut buf, 8, DT_STRTAB, 0xffff_0000);

        let err = ElfImage::parse(&buf).unwrap_err();
        assert!(
            matches!(err, ElfError::UnmappedVaddr(_) | ElfError::Truncated { .. }),
            "expected UnmappedVaddr/Truncated, got {err:?}"
        );
    }

    #[test]
    fn dynamic_symtab_vaddr_past_eof_is_typed_err() {
        let mut buf = build_fixture();
        put_dyn(&mut buf, 6, DT_SYMTAB, 0xffff_0000);
        let err = ElfImage::parse(&buf).unwrap_err();
        assert!(
            matches!(err, ElfError::UnmappedVaddr(_) | ElfError::Truncated { .. }),
            "expected UnmappedVaddr/Truncated, got {err:?}"
        );
    }

    #[test]
    fn absurd_strsz_does_not_over_read_or_panic() {
        let mut buf = build_fixture();
        put_dyn(&mut buf, 9, DT_STRSZ, u64::MAX);

        let _ = ElfImage::parse(&buf);
    }

    #[test]
    fn unterminated_string_is_typed_err() {
        let mut buf = build_fixture();
        for b in buf
            .iter_mut()
            .skip(STR_OFF as usize)
            .take(IMG_SIZE - STR_OFF as usize)
        {
            *b = b'X';
        }
        let img = ElfImage::parse(&buf).expect("parse tolerates corrupt symbol names");
        let err = img.soname().unwrap_err();
        assert!(
            matches!(err, ElfError::Truncated { .. }),
            "expected Truncated (no NUL in strtab), got {err:?}"
        );
    }

    #[test]
    fn needed_offset_past_strtab_is_typed_err() {
        let mut buf = build_fixture();
        put_dyn(&mut buf, 10, DT_NEEDED, 0x10_000);
        let img = ElfImage::parse(&buf).expect("header/dynamic still parse (names read lazily)");
        let err = img.needed().unwrap_err();
        assert!(
            matches!(err, ElfError::Truncated { .. } | ElfError::UnmappedVaddr(_)),
            "expected Truncated/UnmappedVaddr, got {err:?}"
        );
    }

    #[test]
    fn rela_present_without_symtab_is_typed_err() {
        let mut buf = build_fixture();
        put_dyn(&mut buf, 6, DT_NULL, 0);

        let mut buf2 = build_fixture();

        put_u64(&mut buf2, DYN_OFF as usize + 6 * DYN_SIZE, 0x6fff_fffe_u64);
        let err = ElfImage::parse(&buf2).unwrap_err();
        assert!(
            matches!(err, ElfError::MissingDynamic("DT_SYMTAB")),
            "expected MissingDynamic(DT_SYMTAB), got {err:?}"
        );

        let _ = ElfImage::parse(&buf);
    }

    #[test]
    fn read_sleb128_running_past_64_bits_is_bad_sleb128() {
        let bytes = [0x80u8; 11];
        let mut cur = 0usize;
        assert!(matches!(
            read_sleb128(&bytes, &mut cur),
            Err(ElfError::BadSleb128(0))
        ));
    }

    #[test]
    fn aps2_reloc_count_u64_max_does_not_preallocate_or_panic() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, i64::MAX);
        enc_sleb128(&mut s, 0);

        let (buf, _) = build_aps2_image(&s);
        let img = ElfImage::parse(&buf).unwrap();
        let err = img.relocations().unwrap_err();
        assert!(
            matches!(
                err,
                ElfError::Truncated { .. } | ElfError::BadAndroidReloc(_, _)
            ),
            "expected Truncated/BadAndroidReloc, got {err:?}"
        );
    }

    #[test]
    fn aps2_negative_reloc_count_is_typed_err() {
        let mut s = Vec::new();
        s.extend_from_slice(&APS2_MAGIC);
        enc_sleb128(&mut s, -1);
        enc_sleb128(&mut s, 0);
        let (buf, _) = build_aps2_image(&s);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            img.relocations().unwrap_err(),
            ElfError::BadAndroidReloc(0, 0)
        ));
    }

    #[test]
    fn vaddr_to_off_overlapping_unordered_loads_resolve_by_containment() {
        let mut buf = vec![0u8; IMG_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u64(&mut buf, 32, PH_OFF as u64);
        put_u16(&mut buf, 54, PHDR_SIZE as u16);
        put_u16(&mut buf, 56, 2);

        put_phdr(
            &mut buf, 0, PT_LOAD, PF_R, 0x2000, 0x2000, 0x1000, 0x1000, 0x1000,
        );
        put_phdr(&mut buf, 1, PT_LOAD, PF_R, 0x0, 0x0, 0x1000, 0x1000, 0x1000);
        let img = ElfImage::parse(&buf).expect("no dynamic → header/phdr-only parse");

        assert_eq!(img.vaddr_to_off(0x10).unwrap(), 0x10);
        assert_eq!(img.vaddr_to_off(0x2010).unwrap(), 0x2010);
        assert!(matches!(
            img.vaddr_to_off(0x5000),
            Err(ElfError::UnmappedVaddr(0x5000))
        ));
    }
}
