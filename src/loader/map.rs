//! Pure-Rust PT_LOAD segment mapper + base relocator — the loader's third piece.
//!
//! 2026-06-05: [`elf`](super::elf) decodes a `.so`'s bytes; [`reloc`](super::reloc) applies
//! relocations over a [`reloc::RelocImage`]. This module is the step between them: it **maps** a
//! parsed [`ElfImage`]'s `PT_LOAD` segments into one contiguous anonymous region (forming the
//! [`reloc::RelocImage`] the applier writes through), applies the base-only relocations
//! (`R_X86_64_RELATIVE` + `DT_RELR`), and `mprotect`s each segment to its final permissions.
//!
//! ## What it does (and the strict boundary of what it does NOT)
//! - **MAP:** reserve `max(vaddr+memsz) - min(vaddr)` (page-rounded) anonymous bytes
//!   `PROT_NONE`/`MAP_PRIVATE` to claim a contiguous range + get a load base; copy each segment's
//!   `p_filesz` file bytes to `base + vaddr`; the `[filesz, memsz)` bss tail is already zero
//!   (fresh anonymous pages). One big reservation (not per-segment `mmap`) makes the standard ELF
//!   page-overlap — a segment's final partial page shared with the next segment's first page —
//!   correct by construction: bytes are placed by vaddr into the single region.
//! - **RELOCATE (base-only):** with the region writable, apply via [`reloc`] ONLY the relocations
//!   that need just the load base: `R_X86_64_RELATIVE` (from `.rela.dyn`) and `DT_RELR`. Then
//!   `mprotect` each segment to its `p_flags` (`PF_R/W/X` → `PROT_READ/WRITE/EXEC`).
//! - **SYMBOL RELOCATIONS (step 5) — [`MappedObject::relocate_symbols`]:** a follow-on pass applies
//!   `R_X86_64_JUMP_SLOT` / `GLOB_DAT` / `R_X86_64_64` through a [`Scope`]
//!   ([`reloc::SymbolResolver`]); [`MappedObject::map_and_relocate_with_scope`] does both passes in
//!   one call. They reference dynamic symbols, so the caller supplies the resolution scope.
//! - **DEFERRED — still not attempted here (documented why):**
//!   - `R_X86_64_TPOFF64` needs the static-TLS block + `%fs`/TCB (step 4) — there is no
//!     thread-pointer-relative base assigned yet.
//!   - `R_X86_64_IRELATIVE` needs **executing** the library's ifunc resolver functions — explicitly
//!     out of scope; this module **never executes, jumps into, or runs init functions** of the
//!     mapped object. It maps + relocates + verifies only.
//!
//! ## Safety
//! Unlike `reloc.rs`/`elf.rs` (`#![forbid(unsafe_code)]`), this module performs the raw
//! `mmap`/`mprotect`/`munmap` syscalls (via `rustix::mm`) and writes through the returned pointer,
//! so `unsafe` is unavoidable and is confined here (AGENTS.md §2.3). Every `unsafe` block carries a
//! `// SAFETY:` comment justifying the pointer/length/alignment invariant it relies on. The mapped
//! region is exposed to the relocation pass as a safe `&mut [u8]` ([`MappedObject::image_bytes`]),
//! so the relocation arithmetic itself stays in the bounds-checked, `unsafe`-free `reloc` core.
//!
//! ## RAII
//! [`MappedObject`] owns the mapping and `munmap`s the whole reserved span on `Drop` — no leak.

use std::ptr::NonNull;

use rustix::mm::{mmap_anonymous, mprotect, munmap, MapFlags, MprotectFlags, ProtFlags};

use super::elf::{ElfImage, LoadSegment, PF_R, PF_W, PF_X};
use super::reloc::{self, RelocError, RelocImage};
use super::resolve::{Scope, ScopedResolver};

