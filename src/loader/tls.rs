//! Pure-Rust x86-64 variant-II static-TLS layout + `R_X86_64_TPOFF64` offset resolution — the
//! loader's fourth piece (the static-TLS block + thread-pointer-relative offsets).
//!
//! 2026-06-05: [`map`](super::map) maps a `.so`, applies the base relocations
//! (`R_X86_64_RELATIVE`/`DT_RELR`) and — via [`resolve`](super::resolve) — the symbol relocations
//! (`GLOB_DAT`/`JUMP_SLOT`/`R_X86_64_64`). The one remaining non-ifunc relocation class is
//! `R_X86_64_TPOFF64` (the `unknown reloc type 18` the apkenv linker aborts on): a static-TLS,
//! thread-pointer-relative offset. This module supplies the missing piece: the **static-TLS block
//! layout** that assigns each module a tp-relative base offset, and a [`reloc::SymbolResolver`]
//! ([`TlsResolver`]) that turns a TLS symbol into the tp-relative value `TPOFF64` writes.
//!
//! ## Clean-room provenance
//! Every rule below is from the **public** ELF / x86-64 psABI Thread-Local-Storage specification
//! (the "variant II" TLS model) — general, well-established public knowledge. No dynamic-linker or
//! libc source was read. The variant-II facts used here:
//! - The **thread pointer** (TP; `%fs.base` on x86-64) points at the thread-control block (TCB).
//! - Each module's static-TLS block sits **below** TP, so a symbol's tp-relative offset is
//!   **negative**.
//! - Blocks are stacked by accumulating each module's **aligned** size: with the first module's
//!   block nearest TP, `offset_1 = roundup(size_1, align_1)`,
//!   `offset_2 = offset_1 + roundup(size_2, align_2)`, … . Module `i`'s block occupies
//!   `[TP - offset_i, TP - offset_i + size_i)`.
//! - A TLS symbol's tp-relative value is `-offset_i + st_value` (`st_value` is the symbol's byte
//!   offset within its module's TLS block). `R_X86_64_TPOFF64` writes `S_tp + A` (this value plus
//!   the relocation addend); [`reloc::apply_one`] adds the addend.
//!
//! ## What this module delivers — and the HONEST boundary of what it does NOT (2026-06-05)
//! This module computes the **layout, the per-module tp-offset, the per-symbol tp-relative value**,
//! assembles the **initialization block** in Eclipse-owned memory (`.tdata` copied, `.tbss` zeroed,
//! correctly aligned), and applies `R_X86_64_TPOFF64` through [`reloc`]. The computed offsets and
//! assembled block are correct per the psABI.
//!
//! **It does NOT bind the block to a live thread pointer.** For the offsets to be *reachable at
//! runtime*, the assembled block must be placed at `[TP - offset_i, …)` for a real TP (`%fs`/TCB).
//! Eclipse runs on glibc, which **owns** the main thread's `%fs` and its static-TLS area, so wiring
//! Eclipse-loaded modules' blocks to the live TP is a **separate integration step** with real
//! tradeoffs, none of which this module makes:
//! - **(a) glibc static-TLS surplus** — place blocks in the spare static-TLS the glibc loader
//!   reserves (`dl_tls_static_surplus`); bounded size, but reuses the host TP directly.
//! - **(b) a private TCB** — allocate our own TCB + blocks and swap `%fs` at call boundaries into
//!   the loaded code; full control, but every crossing must save/restore `%fs`.
//! - **(c) dynamic TLS** — resolve via `__tls_get_addr` / dynamic thread vector instead of static
//!   offsets; most general, not the static-TLS fast path `TPOFF64` encodes.
//!
//! This module therefore intentionally **does not** modify `%fs`, set up a TCB, or execute the
//! loaded code. It provides the layout/offset math + `TPOFF64` application + tests. Wiring it to a
//! live `%fs` is the documented follow-on (AGENTS.md §5).
//!
//! ## Cross-module TLS imports (why a layout spans modules)
//! A `TPOFF64` relocation frequently references a TLS symbol **defined in another module** — e.g.
//! `libm.so.6`'s only `TPOFF64` references `errno`, which is `TLS GLOBAL UND` in libm and
//! **defined** in `libc.so.6`'s `PT_TLS` (libm has no `PT_TLS` of its own). So the offset is not
//! "libm's own block + st_value"; it is the offset of `errno` **within libc's** static-TLS block.
//! A [`TlsLayout`] therefore lays out *one or more* modules and indexes every module's **defined**
//! TLS symbols by name, so a `TPOFF64` against an imported TLS symbol resolves to the defining
//! module's tp-relative value — exactly mirroring [`resolve`]'s cross-module symbol scope.
//!
//! ## Safety
//! `#![forbid(unsafe_code)]`. The assembled block is a plain `Vec<u8>`; all layout arithmetic is
//! checked. Like `reloc.rs`/`elf.rs`, this is benign data computation — it maps/executes nothing.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use super::elf::{DynSym, TlsSegment};
use super::reloc::SymbolResolver;

