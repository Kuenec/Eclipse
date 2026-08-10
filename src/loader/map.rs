use std::ptr::NonNull;

use rustix::mm::{
    madvise, mmap_anonymous, mprotect, munmap, Advice, MapFlags, MprotectFlags, ProtFlags,
};

use super::elf::{ElfImage, LoadSegment, RelroSegment, PF_R, PF_W, PF_X};
use super::reloc::{self, RelocError, RelocImage, SymbolResolver};
use super::resolve::{Scope, ScopedResolver};
use super::tls::{TlsLayout, TlsResolver};

#[derive(Debug)]
pub enum MapError {
    NoLoadSegments,

    SpanOverflow(&'static str),

    SegmentOutOfFile(u64),

    FileSizeExceedsMemSize(u64, u64),

    Os(rustix::io::Errno),

    Reloc(RelocError),
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLoadSegments => write!(f, "ELF image has no PT_LOAD segments to map"),
            Self::SpanOverflow(what) => write!(f, "PT_LOAD layout arithmetic overflowed: {what}"),
            Self::SegmentOutOfFile(off) => {
                write!(
                    f,
                    "PT_LOAD at file offset {off:#x} extends past the file bytes"
                )
            }
            Self::FileSizeExceedsMemSize(fz, mz) => {
                write!(f, "PT_LOAD p_filesz {fz:#x} exceeds p_memsz {mz:#x}")
            }
            Self::Os(e) => write!(f, "memory-mapping syscall failed: {e}"),
            Self::Reloc(e) => write!(f, "base relocation failed: {e}"),
        }
    }
}

impl std::error::Error for MapError {}

impl From<RelocError> for MapError {
    fn from(e: RelocError) -> Self {
        Self::Reloc(e)
    }
}