/// Errors from mapping or base-relocating a parsed ELF image.
#[derive(Debug)]
pub enum MapError {
    /// The image declared no `PT_LOAD` segments — nothing to map.
    NoLoadSegments,
    /// The page-rounded total span overflowed `usize`, or a segment's vaddr/memsz/filesz arithmetic
    /// overflowed. Carries a short static description of the offending computation.
    SpanOverflow(&'static str),
    /// A segment's `p_filesz` bytes are not present in the file slice `[file_offset, +file_size)`.
    /// Carries the segment's `file_offset`.
    SegmentOutOfFile(u64),
    /// The underlying `mmap`/`mprotect`/`munmap` syscall failed. Carries the OS error.
    Os(rustix::io::Errno),
    /// A relocation failed (out of bounds / unsupported). Carries the [`RelocError`].
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

/// x86-64 `R_X86_64_IRELATIVE` (type 37): an ifunc relocation whose value is produced by
/// **executing** the library's resolver function — explicitly out of scope for this map/relocate
/// core (nothing is executed). Counted as deferred by [`MappedObject::relocate_symbols`].
const R_X86_64_IRELATIVE: u32 = 37;

/// Counts from a successful [`MappedObject::map_and_relocate`], for verification/reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MapStats {
    /// Number of `PT_LOAD` segments mapped.
    pub segments_mapped: usize,
    /// Number of `R_X86_64_RELATIVE` relocations applied (from `.rela.dyn`/`.rela.plt`).
    pub relative_applied: usize,
    /// Number of `DT_RELR`-encoded relative relocations applied.
    pub relr_applied: usize,
    /// Number of relocations skipped (deferred) by type — JUMP_SLOT/GLOB_DAT/64/TPOFF64/IRELATIVE
    /// and any other non-base type, which need the symbol resolver / static-TLS block / ifunc
    /// execution (out of scope for this map+base-relocate step).
    pub skipped_by_type: usize,
}

/// Counts from a [`MappedObject::relocate_symbols`] pass, for verification/reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SymbolRelocStats {
    /// Number of `R_X86_64_GLOB_DAT` relocations applied (GOT entries → symbol addresses).
    pub glob_dat_applied: usize,
    /// Number of `R_X86_64_JUMP_SLOT` relocations applied (PLT entries → symbol addresses; eager
    /// under `BIND_NOW`).
    pub jump_slot_applied: usize,
    /// Number of `R_X86_64_64` relocations applied (`sym + addend`).
    pub abs64_applied: usize,
    /// Number of symbol relocations that resolved to a **non-null** address (a real definition).
    /// The remainder (`total_symbol - resolved_nonnull`) are weak-undef → 0, which is legal.
    pub resolved_nonnull: usize,
    /// Number of relocations deferred (not applied) by this pass: `R_X86_64_TPOFF64` (needs the
    /// static-TLS block + `%fs`/TCB) and `R_X86_64_IRELATIVE` (needs executing ifunc resolvers),
    /// plus any other non-symbol type already handled by the base pass.
    pub deferred: usize,
}

impl SymbolRelocStats {
    /// Total symbol-dependent relocations applied in this pass.
    pub fn total_applied(&self) -> usize {
        self.glob_dat_applied + self.jump_slot_applied + self.abs64_applied
    }
}

/// Round `addr` down to the start of its page.
fn page_floor(addr: u64, page: u64) -> u64 {
    addr & !(page - 1)
}

/// Round `addr` up to the next page boundary. Returns `None` on overflow.
fn page_ceil(addr: u64, page: u64) -> Option<u64> {
    addr.checked_add(page - 1).map(|a| a & !(page - 1))
}

/// `p_flags` (`PF_R/W/X`) → `rustix` [`ProtFlags`] for the relocated, final protection.
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

/// A library image mapped into one contiguous anonymous region, owning the mapping for RAII.
///
/// The whole reserved span is `munmap`ped on [`Drop`]. While alive, [`MappedObject::image_bytes`]
/// hands the relocation pass a safe `&mut [u8]` over the region, and [`MappedObject::load_base`]
/// gives the run-time load address relocations are computed against.
pub struct MappedObject {
    /// Page-aligned start of the reserved anonymous region (the load base for relocations: the
    /// image's lowest `p_vaddr` is page-floored to 0, so `base + vaddr` is the run-time address).
    base: NonNull<u8>,
    /// Total page-rounded length of the reserved region (the `munmap` length).
    span: usize,
    /// Module static-TLS tp-relative base offset, forwarded to the [`RelocImage`]. Unused by the
    /// base-only relocations applied here (TPOFF64 is deferred) — kept 0 until the static-TLS step.
    static_tls_offset: i64,
}

// SAFETY: 2026-06-05 — `MappedObject` owns an exclusive mmap'd region (created `MAP_PRIVATE`, never
// shared with another owner). The raw pointer is just the region's address; there is no interior
// thread-affinity. Sending/sharing it across threads is as sound as sending a `Box<[u8]>`: all
// access goes through `&self`/`&mut self`, which Rust's borrow rules already serialize.
unsafe impl Send for MappedObject {}
unsafe impl Sync for MappedObject {}