/// `st_info & 0xf` value `STT_TLS`: a thread-local-storage symbol (its `st_value` is an offset
/// within the module's TLS block, not a load address). Public System V gABI value.
const STT_TLS: u8 = 6;
/// `st_shndx` value `SHN_UNDEF`: an undefined (imported) symbol — not a definition.
const SHN_UNDEF: u16 = 0;

/// Round `value` up to a multiple of `align` (a power of two, or 0/1 meaning "no alignment").
/// Returns `None` on overflow. Public alignment identity: `roundup(v, a) = (v + a - 1) & !(a - 1)`.
fn round_up(value: u64, align: u64) -> Option<u64> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

/// Errors from building or querying a static-TLS layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    /// A module's `PT_TLS` `p_align` was not a power of two (variant-II layout requires a
    /// power-of-two alignment to stack blocks). Carries the offending alignment.
    BadAlign(u64),
    /// A module's `PT_TLS` `p_filesz` exceeded its `p_memsz` (the `.tdata` image cannot be larger
    /// than the whole TLS block). Carries `(file_size, mem_size)`.
    FileLargerThanMem(u64, u64),
    /// The accumulated layout size or an offset computation overflowed. Carries a short description.
    Overflow(&'static str),
    /// A module's `PT_TLS` `.tdata` bytes `[file_offset, +file_size)` are not present in the file
    /// slice. Carries the segment's `file_offset`.
    TdataOutOfFile(u64),
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadAlign(a) => write!(f, "PT_TLS p_align {a} is not a power of two"),
            Self::FileLargerThanMem(fz, mz) => {
                write!(f, "PT_TLS p_filesz {fz} exceeds p_memsz {mz}")
            }
            Self::Overflow(what) => write!(f, "static-TLS layout arithmetic overflowed: {what}"),
            Self::TdataOutOfFile(off) => {
                write!(
                    f,
                    "PT_TLS .tdata at file offset {off:#x} is past the file bytes"
                )
            }
        }
    }
}

impl std::error::Error for TlsError {}

/// One module's placement within a [`TlsLayout`]: its tp-relative base offset and the byte range it
/// occupies inside the assembled initialization block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsModule {
    /// The module's tp-relative base offset (**negative**: the block sits below the thread pointer).
    /// A defined TLS symbol at within-block `st_value` has tp-relative value `tp_offset + st_value`.
    pub tp_offset: i64,
    /// Byte offset of this module's block within the assembled [`TlsLayout::init_block`].
    pub block_offset: usize,
    /// Size of this module's TLS block in bytes (`p_memsz`).
    pub size: usize,
}

/// A static-TLS layout over one or more modules (variant II). It stacks each module's block below
/// the thread pointer by accumulating aligned sizes, assembles the initialization block
/// (`.tdata` copied + `.tbss` zeroed, correctly aligned), records each module's [`TlsModule`]
/// placement, and indexes every module's **defined** TLS symbols by name so a `TPOFF64` against an
/// imported TLS symbol resolves to the defining module's tp-relative value.
///
/// See the module docs for the variant-II rules and the runtime `%fs`-binding boundary.
#[derive(Debug, Clone, Default)]
pub struct TlsLayout {
    /// The assembled TLS init image: each module's `.tdata` placed at its `block_offset`, `.tbss`
    /// left zero. This is the template a future `%fs`-binding step would copy below the live TP.
    init_block: Vec<u8>,
    /// Per-module placement, in the order modules were added.
    modules: Vec<TlsModule>,
    /// Running accumulated offset from TP (the largest `offset_i` so far); the next module's
    /// `tp_offset` is `-(accumulated + roundup(size, align))`.
    accumulated: u64,
    /// name → (tp_offset of the defining module, within-block `st_value`) for every **defined** TLS
    /// symbol. A symbol's tp-relative value is `tp_offset + st_value`. Used by [`TlsResolver`].
    tls_defs: HashMap<String, (i64, u64)>,
}