const R_X86_64_IRELATIVE: u32 = 37;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MapStats {
    pub segments_mapped: usize,

    pub relative_applied: usize,

    pub relr_applied: usize,

    pub skipped_by_type: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SymbolRelocStats {
    pub glob_dat_applied: usize,

    pub jump_slot_applied: usize,

    pub abs64_applied: usize,

    pub resolved_nonnull: usize,

    pub deferred: usize,
}

impl SymbolRelocStats {
    pub fn total_applied(&self) -> usize {
        self.glob_dat_applied + self.jump_slot_applied + self.abs64_applied
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TlsRelocStats {
    pub tpoff64_applied: usize,

    pub deferred: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialSymbolStats {
    pub applied_nonnull: usize,

    pub applied_weak_zero: usize,

    pub unresolved_strong: usize,

    pub deferred: usize,

    pub unresolved: Vec<String>,
}

impl PartialSymbolStats {
    pub fn total_applied(&self) -> usize {
        self.applied_nonnull + self.applied_weak_zero
    }
}

fn page_floor(addr: u64, page: u64) -> u64 {
    addr & !(page - 1)
}

fn page_ceil(addr: u64, page: u64) -> Option<u64> {
    addr.checked_add(page - 1).map(|a| a & !(page - 1))
}

fn prot_of(flags: u32) -> ProtFlags {
    let mut p = ProtFlags::empty();
    if flags & PF_R != 0 {
        p |= ProtFlags::READ;
    }
    if flags & PF_W != 0 {
        p |= ProtFlags::WRITE;
    }
    if flags & PF_X != 0 {
        p |= ProtFlags::EXEC;
    }
    p
}

pub struct MappedObject {
    base: NonNull<u8>,

    span: usize,

    static_tls_offset: i64,

    region_start: u64,
}

unsafe impl Send for MappedObject {}
unsafe impl Sync for MappedObject {}

impl MappedObject {
    pub fn map_and_relocate(
        img: &ElfImage<'_>,
        file: &[u8],
        page_size: u64,
    ) -> Result<(Self, MapStats), MapError> {
        if img.loads.is_empty() {
            return Err(MapError::NoLoadSegments);
        }

        let min_vaddr = img.loads.iter().map(|s| s.vaddr).min().expect("non-empty");
        let mut max_end: u64 = 0;
        for s in &img.loads {
            if s.file_size > s.mem_size {
                return Err(MapError::FileSizeExceedsMemSize(s.file_size, s.mem_size));
            }
            let end = s
                .vaddr
                .checked_add(s.mem_size)
                .ok_or(MapError::SpanOverflow("vaddr + memsz"))?;
            max_end = max_end.max(end);
        }
        let region_start = page_floor(min_vaddr, page_size);
        let region_end =
            page_ceil(max_end, page_size).ok_or(MapError::SpanOverflow("memsz ceil"))?;
        let span_u64 = region_end
            .checked_sub(region_start)
            .ok_or(MapError::SpanOverflow("region end - start"))?;
        let span =
            usize::try_from(span_u64).map_err(|_| MapError::SpanOverflow("span as usize"))?;
        if span == 0 {
            return Err(MapError::NoLoadSegments);
        }

        let ptr = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                span,
                ProtFlags::empty(),
                MapFlags::PRIVATE,
            )
        }
        .map_err(MapError::Os)?;

        let base = NonNull::new(ptr.cast::<u8>()).ok_or(MapError::Os(rustix::io::Errno::NOMEM))?;

        const HUGE_PAGE_THRESHOLD: usize = 2 * 1024 * 1024;
        if span >= HUGE_PAGE_THRESHOLD {
            let _ = unsafe { madvise(ptr, span, Advice::LinuxHugepage) };
        }

        let load_base = (base.as_ptr() as u64).wrapping_sub(region_start);

        let mut obj = MappedObject {
            base,
            span,
            static_tls_offset: 0,
            region_start,
        };

        for seg in &img.loads {
            obj.populate_segment(file, seg, region_start, page_size)?;
        }

        let relas = img
            .relocations()
            .map_err(|_| MapError::SpanOverflow("relocations decode"))?;
        let relr = img
            .relr()
            .map_err(|_| MapError::SpanOverflow("relr decode"))?;

        let mut stats = MapStats {
            segments_mapped: img.loads.len(),
            ..Default::default()
        };

        let mut relative: Vec<reloc::Rela> = Vec::new();
        for r in &relas {
            if r.r_type == reloc::R_X86_64_RELATIVE {
                relative.push(*r);
            } else {
                stats.skipped_by_type += 1;
            }
        }
        stats.relative_applied = relative.len();

        stats.relr_applied = count_relr_targets(&relr);

        let relr_runtime: Vec<u64> = relr
            .iter()
            .map(|&w| {
                if w & 1 == 0 {
                    w.wrapping_add(load_base)
                } else {
                    w
                }
            })
            .collect();

        {
            let base_addr = load_base;
            let tls_off = obj.static_tls_offset;

            let bytes = unsafe { obj.image_bytes() };
            let mut image = reloc::SliceImage::new(base_addr, tls_off, bytes);
            reloc::apply_rela(&mut image, &NullResolver, &relative)?;
            reloc::apply_relr(&mut image, &relr_runtime)?;
        }

        for seg in &img.loads {
            obj.protect_segment(seg, region_start, page_size)?;
        }

        Ok((obj, stats))
    }

    pub fn relocate_symbols(
        &mut self,
        img: &ElfImage<'_>,
        scope: &Scope,
        page_size: u64,
    ) -> Result<SymbolRelocStats, MapError> {
        let min_vaddr = img
            .loads
            .iter()
            .map(|s| s.vaddr)
            .min()
            .ok_or(MapError::NoLoadSegments)?;
        let region_start = page_floor(min_vaddr, page_size);

        let relas = img
            .relocations()
            .map_err(|_| MapError::SpanOverflow("relocations decode"))?;
        let mut symbol_relas: Vec<reloc::Rela> = Vec::new();
        let mut stats = SymbolRelocStats::default();
        for r in &relas {
            match r.r_type {
                reloc::R_X86_64_GLOB_DAT => {
                    stats.glob_dat_applied += 1;
                    symbol_relas.push(*r);
                }
                reloc::R_X86_64_JUMP_SLOT => {
                    stats.jump_slot_applied += 1;
                    symbol_relas.push(*r);
                }
                reloc::R_X86_64_64 => {
                    stats.abs64_applied += 1;
                    symbol_relas.push(*r);
                }
                reloc::R_X86_64_TPOFF64 | R_X86_64_IRELATIVE => stats.deferred += 1,

                _ => {}
            }
        }

        let resolver = ScopedResolver::new(scope, &img.dynsyms);
        let load_base = self.load_base().wrapping_sub(region_start);
        let tls_off = self.static_tls_offset;

        for seg in &img.loads {
            self.mprotect_segment_pages(
                seg,
                region_start,
                page_size,
                ProtFlags::READ | ProtFlags::WRITE,
            )?;
        }

        let resolved_nonnull = {
            let bytes = unsafe { self.image_bytes() };
            let mut image = reloc::SliceImage::new(load_base, tls_off, bytes);
            reloc::apply_rela(&mut image, &resolver, &symbol_relas)?;

            let mut nonnull = 0usize;
            for r in &symbol_relas {
                let off = usize::try_from(r.offset)
                    .map_err(|_| MapError::SpanOverflow("reloc offset as usize"))?;
                if image.read_u64(off)? != 0 {
                    nonnull += 1;
                }
            }
            nonnull
        };
        stats.resolved_nonnull = resolved_nonnull;

        for seg in &img.loads {
            self.protect_segment(seg, region_start, page_size)?;
        }

        Ok(stats)
    }

    pub fn relocate_symbols_partial(
        &mut self,
        img: &ElfImage<'_>,
        scope: &Scope,
        page_size: u64,
    ) -> Result<PartialSymbolStats, MapError> {
        let min_vaddr = img
            .loads
            .iter()
            .map(|s| s.vaddr)
            .min()
            .ok_or(MapError::NoLoadSegments)?;
        let region_start = page_floor(min_vaddr, page_size);

        let relas = img
            .relocations()
            .map_err(|_| MapError::SpanOverflow("relocations decode"))?;

        let resolver = ScopedResolver::new(scope, &img.dynsyms);
        let mut to_apply: Vec<reloc::Rela> = Vec::new();
        let mut stats = PartialSymbolStats::default();
        let mut unresolved_names: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for r in &relas {
            match r.r_type {
                reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64 => {
                    match resolver.resolve_symbol(r.sym_index) {
                        Some(0) => {
                            stats.applied_weak_zero += 1;
                            to_apply.push(*r);
                        }
                        Some(_) => {
                            stats.applied_nonnull += 1;
                            to_apply.push(*r);
                        }
                        None => {
                            stats.unresolved_strong += 1;
                            let name = img
                                .dynsyms
                                .get(r.sym_index as usize)
                                .map(|s| s.name.clone())
                                .unwrap_or_default();
                            if !name.is_empty() {
                                unresolved_names.insert(name);
                            }
                        }
                    }
                }
                reloc::R_X86_64_TPOFF64 | R_X86_64_IRELATIVE => stats.deferred += 1,

                _ => {}
            }
        }
        stats.unresolved = unresolved_names.into_iter().collect();

        let load_base = self.load_base().wrapping_sub(region_start);
        let tls_off = self.static_tls_offset;

        for seg in &img.loads {
            self.mprotect_segment_pages(
                seg,
                region_start,
                page_size,
                ProtFlags::READ | ProtFlags::WRITE,
            )?;
        }
        {
            let bytes = unsafe { self.image_bytes() };
            let mut image = reloc::SliceImage::new(load_base, tls_off, bytes);
            for r in &to_apply {
                reloc::apply_one(&mut image, &resolver, r)?;
            }
        }
        for seg in &img.loads {
            self.protect_segment(seg, region_start, page_size)?;
        }

        Ok(stats)
    }

    pub fn relocate_tls<R: reloc::SymbolResolver>(
        &mut self,
        img: &ElfImage<'_>,
        inner: &R,
        layout: &TlsLayout,
        own_tp_offset: Option<i64>,
        page_size: u64,
    ) -> Result<TlsRelocStats, MapError> {
        let min_vaddr = img
            .loads
            .iter()
            .map(|s| s.vaddr)
            .min()
            .ok_or(MapError::NoLoadSegments)?;
        let region_start = page_floor(min_vaddr, page_size);

        let relas = img
            .relocations()
            .map_err(|_| MapError::SpanOverflow("relocations decode"))?;
        let mut tls_relas: Vec<reloc::Rela> = Vec::new();
        let mut stats = TlsRelocStats::default();
        for r in &relas {
            match r.r_type {
                reloc::R_X86_64_TPOFF64 => {
                    stats.tpoff64_applied += 1;
                    tls_relas.push(*r);
                }
                R_X86_64_IRELATIVE => stats.deferred += 1,

                _ => {}
            }
        }

        if tls_relas.is_empty() {
            return Ok(stats);
        }

        let resolver = TlsResolver::new(inner, &img.dynsyms, layout, own_tp_offset);
        let load_base = self.load_base().wrapping_sub(region_start);

        for seg in &img.loads {
            self.mprotect_segment_pages(
                seg,
                region_start,
                page_size,
                ProtFlags::READ | ProtFlags::WRITE,
            )?;
        }

        {
            let bytes = unsafe { self.image_bytes() };
            let mut image = reloc::SliceImage::new(load_base, 0, bytes);
            reloc::apply_rela(&mut image, &resolver, &tls_relas)?;
        }

        for seg in &img.loads {
            self.protect_segment(seg, region_start, page_size)?;
        }

        Ok(stats)
    }

    pub fn map_and_relocate_with_scope(
        img: &ElfImage<'_>,
        file: &[u8],
        page_size: u64,
        build_scope: impl FnOnce(u64, &[super::elf::DynSym]) -> Scope,
    ) -> Result<(Self, MapStats, SymbolRelocStats), MapError> {
        let (mut obj, map_stats) = Self::map_and_relocate(img, file, page_size)?;
        let scope = build_scope(obj.load_base(), &img.dynsyms);
        let sym_stats = obj.relocate_symbols(img, &scope, page_size)?;
        Ok((obj, map_stats, sym_stats))
    }

    fn populate_segment(
        &mut self,
        file: &[u8],
        seg: &LoadSegment,
        region_start: u64,
        page_size: u64,
    ) -> Result<(), MapError> {
        let seg_off_in_region = seg
            .vaddr
            .checked_sub(region_start)
            .ok_or(MapError::SpanOverflow("vaddr - region_start"))?;
        let seg_off = usize::try_from(seg_off_in_region)
            .map_err(|_| MapError::SpanOverflow("segment offset as usize"))?;

        self.mprotect_segment_pages(
            seg,
            region_start,
            page_size,
            ProtFlags::READ | ProtFlags::WRITE,
        )?;

        let filesz = usize::try_from(seg.file_size)
            .map_err(|_| MapError::SpanOverflow("filesz as usize"))?;
        if filesz == 0 {
            return Ok(());
        }
        let file_off = usize::try_from(seg.file_offset)
            .map_err(|_| MapError::SpanOverflow("file_offset as usize"))?;
        let file_end = file_off
            .checked_add(filesz)
            .ok_or(MapError::SpanOverflow("file_offset + filesz"))?;
        let src = file
            .get(file_off..file_end)
            .ok_or(MapError::SegmentOutOfFile(seg.file_offset))?;

        let dst = unsafe {
            let end = seg_off
                .checked_add(filesz)
                .ok_or(MapError::SpanOverflow("seg_off + filesz"))?;
            if end > self.span {
                return Err(MapError::SpanOverflow("segment past span"));
            }
            std::slice::from_raw_parts_mut(self.base.as_ptr().add(seg_off), filesz)
        };
        dst.copy_from_slice(src);
        Ok(())
    }

    fn protect_segment(
        &self,
        seg: &LoadSegment,
        region_start: u64,
        page_size: u64,
    ) -> Result<(), MapError> {
        let prot = prot_of(seg.flags);
        self.mprotect_segment_pages(seg, region_start, page_size, prot)
    }

    fn mprotect_segment_pages(
        &self,
        seg: &LoadSegment,
        region_start: u64,
        page_size: u64,
        prot: ProtFlags,
    ) -> Result<(), MapError> {
        let seg_start = page_floor(seg.vaddr, page_size);
        let seg_end_unaligned = seg
            .vaddr
            .checked_add(seg.mem_size)
            .ok_or(MapError::SpanOverflow("vaddr + memsz"))?;
        let seg_end = page_ceil(seg_end_unaligned, page_size)
            .ok_or(MapError::SpanOverflow("segment end ceil"))?;

        if seg_end <= seg_start {
            return Ok(());
        }
        let off_in_region = seg_start
            .checked_sub(region_start)
            .ok_or(MapError::SpanOverflow("seg_start - region_start"))?;
        let off = usize::try_from(off_in_region)
            .map_err(|_| MapError::SpanOverflow("protect offset as usize"))?;
        let len = usize::try_from(seg_end - seg_start)
            .map_err(|_| MapError::SpanOverflow("protect len as usize"))?;
        if off.checked_add(len).map(|e| e > self.span).unwrap_or(true) {
            return Err(MapError::SpanOverflow("protect range past span"));
        }

        unsafe {
            mprotect(
                self.base.as_ptr().add(off).cast(),
                len,
                mprotect_flags(prot),
            )
        }
        .map_err(MapError::Os)
    }

    pub fn apply_relro(&self, relro: &RelroSegment, page_size: u64) -> Result<(), MapError> {
        let prot_start = page_floor(relro.vaddr, page_size);
        let end_unaligned = relro
            .vaddr
            .checked_add(relro.mem_size)
            .ok_or(MapError::SpanOverflow("relro vaddr + memsz"))?;

        let prot_end = page_floor(end_unaligned, page_size);
        if prot_end <= prot_start {
            return Ok(());
        }
        let off_in_region = prot_start
            .checked_sub(self.region_start)
            .ok_or(MapError::SpanOverflow("relro start - region_start"))?;
        let off = usize::try_from(off_in_region)
            .map_err(|_| MapError::SpanOverflow("relro offset as usize"))?;
        let len = usize::try_from(prot_end - prot_start)
            .map_err(|_| MapError::SpanOverflow("relro len as usize"))?;
        if off.checked_add(len).map(|e| e > self.span).unwrap_or(true) {
            return Err(MapError::SpanOverflow("relro range past span"));
        }

        unsafe {
            mprotect(
                self.base.as_ptr().add(off).cast(),
                len,
                mprotect_flags(ProtFlags::READ),
            )
        }
        .map_err(MapError::Os)
    }

    pub fn load_base(&self) -> u64 {
        self.base.as_ptr() as u64
    }

    pub fn span(&self) -> usize {
        self.span
    }

    pub fn read_u64(&self, off: usize) -> Result<u64, MapError> {
        let end = off
            .checked_add(8)
            .ok_or(MapError::SpanOverflow("read offset + 8"))?;
        if end > self.span {
            return Err(MapError::SpanOverflow("read past span"));
        }

        let bytes = unsafe { std::slice::from_raw_parts(self.base.as_ptr().add(off), 8) };
        Ok(u64::from_le_bytes(bytes.try_into().expect("8-byte slice")))
    }

    pub unsafe fn image_bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), self.span) }
    }
}