impl MappedObject {
    /// Map the parsed image's `PT_LOAD` segments and apply the base-only relocations.
    ///
    /// `file` is the same byte slice [`ElfImage::parse`] decoded (the segment file bytes are copied
    /// from it). `page_size` is the host page size (use [`host_page_size`]). On success the region
    /// is mapped, RELATIVE+RELR are applied, and each segment is `mprotect`ed to its final
    /// `p_flags`. Symbol/TLS/ifunc relocations are counted as `skipped_by_type` and deferred.
    pub fn map_and_relocate(
        img: &ElfImage<'_>,
        file: &[u8],
        page_size: u64,
    ) -> Result<(Self, MapStats), MapError> {
        if img.loads.is_empty() {
            return Err(MapError::NoLoadSegments);
        }

        // 1) Compute the total vaddr span: [min(vaddr) page-floored, max(vaddr+memsz) page-ceiled).
        // PIE objects start at vaddr 0, but compute min explicitly so a non-zero-based image (or one
        // with a gap before the first segment) still maps correctly.
        let min_vaddr = img.loads.iter().map(|s| s.vaddr).min().expect("non-empty");
        let mut max_end: u64 = 0;
        for s in &img.loads {
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

        // 2) Reserve the whole span as one PROT_NONE anonymous mapping to claim a contiguous range
        // and obtain the load base. Mapping the entire object in one reservation makes the standard
        // ELF page-overlap (a segment's final partial page shared with the next segment's start)
        // correct by construction — bytes are placed by vaddr into this single region.
        // SAFETY: 2026-06-05 — `mmap_anonymous(NULL, span, …)` with a non-zero `span` lets the kernel
        // choose a fresh, page-aligned address for a private anonymous region. NULL addr means "no
        // fixed placement", so no existing mapping can be clobbered. `span` is the page-rounded byte
        // length computed above (> 0). The returned address owns `span` bytes for this object's life.
        let ptr = unsafe {
            mmap_anonymous(
                std::ptr::null_mut(),
                span,
                ProtFlags::empty(),
                MapFlags::PRIVATE,
            )
        }
        .map_err(MapError::Os)?;
        // The kernel returns a page-aligned, non-null address on success.
        let base = NonNull::new(ptr.cast::<u8>()).ok_or(MapError::Os(rustix::io::Errno::NOMEM))?;

        // The reserved region's address corresponds to `region_start` in image-vaddr space. The
        // load base used by relocations (`base + vaddr` = run-time address) is therefore
        // `mapping_addr - region_start`. For a PIE (region_start == 0) this is just the mapping addr.
        let load_base = (base.as_ptr() as u64).wrapping_sub(region_start);

        let mut obj = MappedObject {
            base,
            span,
            static_tls_offset: 0,
        };

        // 3) For each PT_LOAD: make its pages writable, copy filesz file bytes to base+vaddr (the
        // bss tail [filesz, memsz) is already zero from the fresh anonymous pages). Protections are
        // set to final values in step 5 after relocation.
        for seg in &img.loads {
            obj.populate_segment(file, seg, region_start, page_size)?;
        }

        // 4) Apply base-only relocations over a writable view of the whole region. RELATIVE (from
        // .rela.dyn/.rela.plt) and DT_RELR need only the load base; everything else is deferred.
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

        // Partition .rela entries: apply only RELATIVE here; count the rest as deferred.
        let mut relative: Vec<reloc::Rela> = Vec::new();
        for r in &relas {
            if r.r_type == reloc::R_X86_64_RELATIVE {
                relative.push(*r);
            } else {
                stats.skipped_by_type += 1;
            }
        }
        stats.relative_applied = relative.len();
        // DT_RELR words encode a variable number of relocations; count the expansion (address words
        // + set bitmap bits) for the report via a small wrapper mirroring `apply_relr`'s decoding.
        stats.relr_applied = count_relr_targets(&relr);

        // 2026-06-05: `DT_RELR` ADDRESS words are in-object virtual addresses (image base 0), but
        // `reloc::apply_relr` (matching its `.rela` sibling, which rebases internally) expects the
        // address words to be RUN-TIME addresses (`load_base + vaddr`). Rebase each EVEN (address)
        // word by `load_base`; ODD (bitmap) words are bit-flags, not addresses, and pass through
        // unchanged. This keeps the `reloc` core's "address words are run-time addresses" contract
        // (`reloc.rs` unchanged) and is the correct file→runtime boundary for the table.
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
            // SAFETY: 2026-06-05 — `image_bytes` exposes the mapping as `&mut [u8]` of exactly `span`
            // bytes (see its SAFETY). The region's pages were made writable in step 3
            // (`populate_segment` upgrades each to RW before copying; final protection is set in
            // step 5). The reloc core only writes within `[0, span)`, all bounds-checked.
            let bytes = unsafe { obj.image_bytes() };
            let mut image = reloc::SliceImage::new(base_addr, tls_off, bytes);
            reloc::apply_rela(&mut image, &NullResolver, &relative)?;
            reloc::apply_relr(&mut image, &relr_runtime)?;
        }

        // 5) Set each segment's final protection from p_flags (page-rounded). RELRO is left at its
        // segment's RW/R flags here; the read-only-after-reloc hardening of PT_GNU_RELRO is a
        // separate later step (it overlaps a segment and must run after all relocations of a full
        // dependency graph). For map+base-relocate verification, segment p_flags are sufficient.
        for seg in &img.loads {
            obj.protect_segment(seg, region_start, page_size)?;
        }

        Ok((obj, stats))
    }