impl TlsLayout {
    /// An empty layout (no modules; resolves no TLS symbol).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a module's `PT_TLS` template + its dynamic symbols to the layout, returning the assigned
    /// [`TlsModule`] placement.
    ///
    /// `tls` is the module's [`TlsSegment`] (`vaddr`/`file_size`/`mem_size`/`align`); `file` is the
    /// module's raw file bytes (the same slice [`super::elf::ElfImage::parse`] decoded); `tdata_off`
    /// is the file offset of the `.tdata` initialization image (the caller converts the segment's
    /// `vaddr` via [`super::elf::ElfImage::vaddr_to_off`] — `TlsSegment` records only the vaddr,
    /// and the file offset comes from the `PT_LOAD` mapping). `dynsyms` are the module's decoded
    /// symbols, used to index its **defined** TLS symbols (`STT_TLS`, `st_shndx != SHN_UNDEF`) by
    /// name.
    ///
    /// Variant-II stacking: this block's `offset_i = accumulated + roundup(mem_size, align)`, its
    /// `tp_offset = -offset_i`, and it occupies `[block_offset, block_offset + mem_size)` within the
    /// assembled init block (with `block_offset` aligned to `align`). The `.tdata` bytes are copied
    /// in; the `.tbss` tail stays zero. Returns [`TlsError`] on a bad alignment / overflow / missing
    /// `.tdata` bytes.
    pub fn add_module(
        &mut self,
        tls: &TlsSegment,
        file: &[u8],
        tdata_off: u64,
        dynsyms: &[DynSym],
    ) -> Result<TlsModule, TlsError> {
        if tls.align > 1 && !tls.align.is_power_of_two() {
            return Err(TlsError::BadAlign(tls.align));
        }
        if tls.file_size > tls.mem_size {
            return Err(TlsError::FileLargerThanMem(tls.file_size, tls.mem_size));
        }

        // Variant II: the new block's distance from TP is the running total plus this block's
        // aligned size. Align the running total up first so the block itself is aligned relative to
        // TP, then add its aligned size — both keep every block's start at a multiple of its align.
        let aligned_acc = round_up(self.accumulated, tls.align)
            .ok_or(TlsError::Overflow("accumulated alignment"))?;
        let aligned_size =
            round_up(tls.mem_size, tls.align).ok_or(TlsError::Overflow("module size alignment"))?;
        let offset_i = aligned_acc
            .checked_add(aligned_size)
            .ok_or(TlsError::Overflow("offset accumulation"))?;
        let tp_offset =
            -(i64::try_from(offset_i).map_err(|_| TlsError::Overflow("offset as i64"))?);

        // Place this module's bytes in the assembled init block. Lay it out contiguously after the
        // current block, aligning the start so the assembled image mirrors the tp-relative layout.
        let block_offset = round_up(self.init_block.len() as u64, tls.align)
            .ok_or(TlsError::Overflow("block_offset alignment"))?;
        let block_offset = usize::try_from(block_offset)
            .map_err(|_| TlsError::Overflow("block_offset as usize"))?;
        let mem_size =
            usize::try_from(tls.mem_size).map_err(|_| TlsError::Overflow("mem_size as usize"))?;
        let block_end = block_offset
            .checked_add(mem_size)
            .ok_or(TlsError::Overflow("block_offset + mem_size"))?;
        if self.init_block.len() < block_end {
            self.init_block.resize(block_end, 0);
        }

        // Copy the .tdata initialization image; the [.tdata, .tbss) tail stays zero (resize fills 0).
        let file_size =
            usize::try_from(tls.file_size).map_err(|_| TlsError::Overflow("file_size as usize"))?;
        if file_size > 0 {
            let file_off = usize::try_from(tdata_off)
                .map_err(|_| TlsError::Overflow("tdata file offset as usize"))?;
            let file_end = file_off
                .checked_add(file_size)
                .ok_or(TlsError::Overflow("tdata file end"))?;
            let src = file
                .get(file_off..file_end)
                .ok_or(TlsError::TdataOutOfFile(tdata_off))?;
            self.init_block[block_offset..block_offset + file_size].copy_from_slice(src);
        }

        // Index this module's DEFINED TLS symbols by name (TPOFF64 resolves imports to these).
        for sym in dynsyms {
            if sym.sym_type == STT_TLS && sym.shndx != SHN_UNDEF && !sym.name.is_empty() {
                // Within one module a later (or first) definition is canonical; insert keeps the
                // first-seen, which matches the .dynsym's primary definition order.
                self.tls_defs
                    .entry(sym.name.clone())
                    .or_insert((tp_offset, sym.value));
            }
        }

        self.accumulated = offset_i;
        let module = TlsModule {
            tp_offset,
            block_offset,
            size: mem_size,
        };
        self.modules.push(module);
        Ok(module)
    }