impl Drop for MappedObject {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.span) };
    }
}

fn mprotect_flags(prot: ProtFlags) -> MprotectFlags {
    let mut m = MprotectFlags::empty();
    if prot.contains(ProtFlags::READ) {
        m |= MprotectFlags::READ;
    }
    if prot.contains(ProtFlags::WRITE) {
        m |= MprotectFlags::WRITE;
    }
    if prot.contains(ProtFlags::EXEC) {
        m |= MprotectFlags::EXEC;
    }
    m
}

struct NullResolver;
impl reloc::SymbolResolver for NullResolver {
    fn resolve_symbol(&self, _i: u32) -> Option<u64> {
        None
    }
    fn resolve_tls_offset(&self, _i: u32) -> Option<u64> {
        None
    }
}

fn count_relr_targets(entries: &[u64]) -> usize {
    let mut n = 0usize;
    for &entry in entries {
        if entry & 1 == 0 {
            n += 1;
        } else {
            n += (entry >> 1).count_ones() as usize;
        }
    }
    n
}

pub fn host_page_size() -> u64 {
    rustix::param::page_size() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::elf::{ElfImage, PF_R, PF_W, PF_X};
    use crate::loader::reloc::R_X86_64_RELATIVE;

    const PAGE: u64 = 0x1000;
    const PH_OFF: usize = 0x40;
    const DYN_OFF: u64 = 0x200;
    const SYM_OFF: u64 = 0x280;
    const RELA_OFF: u64 = 0x300;
    const RELR_OFF: u64 = 0x380;
    const STR_OFF: u64 = 0x3c0;

    const RELA_TARGET: u64 = 0x1000;
    const RELR_TARGET: u64 = 0x1008;
    const DATA_FILE_OFF: u64 = 0x1000;
    const DATA_FILESZ: u64 = 0x40;
    const DATA_MEMSZ: u64 = 0x80;
    const FILE_SIZE: usize = 0x1040;

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
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const PT_GNU_RELRO: u32 = 0x6474_e552;
    const DT_NULL: i64 = 0;
    const DT_RELA: i64 = 7;
    const DT_RELASZ: i64 = 8;
    const DT_RELAENT: i64 = 9;
    const DT_RELR: i64 = 36;
    const DT_RELRSZ: i64 = 35;
    const DT_RELRENT: i64 = 37;
    const DT_STRTAB: i64 = 5;
    const DT_STRSZ: i64 = 10;
    const DT_SYMTAB: i64 = 6;
    const DT_SYMENT: i64 = 11;
    const SYM_ENT: u64 = 24;
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

    fn put_dyn(buf: &mut [u8], slot: usize, tag: i64, val: u64) {
        let off = DYN_OFF as usize + slot * DYN_SIZE;
        put_u64(buf, off, tag as u64);
        put_u64(buf, off + 8, val);
    }

    const TEXT_MARK_OFF: usize = 0x100;
    const TEXT_MARK: u64 = 0xdead_beef_cafe_babe;

    const RELA_ADDEND: i64 = 0x1234;
    const RELR_SEED: u64 = 0x40;

    fn build_two_segment_fixture() -> Vec<u8> {
        let mut buf = vec![0u8; FILE_SIZE];

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

        put_phdr(&mut buf, 0, PT_LOAD, PF_R | PF_X, 0, 0, PAGE, PAGE, PAGE);
        put_phdr(
            &mut buf,
            1,
            PT_LOAD,
            PF_R | PF_W,
            DATA_FILE_OFF,
            0x1000,
            DATA_FILESZ,
            DATA_MEMSZ,
            PAGE,
        );
        put_phdr(
            &mut buf,
            2,
            PT_DYNAMIC,
            PF_R | PF_W,
            DYN_OFF,
            DYN_OFF,
            0x100,
            0x100,
            8,
        );

        let mut slot = 0;
        let mut d = |buf: &mut [u8], tag: i64, val: u64| {
            put_dyn(buf, slot, tag, val);
            slot += 1;
        };
        d(&mut buf, DT_RELA, RELA_OFF);
        d(&mut buf, DT_RELASZ, RELA_ENT);
        d(&mut buf, DT_RELAENT, RELA_ENT);
        d(&mut buf, DT_RELR, RELR_OFF);
        d(&mut buf, DT_RELRSZ, 8);
        d(&mut buf, DT_RELRENT, 8);
        d(&mut buf, DT_SYMTAB, SYM_OFF);
        d(&mut buf, DT_SYMENT, SYM_ENT);
        d(&mut buf, DT_STRTAB, STR_OFF);
        d(&mut buf, DT_STRSZ, 1);
        d(&mut buf, DT_NULL, 0);

        put_u64(&mut buf, RELA_OFF as usize, RELA_TARGET);
        put_u64(&mut buf, RELA_OFF as usize + 8, R_X86_64_RELATIVE as u64);
        put_u64(&mut buf, RELA_OFF as usize + 16, RELA_ADDEND as u64);

        put_u64(&mut buf, RELR_OFF as usize, RELR_TARGET);

        buf[STR_OFF as usize] = 0;

        put_u64(&mut buf, TEXT_MARK_OFF, TEXT_MARK);

        put_u64(&mut buf, RELR_TARGET as usize, RELR_SEED);

        buf
    }

    fn build_relro_fixture() -> Vec<u8> {
        let mut buf = build_two_segment_fixture();
        put_u16(&mut buf, 56, 4);

        put_phdr(
            &mut buf,
            3,
            PT_GNU_RELRO,
            PF_R,
            0x1000,
            0x1000,
            PAGE,
            PAGE,
            1,
        );
        buf
    }

    fn read_word(obj: &mut MappedObject, vaddr: u64) -> u64 {
        let bytes = unsafe { obj.image_bytes() };
        let off = vaddr as usize;
        u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn maps_segments_and_zeroes_bss() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).expect("fixture parses");
        assert_eq!(img.loads.len(), 2);

        let (mut obj, stats) =
            MappedObject::map_and_relocate(&img, &buf, PAGE).expect("map+relocate");
        assert_eq!(stats.segments_mapped, 2);

        assert_eq!(read_word(&mut obj, TEXT_MARK_OFF as u64), TEXT_MARK);

        for off in (DATA_FILESZ..DATA_MEMSZ).step_by(8) {
            assert_eq!(read_word(&mut obj, 0x1000 + off), 0, "bss word at {off:#x}");
        }
    }

    #[test]
    fn relative_reloc_rewrites_target_with_base() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let (mut obj, stats) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();

        assert_eq!(stats.relative_applied, 1);
        let base = obj.load_base();

        let got = read_word(&mut obj, RELA_TARGET);
        assert_eq!(got, base.wrapping_add(RELA_ADDEND as u64));

        assert!(
            got >= base && got < base + obj.span() as u64,
            "RELATIVE target {got:#x} must be inside [{base:#x}, {:#x})",
            base + obj.span() as u64
        );
    }

    #[test]
    fn relr_reloc_adds_base_to_seeded_word() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let (mut obj, stats) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();

        assert_eq!(stats.relr_applied, 1);
        let base = obj.load_base();

        let got = read_word(&mut obj, RELR_TARGET);
        assert_eq!(got, base.wrapping_add(RELR_SEED));
        assert!(got >= base && got < base + obj.span() as u64);
    }

    #[test]
    fn page_rounding_span_is_correct() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let (obj, _) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();
        assert_eq!(obj.span(), 0x2000);
    }

    #[test]
    fn no_load_segments_is_typed_err() {
        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS] = ELFCLASS64;
        buf[EI_DATA] = ELFDATA2LSB;
        put_u16(&mut buf, 16, ET_DYN);
        put_u16(&mut buf, 18, EM_X86_64);
        put_u64(&mut buf, 32, PH_OFF as u64);
        put_u16(&mut buf, 54, PHDR_SIZE as u16);
        put_u16(&mut buf, 56, 0);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            MappedObject::map_and_relocate(&img, &buf, PAGE),
            Err(MapError::NoLoadSegments)
        ));
    }

    #[test]
    fn drop_unmaps_without_leak() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        for _ in 0..256 {
            let (obj, _) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();
            assert_eq!(obj.span(), 0x2000);
            drop(obj);
        }
    }

    #[test]
    fn apply_relro_hardens_region_and_keeps_it_readable() {
        let buf = build_relro_fixture();
        let img = ElfImage::parse(&buf).expect("relro fixture parses");
        assert!(img.relro.is_some(), "fixture declares PT_GNU_RELRO");
        let relro = img.relro.unwrap();
        assert_eq!(relro.vaddr, 0x1000);
        assert_eq!(relro.mem_size, PAGE);

        let (mut obj, stats) =
            MappedObject::map_and_relocate(&img, &buf, PAGE).expect("map+base-relocate");
        assert_eq!(stats.relative_applied, 1);
        let base = obj.load_base();

        obj.apply_relro(&relro, PAGE).expect("apply_relro succeeds");

        assert_eq!(read_word(&mut obj, RELA_TARGET), base + RELA_ADDEND as u64);

        assert_eq!(read_word(&mut obj, RELR_TARGET), RELR_SEED + base);
    }

    #[test]
    fn apply_relro_subpage_region_is_a_clean_noop() {
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let (obj, _) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();

        let tiny = RelroSegment {
            vaddr: 0x1000,
            mem_size: 0x40,
        };
        obj.apply_relro(&tiny, PAGE)
            .expect("sub-page relro is a no-op Ok");
    }

    #[test]
    fn count_relr_targets_matches_encoding() {
        let data_bits = 0b1011u64;
        let bitmap = (data_bits << 1) | 1;
        assert_eq!(count_relr_targets(&[0x1000, bitmap]), 1 + 3);

        assert_eq!(count_relr_targets(&[]), 0);
    }

    #[test]
    fn real_libm_maps_and_base_relocates() {
        const CANDIDATES: &[&str] = &[
            "/usr/lib/libm.so.6",
            "/usr/lib/x86_64-linux-gnu/libm.so.6",
            "/lib/x86_64-linux-gnu/libm.so.6",
        ];
        let Some(path) = CANDIDATES.iter().find(|p| std::path::Path::new(p).exists()) else {
            eprintln!("real_libm_maps_and_base_relocates: no host libm.so.6; skipping");
            return;
        };
        let bytes = std::fs::read(path).expect("read libm bytes");
        let img = ElfImage::parse(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"));
        let page = host_page_size();

        let (mut obj, stats) = MappedObject::map_and_relocate(&img, &bytes, page)
            .unwrap_or_else(|e| panic!("map {path}: {e}"));

        let base = obj.load_base();
        let span = obj.span() as u64;
        assert!(
            span > 0 && base != 0,
            "mapping must succeed with a real base"
        );
        assert_eq!(stats.segments_mapped, img.loads.len());

        let relas = img.relocations().unwrap();
        let mut checked_relative = 0usize;
        {
            let image = unsafe { obj.image_bytes() };
            for r in &relas {
                if r.r_type != R_X86_64_RELATIVE {
                    continue;
                }
                let off = r.offset as usize;
                let word = u64::from_le_bytes(image[off..off + 8].try_into().unwrap());
                assert!(
                    word >= base && word < base + span,
                    "RELATIVE @ {:#x} → {word:#x} not in [{base:#x}, {:#x})",
                    r.offset,
                    base + span
                );
                checked_relative += 1;
            }
        }

        let relr = img.relr().unwrap();
        let relr_count = count_relr_targets(&relr);

        eprintln!(
            "real_libm_maps_and_base_relocates: {path} — segments={} RELATIVE_applied={} RELR_applied={} skipped_by_type={} (verified {checked_relative} RELATIVE targets in-object)",
            stats.segments_mapped, stats.relative_applied, stats.relr_applied, stats.skipped_by_type,
        );
        assert_eq!(stats.relative_applied, checked_relative);
        assert_eq!(stats.relr_applied, relr_count);
    }

    #[test]
    fn real_libm_resolves_and_applies_symbol_relocations() {
        use super::super::resolve::{
            HostDlsymProvider, LoadedObjectProvider, Scope, SymbolProvider,
        };

        const CANDIDATES: &[&str] = &[
            "/usr/lib/libm.so.6",
            "/usr/lib/x86_64-linux-gnu/libm.so.6",
            "/lib/x86_64-linux-gnu/libm.so.6",
        ];
        let Some(path) = CANDIDATES.iter().find(|p| std::path::Path::new(p).exists()) else {
            eprintln!(
                "real_libm_resolves_and_applies_symbol_relocations: no host libm.so.6; skipping"
            );
            return;
        };
        let bytes = std::fs::read(path).expect("read libm bytes");
        let img = ElfImage::parse(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"));
        let page = host_page_size();

        let (mut obj, sym_stats) = {
            let (obj, map_stats) =
                MappedObject::map_and_relocate(&img, &bytes, page).expect("map+base-relocate libm");
            let base = obj.load_base();

            let mut scope = Scope::new();
            scope
                .push(Box::new(LoadedObjectProvider::new(base, &img.dynsyms)))
                .push(Box::new(HostDlsymProvider));
            let mut obj = obj;
            let sym_stats = obj
                .relocate_symbols(&img, &scope, page)
                .unwrap_or_else(|e| panic!("symbol relocate {path}: {e}"));

            assert_eq!(
                sym_stats.total_applied() + sym_stats.deferred,
                map_stats.skipped_by_type,
                "every base-deferred reloc is accounted for as applied or deferred"
            );
            (obj, sym_stats)
        };

        let base = obj.load_base();
        let span = obj.span() as u64;

        let relas = img.relocations().unwrap();
        let mut scope = Scope::new();
        scope
            .push(Box::new(LoadedObjectProvider::new(base, &img.dynsyms)))
            .push(Box::new(HostDlsymProvider));
        let mut strong_count = 0usize;
        let mut weak_zero_count = 0usize;
        let mut total_symbol = 0usize;
        {
            let image = unsafe { obj.image_bytes() };
            for r in &relas {
                let is_symbol = matches!(
                    r.r_type,
                    reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
                );
                if !is_symbol {
                    continue;
                }
                total_symbol += 1;
                let off = r.offset as usize;
                let word = u64::from_le_bytes(image[off..off + 8].try_into().unwrap());

                let sym = &img.dynsyms[r.sym_index as usize];
                let scope_hit = scope.resolve(&sym.name);
                if scope_hit.is_some() {
                    assert_ne!(word, 0, "resolved {} wrote a null slot", sym.name);
                    strong_count += 1;

                    if let Some(off_in_obj) = LoadedObjectProvider::new(base, &img.dynsyms)
                        .resolve(&sym.name)
                        .filter(|s| !s.weak || scope_hit == Some(*s))
                        .map(|s| s.addr)
                    {
                        if off_in_obj == word {
                            assert!(
                                word >= base && word < base + span,
                                "self-defined {} → {word:#x} not in [{base:#x}, {:#x})",
                                sym.name,
                                base + span
                            );
                        }
                    }
                } else {
                    assert_eq!(
                        sym.bind, 2,
                        "STRONG symbol {} was unresolved (would be a typed error)",
                        sym.name
                    );
                    assert_eq!(word, 0, "weak-undef {} must be 0", sym.name);
                    weak_zero_count += 1;
                }
            }
        }

        eprintln!(
            "real_libm_resolves_and_applies_symbol_relocations: {path} — total_symbol_relocs={total_symbol} \
             GLOB_DAT={} JUMP_SLOT={} ABS64={} resolved_nonnull={} weak_undef_zero={} deferred(TPOFF64/IRELATIVE)={} \
             (strong_resolved={strong_count})",
            sym_stats.glob_dat_applied,
            sym_stats.jump_slot_applied,
            sym_stats.abs64_applied,
            sym_stats.resolved_nonnull,
            weak_zero_count,
            sym_stats.deferred,
        );

        assert_eq!(strong_count + weak_zero_count, total_symbol);
        assert_eq!(sym_stats.total_applied(), total_symbol);
        assert_eq!(sym_stats.resolved_nonnull, strong_count);

        assert!(
            strong_count >= 20,
            "expected most of libm's symbol relocs to resolve, got {strong_count}/{total_symbol}"
        );
    }

    #[test]
    fn real_libm_applies_tpoff64_through_libc_tls_layout() {
        use super::super::resolve::{HostDlsymProvider, LoadedObjectProvider, Scope};
        use super::super::tls::TlsLayout;

        const LIBM: &[&str] = &[
            "/usr/lib/libm.so.6",
            "/usr/lib/x86_64-linux-gnu/libm.so.6",
            "/lib/x86_64-linux-gnu/libm.so.6",
        ];
        const LIBC: &[&str] = &[
            "/usr/lib/libc.so.6",
            "/usr/lib/x86_64-linux-gnu/libc.so.6",
            "/lib/x86_64-linux-gnu/libc.so.6",
        ];
        let (Some(libm_path), Some(libc_path)) = (
            LIBM.iter().find(|p| std::path::Path::new(p).exists()),
            LIBC.iter().find(|p| std::path::Path::new(p).exists()),
        ) else {
            eprintln!(
                "real_libm_applies_tpoff64_through_libc_tls_layout: no host libm/libc; skipping"
            );
            return;
        };

        let page = host_page_size();

        let libc_bytes = std::fs::read(libc_path).expect("read libc bytes");
        let libc_img =
            ElfImage::parse(&libc_bytes).unwrap_or_else(|e| panic!("parse {libc_path}: {e}"));
        let libc_tls = libc_img
            .tls
            .expect("libc.so.6 must have a PT_TLS (it defines errno/etc.)");
        let tdata_off = libc_img
            .vaddr_to_off(libc_tls.vaddr)
            .expect("libc PT_TLS vaddr maps to a file offset");
        let mut tls_layout = TlsLayout::new();
        tls_layout
            .add_module(&libc_tls, &libc_bytes, tdata_off as u64, &libc_img.dynsyms)
            .unwrap_or_else(|e| panic!("layout libc TLS: {e}"));

        let errno_sym = libc_img
            .dynsyms
            .iter()
            .find(|s| s.name == "errno" && s.shndx != 0 && s.sym_type == 6)
            .expect("libc defines a TLS `errno`");
        let memsz = libc_tls.mem_size;
        let align = libc_tls.align.max(1);
        let offset_1 = memsz.div_ceil(align) * align;
        let expected_errno_tp = -(offset_1 as i64) + errno_sym.value as i64;
        assert_eq!(
            tls_layout.tp_offset_of("errno"),
            Some(expected_errno_tp),
            "TlsLayout's errno tp-offset must match the variant-II hand computation"
        );

        let libm_bytes = std::fs::read(libm_path).expect("read libm bytes");
        let libm_img =
            ElfImage::parse(&libm_bytes).unwrap_or_else(|e| panic!("parse {libm_path}: {e}"));

        let (mut obj, map_stats) = MappedObject::map_and_relocate(&libm_img, &libm_bytes, page)
            .expect("map+base-relocate libm");
        let base = obj.load_base();

        let mut scope = Scope::new();
        scope
            .push(Box::new(LoadedObjectProvider::new(base, &libm_img.dynsyms)))
            .push(Box::new(HostDlsymProvider));
        let sym_stats = obj
            .relocate_symbols(&libm_img, &scope, page)
            .expect("symbol relocate libm");

        let inner = ScopedResolver::new(&scope, &libm_img.dynsyms);

        let tls_stats = obj
            .relocate_tls(&libm_img, &inner, &tls_layout, None, page)
            .expect("tls relocate libm");

        assert_eq!(tls_stats.tpoff64_applied, 1, "libm has exactly one TPOFF64");
        assert_eq!(
            tls_stats.deferred, 0,
            "libm has zero IRELATIVE → nothing deferred"
        );

        let relas = libm_img.relocations().unwrap();
        let tpoff = relas
            .iter()
            .find(|r| r.r_type == reloc::R_X86_64_TPOFF64)
            .expect("libm has a TPOFF64");
        let sym_name = libm_img.dynsyms[tpoff.sym_index as usize].name.clone();
        let expected = expected_errno_tp.wrapping_add(tpoff.addend) as u64;
        let written = {
            let image = unsafe { obj.image_bytes() };
            let off = tpoff.offset as usize;
            u64::from_le_bytes(image[off..off + 8].try_into().unwrap())
        };
        assert_eq!(
            written, expected,
            "TPOFF64 for {sym_name} wrote {written:#x}, expected tp_offset+addend {expected:#x}"
        );

        let total_relocs = relas.len();
        let applied =
            map_stats.relative_applied + sym_stats.total_applied() + tls_stats.tpoff64_applied;

        assert_eq!(
            applied, total_relocs,
            "every libm .rela reloc applied (base RELATIVE + symbol + TPOFF64): {applied} of {total_relocs}"
        );
        assert_eq!(
            tls_stats.deferred, 0,
            "nothing deferred (libm has no IRELATIVE)"
        );

        eprintln!(
            "real_libm_applies_tpoff64_through_libc_tls_layout: {libm_path} — TPOFF64 sym={sym_name} \
             tp_offset={expected_errno_tp:#x} (errno st_value={:#x}, libc PT_TLS memsz={memsz:#x} align={align:#x}) \
             written={written:#x} addend={} — libm FULLY relocated modulo ifunc (IRELATIVE deferred=0)",
            errno_sym.value, tpoff.addend,
        );
    }

    fn build_load0_filesz_overrun_fixture(bad_filesz: u64) -> Vec<u8> {
        let mut buf = build_two_segment_fixture();

        let needed = bad_filesz as usize;
        if buf.len() < needed {
            buf.resize(needed, 0);
        }

        let ph0 = PH_OFF;
        put_u64(&mut buf, ph0 + 32, bad_filesz);

        buf
    }

    #[test]
    fn filesz_greater_than_memsz_is_typed_err_no_fault() {
        let buf = build_load0_filesz_overrun_fixture(0x1800);
        let img = ElfImage::parse(&buf).expect("fixture still parses (header/phdr valid)");
        assert!(img.loads[0].file_size > img.loads[0].mem_size);
        match MappedObject::map_and_relocate(&img, &buf, PAGE) {
            Err(MapError::FileSizeExceedsMemSize(fz, mz)) => {
                assert_eq!(fz, 0x1800);
                assert_eq!(mz, PAGE);
            }
            Err(other) => panic!("expected FileSizeExceedsMemSize, got {other}"),
            Ok(_) => panic!("expected FileSizeExceedsMemSize, mapping unexpectedly succeeded"),
        }
    }

    #[test]
    fn filesz_one_byte_over_memsz_is_rejected() {
        let buf = build_load0_filesz_overrun_fixture(PAGE + 1);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            MappedObject::map_and_relocate(&img, &buf, PAGE),
            Err(MapError::FileSizeExceedsMemSize(_, _))
        ));
    }

    #[test]
    fn filesz_equal_to_memsz_still_maps() {
        let buf = build_load0_filesz_overrun_fixture(PAGE);
        let img = ElfImage::parse(&buf).unwrap();
        let (obj, stats) =
            MappedObject::map_and_relocate(&img, &buf, PAGE).expect("filesz==memsz is legal");
        assert_eq!(stats.segments_mapped, 2);
        assert_eq!(obj.span(), 0x2000);
    }

    #[test]
    fn vaddr_plus_memsz_overflow_is_typed_err() {
        let mut buf = build_two_segment_fixture();
        let ph1 = PH_OFF + PHDR_SIZE;
        put_u64(&mut buf, ph1 + 16, u64::MAX - 0x10);
        put_u64(&mut buf, ph1 + 32, 0);
        put_u64(&mut buf, ph1 + 40, 0x1000);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            MappedObject::map_and_relocate(&img, &buf, PAGE),
            Err(MapError::SpanOverflow(_))
        ));
    }

    #[test]
    fn absurdly_huge_span_refuses_rather_than_mmaps() {
        let mut buf = build_two_segment_fixture();
        let ph1 = PH_OFF + PHDR_SIZE;
        put_u64(&mut buf, ph1 + 16, 0x1000);
        put_u64(&mut buf, ph1 + 32, 0);
        put_u64(&mut buf, ph1 + 40, 1u64 << 62);
        let img = ElfImage::parse(&buf).unwrap();
        match MappedObject::map_and_relocate(&img, &buf, PAGE) {
            Err(MapError::SpanOverflow(_)) | Err(MapError::Os(_)) => {}
            Err(other) => panic!("absurd span must refuse with a span/os error, got {other}"),
            Ok(_) => panic!("absurd span must refuse, mapping unexpectedly succeeded"),
        }
    }

    #[test]
    fn segment_filesz_past_file_bytes_is_typed_err() {
        let mut buf = build_two_segment_fixture();
        let ph1 = PH_OFF + PHDR_SIZE;
        put_u64(&mut buf, ph1 + 8, 0x1000);
        put_u64(&mut buf, ph1 + 32, 0x4000);
        put_u64(&mut buf, ph1 + 40, 0x4000);
        let img = ElfImage::parse(&buf).unwrap();
        assert!(matches!(
            MappedObject::map_and_relocate(&img, &buf, PAGE),
            Err(MapError::SegmentOutOfFile(_))
        ));
    }
}