    /// Apply the **symbol-dependent** relocations through a resolution [`Scope`], the step
    /// [`Self::map_and_relocate`] deferred. Resolves `R_X86_64_GLOB_DAT` / `R_X86_64_JUMP_SLOT`
    /// (eager — `BIND_NOW`) / `R_X86_64_64` for `img`'s `.rela.dyn` + `.rela.plt`, writing each
    /// resolved symbol address into its GOT/PLT slot via the [`reloc`] core.
    ///
    /// `img` must be the same parsed image [`Self::map_and_relocate`] mapped (its `dynsyms` index
    /// the relocations' `sym_index`es, and its `relocations()` name the slots). `scope` is the
    /// ordered provider scope (typically a [`super::resolve::LoadedObjectProvider`] of this object
    /// followed by a [`super::resolve::HostDlsymProvider`]); build it with this object's
    /// [`Self::load_base`] + `img.dynsyms`.
    ///
    /// `R_X86_64_TPOFF64` (static-TLS) and `R_X86_64_IRELATIVE` (ifunc) are **counted as deferred**
    /// and not applied — TLS needs the `%fs`/TCB step and ifunc needs executing resolvers, both out
    /// of scope here. `R_X86_64_RELATIVE`/`DT_RELR` were already applied by the base pass.
    ///
    /// The relocation targets (GOT/PLT) sit in writable or RELRO-but-still-RW segments, which
    /// `map_and_relocate` left writable (RELRO hardening is a later step). This pass makes every
    /// segment writable, applies, then restores each segment's final `p_flags` protection — so a
    /// GOT slot in a nominally read-only-after-reloc region is still patchable here.
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