    /// The assembled TLS initialization block (`.tdata` copied, `.tbss` zero). A future
    /// `%fs`-binding step would copy this below the live thread pointer.
    pub fn init_block(&self) -> &[u8] {
        &self.init_block
    }

    /// The per-module placements, in add order.
    pub fn modules(&self) -> &[TlsModule] {
        &self.modules
    }

    /// The tp-relative value of a **defined** TLS symbol by name: `tp_offset(defining module) +
    /// st_value`, as the signed offset `R_X86_64_TPOFF64` writes (before the addend). `None` if no
    /// module in this layout defines the named TLS symbol. The returned value is negative (the
    /// block is below TP).
    pub fn tp_offset_of(&self, name: &str) -> Option<i64> {
        self.tls_defs
            .get(name)
            .map(|&(tp_offset, value)| tp_offset.wrapping_add(value as i64))
    }
}

/// A [`reloc::SymbolResolver`] that applies `R_X86_64_TPOFF64` through a [`TlsLayout`], delegating
/// every **non-TLS** relocation to an inner resolver (typically a
/// [`super::resolve::ScopedResolver`]).
///
/// `resolve_tls_offset(sym_index)`:
/// - **`sym_index == 0` (`STN_UNDEF`)** — a self-referential TPOFF64 against the **referencing
///   object's own** TLS block (no named symbol; the relocation's addend is the within-block
///   offset). The x86-64 psABI computes `S + A` with `S` the symbol's tp-relative address; with no
///   symbol, `S` is the referencing module's own tp-relative base. Returns that base (`own_tp_offset`,
///   supplied at construction). This is glibc's own thread-locals' self-relocation (`libc.so.6`'s 15
///   sym-0 TPOFF64 entries).
/// - **`sym_index != 0`** — maps the object's `sym_index` → its [`DynSym`] name → the layout's
///   [`TlsLayout::tp_offset_of`] (the **full** tp-relative value `-offset_i + st_value` of the
///   defining module — possibly a *different* module, e.g. `libm`'s `errno` import resolving into
///   `libc`'s block).
///
/// `resolve_symbol` forwards to the inner resolver unchanged.
///
/// ## Contract with [`reloc::apply_one`]
/// `reloc::apply_one` computes the `TPOFF64` value as
/// `image.static_tls_offset() + resolve_tls_offset(idx) + addend`. Because `resolve_tls_offset`
/// here already returns the **complete** module-relative-to-TP offset, the image must carry
/// `static_tls_offset == 0` so the written value is exactly `tp_offset + addend`. Callers
/// ([`super::map`]) build the relocation image with `static_tls_offset == 0` for the TLS pass.
pub struct TlsResolver<'a, R: SymbolResolver> {
    inner: &'a R,
    dynsyms: &'a [DynSym],
    layout: &'a TlsLayout,
    /// The referencing object's own TLS module tp-relative base (`None` if the object has no
    /// `PT_TLS`). Used for a self-referential `sym_index == 0` TPOFF64.
    own_tp_offset: Option<i64>,
}

impl<'a, R: SymbolResolver> TlsResolver<'a, R> {
    /// Wrap `inner` (the non-TLS resolver) with TLS resolution over `layout`, for an object whose
    /// relocations index into `dynsyms` and whose own TLS module has tp-relative base
    /// `own_tp_offset` (`None` if the object declares no `PT_TLS`).
    pub fn new(
        inner: &'a R,
        dynsyms: &'a [DynSym],
        layout: &'a TlsLayout,
        own_tp_offset: Option<i64>,
    ) -> Self {
        Self {
            inner,
            dynsyms,
            layout,
            own_tp_offset,
        }
    }
}