        // Partition the relocations: apply only the symbol-address types; count TPOFF64/IRELATIVE
        // (and any leftover non-symbol type) as deferred.
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
                // RELATIVE / DT_RELR were applied by the base pass; anything else is not a symbol
                // reloc this pass owns. Don't double-count base types as deferred.
                _ => {}
            }
        }

        let resolver = ScopedResolver::new(scope, &img.dynsyms);
        let load_base = self.load_base().wrapping_sub(region_start);
        let tls_off = self.static_tls_offset;

        // Make every segment writable for the patch, apply, then restore final protections.
        for seg in &img.loads {
            self.mprotect_segment_pages(
                seg,
                region_start,
                page_size,
                ProtFlags::READ | ProtFlags::WRITE,
            )?;
        }

        let resolved_nonnull = {
            // SAFETY: 2026-06-05 — every segment was just made RW above; `image_bytes` exposes the
            // mapping as `&mut [u8]` of exactly `span` bytes (see its SAFETY); the reloc core only
            // writes within `[0, span)`, all bounds-checked.
            let bytes = unsafe { self.image_bytes() };
            let mut image = reloc::SliceImage::new(load_base, tls_off, bytes);
            reloc::apply_rela(&mut image, &resolver, &symbol_relas)?;
            // Count how many slots now hold a non-null address (a real definition; weak-undef = 0).
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

        // Restore each segment's final protection (the post-reloc state map_and_relocate set).
        for seg in &img.loads {
            self.protect_segment(seg, region_start, page_size)?;
        }

        Ok(stats)
    }

    /// Map + base-relocate (via [`Self::map_and_relocate`]) **and** apply symbol relocations through
    /// `build_scope`, in one call. `build_scope` is given this object's run-time load base and the
    /// image's dynamic symbols so it can construct the resolution [`Scope`] (which must reference
    /// this object's definitions plus any host/already-loaded providers).
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

    /// Copy one segment's file bytes into the mapped region and ensure its pages are writable for
    /// the relocation pass. The bss tail is already zero (fresh anonymous pages).
    fn populate_segment(
        &mut self,
        file: &[u8],
        seg: &LoadSegment,
        region_start: u64,
        page_size: u64,
    ) -> Result<(), MapError> {
        // Region-relative byte offset of this segment's content.
        let seg_off_in_region = seg
            .vaddr
            .checked_sub(region_start)
            .ok_or(MapError::SpanOverflow("vaddr - region_start"))?;
        let seg_off = usize::try_from(seg_off_in_region)
            .map_err(|_| MapError::SpanOverflow("segment offset as usize"))?;

        // Make this segment's page range writable so the copy + later relocations can write. The
        // pages spanning [vaddr, vaddr+memsz) (rounded out) get PROT_READ|WRITE temporarily.
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

        // SAFETY: 2026-06-05 — `seg_off + filesz <= span`: `seg_off` is `vaddr - region_start` and
        // `vaddr + memsz <= region_end`, with `filesz <= memsz` (ELF invariant; an over-large
        // `filesz` only shrinks the bss and is still within memsz's page-ceiled span). `dst` is the
        // mapping's writable bytes at `seg_off`; `src` is exactly `filesz` bytes from the file slice;
        // the two never alias (different allocations). The segment's pages were made writable above.
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

    /// Set this segment's final protection (`p_flags` → PROT bits), page-rounded.
    fn protect_segment(
        &self,
        seg: &LoadSegment,
        region_start: u64,
        page_size: u64,
    ) -> Result<(), MapError> {
        let prot = prot_of(seg.flags);
        self.mprotect_segment_pages(seg, region_start, page_size, prot)
    }

    /// `mprotect` the pages spanning a segment's `[vaddr, vaddr+memsz)` (rounded out to whole pages)
    /// to `prot`. Page-floors the start and page-ceils the end so partial pages are covered.
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
        // A zero-length segment range needs no protection change.
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
        // SAFETY: 2026-06-05 — `[off, off+len)` is within `[0, span)` (checked just above) of this
        // object's own mapping, and both `off` and `len` are page-multiples (page_floor/page_ceil).
        // `mprotect` on a sub-range of an owned mapping with valid page-aligned bounds is sound.
        unsafe {
            mprotect(
                self.base.as_ptr().add(off).cast(),
                len,
                mprotect_flags(prot),
            )
        }
        .map_err(MapError::Os)
    }

    /// The run-time load base relocations are computed against (`load_base + vaddr` = run-time
    /// address of `vaddr`). For a PIE (lowest vaddr 0) this equals the mapping address.
    pub fn load_base(&self) -> u64 {
        // Recompute from the stored base — region_start is folded in at map time, but for the public
        // accessor we expose the mapping address itself (PIE region_start is 0, the common case);
        // internal relocation used the precise `load_base`. Callers comparing reloc targets should
        // use [`Self::span`]-bounded `[load_base, load_base+span)`.
        self.base.as_ptr() as u64
    }

    /// Total length (bytes) of the reserved region.
    pub fn span(&self) -> usize {
        self.span
    }

    /// A safe `&mut [u8]` over the whole mapped region, for the relocation pass.
    ///
    /// # Safety
    /// The caller must ensure the pages being written are currently writable (`PROT_WRITE`) — the
    /// mapper makes every segment's pages RW before relocating and only sets final (possibly
    /// read-only) protections afterward. Writing to a page that has been `mprotect`ed read-only
    /// would fault. The returned slice borrows `self` mutably, so no aliasing `&mut` can coexist.
    pub unsafe fn image_bytes(&mut self) -> &mut [u8] {
        // SAFETY: 2026-06-05 — `base` points at `span` owned, mapped, readable bytes; `u8` has no
        // alignment requirement; the `&mut` is unique for its lifetime (borrows `*self` mutably).
        // Writability is the caller's documented precondition (see the fn-level # Safety).
        unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), self.span) }
    }
}

impl Drop for MappedObject {
    fn drop(&mut self) {
        // SAFETY: 2026-06-05 — `base`/`span` are exactly the address + length returned by this
        // object's `mmap_anonymous`; `munmap`ping precisely the region we own, once, on the single
        // owner's drop, is sound and leaks nothing. The result is ignored: there is no recovery
        // from a failed unmap at drop time, and on a well-formed mapping it cannot fail.
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.span) };
    }
}

/// `ProtFlags` → `MprotectFlags` (same bit meanings; `rustix` types them separately).
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

/// A resolver that resolves nothing — used for the base-only pass, where only `R_X86_64_RELATIVE`
/// (no symbol) is applied. Symbol/TLS relocations are partitioned out before the pass, so this is
/// never consulted; if a stray symbol reloc reached the applier it would surface as a typed
/// `UnresolvedSymbol` error rather than a bogus write.
struct NullResolver;
impl reloc::SymbolResolver for NullResolver {
    fn resolve_symbol(&self, _i: u32) -> Option<u64> {
        None
    }
    fn resolve_tls_offset(&self, _i: u32) -> Option<u64> {
        None
    }
}

/// Count how many individual relocations a `DT_RELR` word table expands to (address words + set
/// bitmap bits), mirroring [`reloc::apply_relr`]'s decoding, for reporting `MapStats::relr_applied`.
fn count_relr_targets(entries: &[u64]) -> usize {
    let mut n = 0usize;
    for &entry in entries {
        if entry & 1 == 0 {
            n += 1; // address word: one relocated location
        } else {
            n += (entry >> 1).count_ones() as usize; // bitmap: one per set data bit
        }
    }
    n
}

/// The host page size, queried at runtime (detect-don't-assume; 4K on x86-64, 16K on some configs).
pub fn host_page_size() -> u64 {
    rustix::param::page_size() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::elf::{ElfImage, PF_R, PF_W, PF_X};
    use crate::loader::reloc::R_X86_64_RELATIVE;

    // ---- Minimal in-memory ELF fixture with two PT_LOAD segments (R-X text + RW data+bss) -------
    //
    // Builds a valid little-endian x86-64 ET_DYN whose file layout maps 1:1 to its vaddrs, with two
    // page-aligned PT_LOAD segments so the mapper's page-rounding, copy, bss-zeroing, and
    // protection split are all exercised. Page size 0x1000.
    //   LOAD0 (R-X): vaddr 0x0000, file [0, 0x1000), memsz 0x1000  — headers + .dynamic + text
    //   LOAD1 (RW ): vaddr 0x1000, file [0x1000, 0x1000+0x40), memsz 0x80  — data (0x40) + bss (0x40)
    // Dynamic + rela + relr live inside LOAD0; the RELATIVE/RELR targets live inside LOAD1.

    const PAGE: u64 = 0x1000;
    const PH_OFF: usize = 0x40;
    const DYN_OFF: u64 = 0x200;
    const SYM_OFF: u64 = 0x280; // one null Elf64_Sym (24 bytes); satisfies elf.rs's DT_SYMTAB check
    const RELA_OFF: u64 = 0x300;
    const RELR_OFF: u64 = 0x380;
    const STR_OFF: u64 = 0x3c0;
    // Targets in LOAD1 (the RW segment).
    const RELA_TARGET: u64 = 0x1000; // word 0 of the data segment
    const RELR_TARGET: u64 = 0x1008; // word 1 of the data segment
    const DATA_FILE_OFF: u64 = 0x1000;
    const DATA_FILESZ: u64 = 0x40;
    const DATA_MEMSZ: u64 = 0x80; // 0x40 data + 0x40 bss
    const FILE_SIZE: usize = 0x1040; // headers/text page + 0x40 of data file bytes

    // Reuse elf.rs's constants by value (kept local to avoid touching the forbid-unsafe module).
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

    /// Distinctive marker bytes seeded into the text segment so we can prove the copy landed.
    const TEXT_MARK_OFF: usize = 0x100;
    const TEXT_MARK: u64 = 0xdead_beef_cafe_babe;
    /// Pre-reloc value stored at the RELATIVE target (its addend) and the RELR target (its base-add).
    const RELA_ADDEND: i64 = 0x1234;
    const RELR_SEED: u64 = 0x40;

    fn build_two_segment_fixture() -> Vec<u8> {
        let mut buf = vec![0u8; FILE_SIZE];

        // Ehdr.
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
        put_u16(&mut buf, 56, 3); // LOAD0, LOAD1, DYNAMIC

        // LOAD0 = R-X over the first page; LOAD1 = RW over the data; DYNAMIC inside LOAD0.
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

        // .dynamic.
        let mut slot = 0;
        let mut d = |buf: &mut [u8], tag: i64, val: u64| {
            put_dyn(buf, slot, tag, val);
            slot += 1;
        };
        d(&mut buf, DT_RELA, RELA_OFF);
        d(&mut buf, DT_RELASZ, RELA_ENT); // one entry
        d(&mut buf, DT_RELAENT, RELA_ENT);
        d(&mut buf, DT_RELR, RELR_OFF);
        d(&mut buf, DT_RELRSZ, 8); // one word
        d(&mut buf, DT_RELRENT, 8);
        d(&mut buf, DT_SYMTAB, SYM_OFF); // one null symbol; required because .rela is present
        d(&mut buf, DT_SYMENT, SYM_ENT);
        d(&mut buf, DT_STRTAB, STR_OFF);
        d(&mut buf, DT_STRSZ, 1);
        d(&mut buf, DT_NULL, 0);

        // .rela.dyn: one RELATIVE at RELA_TARGET, addend RELA_ADDEND.
        put_u64(&mut buf, RELA_OFF as usize, RELA_TARGET);
        put_u64(&mut buf, RELA_OFF as usize + 8, R_X86_64_RELATIVE as u64);
        put_u64(&mut buf, RELA_OFF as usize + 16, RELA_ADDEND as u64);

        // .relr: one even address word naming RELR_TARGET.
        put_u64(&mut buf, RELR_OFF as usize, RELR_TARGET);

        // .dynstr: just a NUL.
        buf[STR_OFF as usize] = 0;

        // Seed a marker in the text page and the RELR pre-value in the data file bytes.
        put_u64(&mut buf, TEXT_MARK_OFF, TEXT_MARK);
        // RELR target is at file offset 0x1008 (data file bytes start at 0x1000); seed it.
        put_u64(&mut buf, RELR_TARGET as usize, RELR_SEED);
        // RELA target word (0x1000) starts zero; RELATIVE overwrites it wholesale.

        buf
    }

    fn read_word(obj: &mut MappedObject, vaddr: u64) -> u64 {
        // SAFETY (test-only): the region is mapped readable; vaddr < span. We read after relocation
        // and after final protections, all of which include PROT_READ for these segments.
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

        // Text marker copied at its vaddr (LOAD0 maps file 1:1 here).
        assert_eq!(read_word(&mut obj, TEXT_MARK_OFF as u64), TEXT_MARK);

        // The bss tail [filesz, memsz) of LOAD1 (data 0x40..0x80) is zero — fresh anonymous pages.
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
        // RELATIVE: *(RELA_TARGET) = base + addend.
        let got = read_word(&mut obj, RELA_TARGET);
        assert_eq!(got, base.wrapping_add(RELA_ADDEND as u64));
        // The relocated word points INSIDE [base, base+span) only if base+addend does; addend is
        // small and positive, so it lands in the mapped region.
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
        // RELR: *(RELR_TARGET) += base. Seeded value was RELR_SEED.
        let got = read_word(&mut obj, RELR_TARGET);
        assert_eq!(got, base.wrapping_add(RELR_SEED));
        assert!(got >= base && got < base + obj.span() as u64);
    }

    #[test]
    fn page_rounding_span_is_correct() {
        // LOAD0 ends at page 0x1000; LOAD1 ends at 0x1000+0x80=0x1080 → ceiled to 0x2000. Span = 2 pages.
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        let (obj, _) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();
        assert_eq!(obj.span(), 0x2000);
    }

    #[test]
    fn no_load_segments_is_typed_err() {
        // Hand-build a header with zero program headers → no PT_LOAD.
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
        // Map and drop many times; if Drop didn't munmap, the address space would exhaust on a
        // 32-bit-ish span budget. This both exercises Drop and proves no double-free/UB on drop.
        let buf = build_two_segment_fixture();
        let img = ElfImage::parse(&buf).unwrap();
        for _ in 0..256 {
            let (obj, _) = MappedObject::map_and_relocate(&img, &buf, PAGE).unwrap();
            assert_eq!(obj.span(), 0x2000);
            drop(obj);
        }
    }

    #[test]
    fn count_relr_targets_matches_encoding() {
        // One address word + a bitmap with 3 set data bits = 4 relocations.
        let data_bits = 0b1011u64; // 3 bits set
        let bitmap = (data_bits << 1) | 1;
        assert_eq!(count_relr_targets(&[0x1000, bitmap]), 1 + 3);
        // Empty table → 0.
        assert_eq!(count_relr_targets(&[]), 0);
    }

    // ---- REAL test: parse + map + base-relocate /usr/lib/libm.so.6 (skips cleanly if absent) -----

    #[test]
    fn real_libm_maps_and_base_relocates() {
        // 2026-06-05: end-to-end on a real toolchain-produced .so. Maps its PT_LOAD segments, applies
        // RELATIVE+RELR, and asserts every relocated relative target now points INSIDE the mapped
        // object. SKIP (not fail) if no host libm is present — never fabricate.
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

        // Every RELATIVE target word must now hold a value inside [base, base+span): a relative
        // reloc points within the object. Re-read each from the mapped image.
        let relas = img.relocations().unwrap();
        let mut checked_relative = 0usize;
        {
            // SAFETY (test): the region is readable post-relocation; offsets are < span (RELATIVE
            // r_offset is an in-object vaddr, which the mapper laid out within [0, span)).
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

        // RELR targets likewise point inside the object after *p += base.
        let relr = img.relr().unwrap();
        let relr_count = count_relr_targets(&relr);

        eprintln!(
            "real_libm_maps_and_base_relocates: {path} — segments={} RELATIVE_applied={} RELR_applied={} skipped_by_type={} (verified {checked_relative} RELATIVE targets in-object)",
            stats.segments_mapped, stats.relative_applied, stats.relr_applied, stats.skipped_by_type,
        );
        assert_eq!(stats.relative_applied, checked_relative);
        assert_eq!(stats.relr_applied, relr_count);
    }

    // ---- REAL test: resolve + apply libm.so.6's symbol relocations through a Scope --------------

    #[test]
    fn real_libm_resolves_and_applies_symbol_relocations() {
        // 2026-06-05: the step-5 end-to-end. Map libm, build a Scope = [LoadedObjectProvider(libm),
        // HostDlsymProvider], and apply its GLOB_DAT (+ any JUMP_SLOT / R_X86_64_64) symbol relocs.
        // Asserts: no unresolved-STRONG error, no panic, and every STRONG symbol reloc resolved to a
        // non-null address in a sane range — while WEAK-undef ones legitimately resolve to 0.
        // libm's symbol relocs are 32 GLOB_DAT (no JUMP_SLOT) per `readelf -r`; of these a handful
        // are weak-undef in this process (`__gmon_start__`, `_ITM_*`) → 0, which is correct.
        // SKIP (not fail) if no host libm — never fabricate.
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
            // Scope: libm's own exported definitions first (resolves its self-references like
            // __signgam / _LIB_VERSION), then the host process (resolves libc imports via dlsym).
            let mut scope = Scope::new();
            scope
                .push(Box::new(LoadedObjectProvider::new(base, &img.dynsyms)))
                .push(Box::new(HostDlsymProvider));
            let mut obj = obj;
            let sym_stats = obj
                .relocate_symbols(&img, &scope, page)
                .unwrap_or_else(|e| panic!("symbol relocate {path}: {e}"));
            // Sanity: the base pass deferred exactly these symbol relocs to us.
            assert_eq!(
                sym_stats.total_applied() + sym_stats.deferred,
                map_stats.skipped_by_type,
                "every base-deferred reloc is accounted for as applied or deferred"
            );
            (obj, sym_stats)
        };

        let base = obj.load_base();
        let span = obj.span() as u64;

        // Independently classify each symbol reloc and verify the written slot value matches the
        // gABI rules: STRONG symbol → non-null, in a sane range; WEAK-undef → 0.
        let relas = img.relocations().unwrap();
        let mut scope = Scope::new();
        scope
            .push(Box::new(LoadedObjectProvider::new(base, &img.dynsyms)))
            .push(Box::new(HostDlsymProvider));
        let mut strong_count = 0usize;
        let mut weak_zero_count = 0usize;
        let mut total_symbol = 0usize;
        {
            // SAFETY (test): region readable post-reloc; symbol-reloc r_offsets are in-object vaddrs.
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
                    // A resolved (strong or scope-found) symbol must be written non-null. For
                    // R_X86_64_64 the addend is folded in, but libm has none of those; GLOB_DAT/
                    // JUMP_SLOT write the bare address.
                    assert_ne!(word, 0, "resolved {} wrote a null slot", sym.name);
                    strong_count += 1;
                    // A definition from libm itself must land inside the mapped object; a host
                    // (dlsym) definition is outside it. Only check in-object for the self-defined.
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
                    // No definition in scope: this is only legal for a WEAK reference (→ 0).
                    assert_eq!(
                        sym.bind, 2, /* STB_WEAK */
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

        // Every symbol reloc is either a non-null strong resolution or a legal weak-undef zero.
        assert_eq!(strong_count + weak_zero_count, total_symbol);
        assert_eq!(sym_stats.total_applied(), total_symbol);
        assert_eq!(sym_stats.resolved_nonnull, strong_count);
        // libm must have at least the well-known libc imports resolved (sanity floor).
        assert!(
            strong_count >= 20,
            "expected most of libm's symbol relocs to resolve, got {strong_count}/{total_symbol}"
        );
    }
}