impl<R: SymbolResolver> SymbolResolver for TlsResolver<'_, R> {
    fn resolve_symbol(&self, sym_index: u32) -> Option<u64> {
        self.inner.resolve_symbol(sym_index)
    }

    fn resolve_tls_offset(&self, sym_index: u32) -> Option<u64> {
        // sym_index 0 = STN_UNDEF: a self-referential TPOFF64 against this object's OWN TLS block.
        // S is the object's own tp-relative base; apply_one adds the addend (the within-block
        // offset). Without an own PT_TLS this is malformed → None (typed UnresolvedSymbol).
        if sym_index == 0 {
            return self.own_tp_offset.map(|v| v as u64);
        }
        let sym = self.dynsyms.get(sym_index as usize)?;
        // The full tp-relative value of the defining module's symbol (negative), as a u64 bit
        // pattern. apply_one adds the addend (and the image's static_tls_offset, which is 0 here).
        self.layout.tp_offset_of(&sym.name).map(|v| v as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `TlsSegment` whose `.tdata` lives at file offset `file_off` (the tests pass the same
    /// `file_off` as `tdata_off` to `add_module`, mirroring the 1:1 PIE identity).
    fn seg(file_off: u64, file_size: u64, mem_size: u64, align: u64) -> TlsSegment {
        TlsSegment {
            vaddr: file_off,
            file_size,
            mem_size,
            align,
        }
    }

    /// A defined TLS `DynSym` (STT_TLS, defined section) named `name` at within-block `value`.
    fn tls_def(name: &str, value: u64) -> DynSym {
        DynSym {
            name: name.to_string(),
            value,
            size: 0,
            bind: 1, // STB_GLOBAL
            sym_type: STT_TLS,
            shndx: 1, // a real section → defined
        }
    }

    /// An undefined (imported) TLS `DynSym`.
    fn tls_undef(name: &str) -> DynSym {
        DynSym {
            shndx: SHN_UNDEF,
            ..tls_def(name, 0)
        }
    }

    /// A non-TLS resolver to feed [`TlsResolver`] for the delegation tests.
    struct InnerFixed;
    impl SymbolResolver for InnerFixed {
        fn resolve_symbol(&self, i: u32) -> Option<u64> {
            match i {
                1 => Some(0x7fff_1234_0000),
                _ => None,
            }
        }
        fn resolve_tls_offset(&self, _i: u32) -> Option<u64> {
            // The inner (non-TLS) resolver never resolves TLS — the wrapper must.
            None
        }
    }

    #[test]
    fn round_up_matches_alignment_identity() {
        assert_eq!(round_up(0, 16), Some(0));
        assert_eq!(round_up(1, 16), Some(16));
        assert_eq!(round_up(16, 16), Some(16));
        assert_eq!(round_up(17, 16), Some(32));
        assert_eq!(round_up(7, 1), Some(7)); // align 1 = no rounding
        assert_eq!(round_up(7, 0), Some(7)); // align 0 = no rounding
    }

    #[test]
    fn single_module_offset_is_minus_roundup_size() {
        // One module: tdata "ABCDEFGH" (8 bytes) + 8 bytes tbss = memsz 16, align 8.
        let file = b"ABCDEFGH".to_vec();
        let dynsyms = vec![tls_def("x", 4)]; // symbol at +4 within the block
        let mut layout = TlsLayout::new();
        let m = layout
            .add_module(&seg(0, 8, 16, 8), &file, 0, &dynsyms)
            .unwrap();
        // offset_1 = roundup(16, 8) = 16 → tp_offset = -16.
        assert_eq!(m.tp_offset, -16);
        assert_eq!(m.size, 16);
        // Symbol tp-relative value = -16 + st_value(4) = -12.
        assert_eq!(layout.tp_offset_of("x"), Some(-12));
        // The init block: tdata copied, tbss zeroed.
        assert_eq!(&layout.init_block()[0..8], b"ABCDEFGH");
        assert_eq!(&layout.init_block()[8..16], &[0u8; 8]);
    }

    #[test]
    fn single_module_size_is_rounded_up_for_offset() {
        // memsz 13, align 8 → roundup(13,8)=16 → tp_offset -16 (not -13).
        let file = vec![0u8; 5];
        let mut layout = TlsLayout::new();
        let m = layout.add_module(&seg(0, 5, 13, 8), &file, 0, &[]).unwrap();
        assert_eq!(m.tp_offset, -16);
    }

    #[test]
    fn multi_module_stacking_and_alignment() {
        // Module 1: memsz 16, align 8 → offset_1 = 16, tp_offset -16.
        // Module 2: memsz 12, align 16 → aligned_acc = roundup(16,16)=16; aligned_size =
        //   roundup(12,16)=16 → offset_2 = 32, tp_offset -32.
        // Module 3: memsz 8, align 8 → aligned_acc = roundup(32,8)=32; aligned_size = 8 →
        //   offset_3 = 40, tp_offset -40.
        let f1 = vec![0x11u8; 16];
        let f2 = vec![0x22u8; 12];
        let f3 = vec![0x33u8; 8];
        let mut layout = TlsLayout::new();
        let m1 = layout
            .add_module(&seg(0, 16, 16, 8), &f1, 0, &[tls_def("a", 0)])
            .unwrap();
        let m2 = layout
            .add_module(&seg(0, 12, 12, 16), &f2, 0, &[tls_def("b", 4)])
            .unwrap();
        let m3 = layout
            .add_module(&seg(0, 8, 8, 8), &f3, 0, &[tls_def("c", 0)])
            .unwrap();
        assert_eq!(m1.tp_offset, -16);
        assert_eq!(m2.tp_offset, -32);
        assert_eq!(m3.tp_offset, -40);
        // Per-symbol tp-relative values across modules.
        assert_eq!(layout.tp_offset_of("a"), Some(-16)); // -16 + 0
        assert_eq!(layout.tp_offset_of("b"), Some(-28)); // -32 + 4
        assert_eq!(layout.tp_offset_of("c"), Some(-40)); // -40 + 0
        assert_eq!(layout.tp_offset_of("missing"), None);
    }

    #[test]
    fn tdata_copied_and_tbss_zeroed_in_assembled_block() {
        // Module: 4 bytes tdata (0xAA*4) + 4 bytes tbss, align 4.
        let file = vec![0xAAu8; 4];
        let mut layout = TlsLayout::new();
        let m = layout.add_module(&seg(0, 4, 8, 4), &file, 0, &[]).unwrap();
        let block = &layout.init_block()[m.block_offset..m.block_offset + m.size];
        assert_eq!(&block[0..4], &[0xAA; 4]); // tdata copied
        assert_eq!(&block[4..8], &[0u8; 4]); // tbss zero
    }

    #[test]
    fn bad_align_is_typed_err() {
        let mut layout = TlsLayout::new();
        // align 3 is not a power of two.
        assert_eq!(
            layout.add_module(&seg(0, 0, 8, 3), &[], 0, &[]),
            Err(TlsError::BadAlign(3))
        );
    }

    #[test]
    fn filesz_larger_than_memsz_is_typed_err() {
        let mut layout = TlsLayout::new();
        assert_eq!(
            layout.add_module(&seg(0, 16, 8, 8), &[0u8; 16], 0, &[]),
            Err(TlsError::FileLargerThanMem(16, 8))
        );
    }

    #[test]
    fn tdata_past_file_is_typed_err() {
        let mut layout = TlsLayout::new();
        // file_size 8 but the file slice is only 4 bytes → .tdata is out of range.
        assert_eq!(
            layout.add_module(&seg(0, 8, 8, 8), &[0u8; 4], 0, &[]),
            Err(TlsError::TdataOutOfFile(0))
        );
    }

    #[test]
    fn tpoff64_applied_through_reloc_writes_tp_offset_plus_addend() {
        use crate::loader::reloc::{apply_one, Rela, SliceImage, R_X86_64_TPOFF64};

        // Layout: one module, symbol "tlsvar" at +0x10 within a memsz-0x40 align-0x10 block.
        // offset_1 = roundup(0x40, 0x10) = 0x40 → tp_offset -0x40; sym tp-relative = -0x40 + 0x10 =
        // -0x30. With addend 8 the TPOFF64 must write -0x30 + 8 = -0x28.
        //
        // 2026-06-05: the named import lives at index 1 (index 0 is ALWAYS the reserved STN_UNDEF
        // null symbol — a real toolchain never places a named symbol there). A sym-0 TPOFF64 is the
        // separate self-reference case (see `tpoff64_sym0_resolves_own_module_block`).
        let file = vec![0u8; 0x20];
        let dynsyms = vec![
            tls_undef(""),       // index 0: the reserved null symbol
            tls_undef("tlsvar"), // index 1: the relocated object's UND TLS import of "tlsvar"
        ];
        let mut layout = TlsLayout::new();
        // The DEFINING module supplies "tlsvar" at +0x10. (Defining-module dynsyms below.)
        layout
            .add_module(&seg(0, 0, 0x40, 0x10), &file, 0, &[tls_def("tlsvar", 0x10)])
            .unwrap();

        let inner = InnerFixed;
        // The referencing object has no own PT_TLS → own_tp_offset None (the import is named,
        // sym_index 1, resolved cross-module via the layout — not the sym-0 self path).
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);

        // The image carries static_tls_offset == 0 (the resolver returns the full tp-relative
        // value; see the TlsResolver contract). One word at offset 0.
        const BASE: u64 = 0x5555_5000_0000;
        let mut buf = vec![0u8; 8];
        let mut img = SliceImage::new(BASE, 0, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 1, // names "tlsvar" via the relocated object's dynsyms
            r_type: R_X86_64_TPOFF64,
            addend: 8,
        };
        apply_one(&mut img, &resolver, &rela).unwrap();

        let written = u64::from_le_bytes(buf[..8].try_into().unwrap());
        let expected = (-0x30i64 + 8) as u64;
        assert_eq!(written, expected);
        assert_eq!(written as i64, -0x28);
    }

    #[test]
    fn tpoff64_sym0_resolves_own_module_block() {
        // 2026-06-05: a self-referential TPOFF64 (sym_index 0 = STN_UNDEF) resolves against the
        // REFERENCING object's OWN TLS block (libc.so.6's 15 sym-0 TPOFF64 entries). S = own
        // tp_offset; the addend is the within-block offset. With own tp_offset -0x80 and addend
        // 0x40, the written value is -0x80 + 0x40 = -0x40.
        use crate::loader::reloc::{apply_one, Rela, SliceImage, R_X86_64_TPOFF64};

        let dynsyms = vec![tls_undef("")]; // index 0: the reserved null symbol
        let layout = TlsLayout::new(); // no named TLS def needed for the self path
        let inner = InnerFixed;
        // The referencing object's own TLS module sits at tp_offset -0x80.
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, Some(-0x80));

        const BASE: u64 = 0x5555_5000_0000;
        let mut buf = vec![0u8; 8];
        let mut img = SliceImage::new(BASE, 0, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 0, // STN_UNDEF: own-module self-reference
            r_type: R_X86_64_TPOFF64,
            addend: 0x40, // within-block offset
        };
        apply_one(&mut img, &resolver, &rela).unwrap();

        let written = u64::from_le_bytes(buf[..8].try_into().unwrap()) as i64;
        assert_eq!(written, -0x80 + 0x40);
        assert_eq!(written, -0x40);

        // Without an own PT_TLS, a sym-0 TPOFF64 is unresolved (None → typed error, never faked).
        let no_own = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(no_own.resolve_tls_offset(0), None);
    }

    #[test]
    fn non_tls_symbol_still_goes_through_inner_resolver() {
        // A GLOB_DAT-style resolve_symbol must delegate to the inner resolver unchanged.
        let dynsyms = vec![tls_undef(""), tls_undef("ignored")];
        let layout = TlsLayout::new();
        let inner = InnerFixed;
        // No own PT_TLS → a sym-0 TLS lookup is None (and the named imports below are unresolved).
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);
        // Inner resolves index 1 to a fixed address; the wrapper forwards it.
        assert_eq!(resolver.resolve_symbol(1), Some(0x7fff_1234_0000));
        assert_eq!(resolver.resolve_symbol(2), None);
        // A sym-0 TLS lookup with no own block is None (no self module).
        assert_eq!(resolver.resolve_tls_offset(0), None);
    }

    #[test]
    fn unresolved_tls_import_is_none() {
        // The relocated object imports "errno" (named, index 1) but no module in the layout defines
        // it → None, which the applier turns into a typed UnresolvedSymbol (never a fabricated
        // offset). The referencing object has no own PT_TLS (own_tp_offset None).
        let dynsyms = vec![tls_undef(""), tls_undef("errno")];
        let mut layout = TlsLayout::new();
        layout
            .add_module(&seg(0, 0, 16, 8), &[], 0, &[tls_def("other", 0)])
            .unwrap();
        let inner = InnerFixed;
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(resolver.resolve_tls_offset(1), None);
    }
}
