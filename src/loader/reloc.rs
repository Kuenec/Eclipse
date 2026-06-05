//! Pure-Rust x86-64 ELF relocation applier — the Eclipse loader's relocation core.
//!
//! 2026-06-05: This is the standalone, unit-tested foundation of Eclipse's own Rust bionic
//! loader. It applies the relocation types that the vendored apkenv-era C shim linker cannot
//! (`docs/bionic-loader-strategy.md` §1) — the exact wall that blocks `System.loadLibrary`.
//!
//! ## Scope: relocation application only
//! The applier operates over a **loaded image abstraction** ([`RelocImage`]): the bytes are
//! assumed already laid out at their virtual offsets (segment layout / `mmap` is a separate,
//! later step). Given that image plus a [`SymbolResolver`] and the module's static-TLS offset,
//! it rewrites the words named by relocation entries. It does **no** ELF parsing, symbol-table
//! reading, or memory mapping — callers pass already-decoded [`Rela`] entries and the raw
//! `DT_RELR` table. Keeping the applier this small makes it total and exhaustively testable.
//!
//! ## Relocation types implemented (x86-64 psABI numbers)
//! All values and write formulas are from the **x86-64 System V psABI** relocation table
//! (general public ELF knowledge — no linker source was read):
//!
//! | Type | # | Write at `base+offset` | Notes |
//! |------|---|------------------------|-------|
//! | [`R_X86_64_64`]      | 1  | `sym + addend`            | Absolute 64-bit. |
//! | [`R_X86_64_GLOB_DAT`]| 6  | `sym`                     | GOT entry → symbol. |
//! | [`R_X86_64_JUMP_SLOT`]| 7 | `sym`                     | PLT entry → symbol (resolved eagerly under `BIND_NOW`). |
//! | [`R_X86_64_RELATIVE`]| 8  | `base + addend`           | Implicit-addend (`.rela`/`DT_RELR`). |
//! | [`R_X86_64_TPOFF64`] | 18 | `tls_offset + addend`     | Static-TLS thread-pointer-relative offset (see below). |
//!
//! Any other type is rejected with [`RelocError::UnsupportedType`] — this is the
//! `unknown reloc type N` gap the apkenv linker aborted on, surfaced here as a typed error
//! instead of an abort.
//!
//! ## `BIND_NOW` / eager binding
//! `BIND_NOW` (`DF_1_NOW`) means the loader must resolve **every** relocation — including
//! `R_X86_64_JUMP_SLOT` (PLT) entries — at load time rather than lazily on first call. This
//! applier already resolves `JUMP_SLOT` eagerly (it writes the symbol address straight into the
//! slot), so honoring `BIND_NOW` requires **no extra work**: a caller applies `.rela.plt`
//! alongside `.rela.dyn`. There is no lazy/PLT-stub path to suppress. See [`apply_rela`].
//!
//! ## `DT_RELR` (compressed relative relocations)
//! `DT_RELR` packs a run of `R_X86_64_RELATIVE` relocations as a bitmap. Decoding is in
//! [`apply_relr`]; the encoding is documented there.
//!
//! ## Static-TLS model assumption (`R_X86_64_TPOFF64`)
//! `R_X86_64_TPOFF64` stores, into the relocated GOT slot, the address of a TLS symbol **as a
//! negative offset from the thread pointer** (`%fs.base` on x86-64; the static-TLS block for a
//! module lives below the thread pointer, so the offset is negative). Per the x86-64 psABI the
//! relocated value is `S_tls_offset + A`, where `S_tls_offset` is the symbol's offset within the
//! module's static-TLS block measured from the thread pointer, and `A` is the addend.
//!
//! This applier takes that per-module tp-relative base offset as an **input**
//! ([`RelocImage::static_tls_offset`]) and computes `tls_offset + addend` for the named symbol's
//! within-block offset (carried in the [`Rela::addend`] / the symbol's value, combined by the
//! caller into the addend it passes — see [`apply_one`]). The applier intentionally does **not**
//! allocate the static-TLS block or set up `%fs`/the TCB: that allocation + thread-pointer setup
//! is a **separate later step** of the broader loader (it must interoperate with the host glibc
//! TCB layout — `docs/bionic-loader-strategy.md` §2(a)). Here we only apply the relocation given
//! the offset the (future) TLS-block allocator will have assigned.
//!
//! ## Soundness
//! Every write is bounds-checked against the image length before any byte is touched
//! ([`RelocImage::write_u64`]); an out-of-range offset returns [`RelocError::OutOfBounds`]
//! rather than panicking or writing out of bounds. The module is `#![forbid(unsafe_code)]`: the
//! image is a safe `&mut [u8]`, so no `unsafe` is needed at all (AGENTS.md §2.3).

#![forbid(unsafe_code)]

use std::fmt;

/// x86-64 `R_X86_64_64`: absolute 64-bit, writes `sym + addend`.
pub const R_X86_64_64: u32 = 1;
/// x86-64 `R_X86_64_GLOB_DAT`: GOT entry, writes `sym`.
pub const R_X86_64_GLOB_DAT: u32 = 6;
/// x86-64 `R_X86_64_JUMP_SLOT`: PLT entry, writes `sym` (resolved eagerly under `BIND_NOW`).
pub const R_X86_64_JUMP_SLOT: u32 = 7;
/// x86-64 `R_X86_64_RELATIVE`: writes `base + addend` (implicit-addend; `.rela` and `DT_RELR`).
pub const R_X86_64_RELATIVE: u32 = 8;
/// x86-64 `R_X86_64_TPOFF64` (type 18): static-TLS, writes `tls_offset + addend`. This is the
/// `unknown reloc type 18` the apkenv linker aborts on (`docs/bionic-loader-strategy.md` §1).
pub const R_X86_64_TPOFF64: u32 = 18;

/// A decoded `Elf64_Rela` entry, split into the fields the applier needs.
///
/// The caller (a later ELF-parsing step) decodes `r_offset` / `r_info` / `r_addend` from the
/// `.rela.dyn` / `.rela.plt` tables. `r_info` is split into the symbol index (`info >> 32`) and
/// the relocation [`Self::r_type`] (`info & 0xffff_ffff`); the applier only needs the type, so
/// the symbol is pre-resolved by the caller and supplied through the [`SymbolResolver`] keyed by
/// that index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rela {
    /// `r_offset`: the location to relocate, as the **in-object virtual offset** (a PIE object
    /// records `r_offset` as a virtual address with a zero image base). The target's run-time
    /// virtual address is therefore [`RelocImage::base`] `+ offset`; [`apply_one`] converts that
    /// back to the in-image byte offset (which equals `offset`) before the bounds-checked write.
    pub offset: u64,
    /// `r_info >> 32`: the dynamic-symbol-table index this relocation references (0 / unused for
    /// `RELATIVE`). The resolver maps this to a runtime address.
    pub sym_index: u32,
    /// `r_info & 0xffff_ffff`: the x86-64 relocation type (e.g. [`R_X86_64_GLOB_DAT`]).
    pub r_type: u32,
    /// `r_addend`: the explicit addend (`A`). For `TPOFF64` the symbol's within-block TLS offset
    /// is folded into the resolver result; see [`apply_one`].
    pub addend: i64,
}

/// Resolves a relocation's referenced symbol to the value the applier writes.
///
/// This is the seam to the rest of the (future) loader: symbol lookup across the bionic
/// two-namespace scope, the Rust shim for unresolved bionic symbols, and the static-TLS offset
/// assignment all live behind this trait. The applier itself stays pure relocation arithmetic.
pub trait SymbolResolver {
    /// Resolve the run-time **address** of the symbol at dynamic-symtab index `sym_index`, for a
    /// non-TLS relocation ([`R_X86_64_64`]/[`R_X86_64_GLOB_DAT`]/[`R_X86_64_JUMP_SLOT`]).
    ///
    /// Returns `None` if the symbol is unresolved (no definition in scope) — the applier turns
    /// that into [`RelocError::UnresolvedSymbol`] rather than writing a bogus address.
    fn resolve_symbol(&self, sym_index: u32) -> Option<u64>;

    /// Resolve the symbol's **offset within its module's static-TLS block** for a
    /// [`R_X86_64_TPOFF64`] relocation. This is the symbol's `st_value` (its byte offset inside
    /// the module's `.tdata`/`.tbss`); it is combined with [`RelocImage::static_tls_offset`] and
    /// the addend in [`apply_one`] to form the final tp-relative value.
    ///
    /// Returns `None` if the TLS symbol is unresolved → [`RelocError::UnresolvedSymbol`].
    fn resolve_tls_offset(&self, sym_index: u32) -> Option<u64>;
}

/// A loaded library image being relocated: the load base, the module's static-TLS tp-relative
/// base offset, and bounds-checked access to the image bytes.
///
/// The concrete in-memory image is [`SliceImage`] (a safe `&mut [u8]`). This trait lets tests use
/// a hand-built fixture and lets the broader loader supply an `mmap`-backed image later, without
/// the applier ever touching raw pointers.
pub trait RelocImage {
    /// The run-time load base (`l_addr`) added to `RELATIVE`/`RELR` addends and used as the
    /// origin for [`Rela::offset`].
    fn base(&self) -> u64;

    /// The module's static-TLS block base **offset from the thread pointer** (negative on
    /// x86-64; the module's TLS block sits below `%fs.base`). Assigned by the (future) static-TLS
    /// allocator; used only for [`R_X86_64_TPOFF64`]. See the module docs' "Static-TLS model".
    fn static_tls_offset(&self) -> i64;

    /// Total length of the image in bytes (for bounds checks).
    fn len(&self) -> usize;

    /// True if the image has no bytes. (Clippy `len_without_is_empty`.)
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read the little-endian `u64` currently stored at image offset `offset`. Returns
    /// [`RelocError::OutOfBounds`] if `[offset, offset+8)` is not fully inside the image.
    fn read_u64(&self, offset: usize) -> Result<u64, RelocError>;

    /// Write `value` as little-endian at image offset `offset`. Returns
    /// [`RelocError::OutOfBounds`] (writing nothing) if `[offset, offset+8)` is not fully inside
    /// the image — the soundness boundary that replaces the apkenv linker's wild write.
    fn write_u64(&mut self, offset: usize, value: u64) -> Result<(), RelocError>;
}

/// Typed relocation errors. Every fallible path returns one of these instead of aborting (the
/// apkenv linker `abort()`ed on the first unknown type) or writing out of bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocError {
    /// The relocation type is not one this applier handles. The field is the raw x86-64 type
    /// number — e.g. `18` was the `unknown reloc type 18` (`R_X86_64_TPOFF64`) abort before this
    /// applier; any *other* unhandled type now surfaces here instead of corrupting the image.
    UnsupportedType(u32),
    /// A relocation referenced a symbol the [`SymbolResolver`] could not resolve. Carries the
    /// dynamic-symtab index.
    UnresolvedSymbol(u32),
    /// The relocation's target word `[offset, offset+8)` lies outside the image bounds. Carries
    /// the offending image offset. No bytes are written.
    OutOfBounds(usize),
    /// A `DT_RELR` entry's relocated address could not be represented as an in-image offset
    /// (underflowed the load base or exceeded `usize`). Carries the raw address.
    RelrAddressInvalid(u64),
}

impl fmt::Display for RelocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedType(t) => {
                write!(f, "unsupported x86-64 relocation type {t}")
            }
            Self::UnresolvedSymbol(i) => {
                write!(f, "unresolved symbol at dynamic-symtab index {i}")
            }
            Self::OutOfBounds(off) => {
                write!(
                    f,
                    "relocation target offset {off} is outside the image bounds"
                )
            }
            Self::RelrAddressInvalid(addr) => {
                write!(
                    f,
                    "DT_RELR relocated address {addr:#x} is not a valid in-image offset"
                )
            }
        }
    }
}

impl std::error::Error for RelocError {}

/// A concrete in-memory [`RelocImage`] over a borrowed, mutable byte buffer.
///
/// The buffer is a safe `&mut [u8]`, so all reads/writes use `slice` indexing with explicit
/// bounds checks — no `unsafe`, no raw pointers (AGENTS.md §2.3). The broader loader will provide
/// an `mmap`-backed `RelocImage` later; this one backs the unit tests and any caller that already
/// has the image in a `Vec<u8>`/slice.
pub struct SliceImage<'a> {
    base: u64,
    static_tls_offset: i64,
    bytes: &'a mut [u8],
}

impl<'a> SliceImage<'a> {
    /// Build an image view over `bytes` with the given load `base` and module
    /// `static_tls_offset` (the tp-relative base used only by [`R_X86_64_TPOFF64`]).
    pub fn new(base: u64, static_tls_offset: i64, bytes: &'a mut [u8]) -> Self {
        Self {
            base,
            static_tls_offset,
            bytes,
        }
    }
}

impl RelocImage for SliceImage<'_> {
    fn base(&self) -> u64 {
        self.base
    }

    fn static_tls_offset(&self) -> i64 {
        self.static_tls_offset
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn read_u64(&self, offset: usize) -> Result<u64, RelocError> {
        let end = offset
            .checked_add(8)
            .ok_or(RelocError::OutOfBounds(offset))?;
        let slot = self
            .bytes
            .get(offset..end)
            .ok_or(RelocError::OutOfBounds(offset))?;
        // `slot` is exactly 8 bytes by construction (end - offset == 8); the array cast is total.
        Ok(u64::from_le_bytes(slot.try_into().expect("8-byte slice")))
    }

    fn write_u64(&mut self, offset: usize, value: u64) -> Result<(), RelocError> {
        let end = offset
            .checked_add(8)
            .ok_or(RelocError::OutOfBounds(offset))?;
        let slot = self
            .bytes
            .get_mut(offset..end)
            .ok_or(RelocError::OutOfBounds(offset))?;
        slot.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }
}

/// Convert a relocation target **virtual address** (load base + in-object offset) into an
/// in-image byte offset, or [`RelocError::OutOfBounds`] if it is below the base. Shared by the
/// `RELATIVE`/`RELR` paths whose target is expressed as a full address.
fn image_offset_of(image: &dyn RelocImage, addr: u64) -> Result<usize, RelocError> {
    let rel = addr
        .checked_sub(image.base())
        .ok_or(RelocError::RelrAddressInvalid(addr))?;
    usize::try_from(rel).map_err(|_| RelocError::RelrAddressInvalid(addr))
}

/// Apply a single decoded relocation entry to the image.
///
/// The write target is `base + offset` for every type; the value written depends on the type:
/// - [`R_X86_64_RELATIVE`] → `base + addend`
/// - [`R_X86_64_GLOB_DAT`] / [`R_X86_64_JUMP_SLOT`] → `sym`
/// - [`R_X86_64_64`] → `sym + addend`
/// - [`R_X86_64_TPOFF64`] → `static_tls_offset + sym_tls_offset + addend`
///   (the symbol's within-block TLS offset, from [`SymbolResolver::resolve_tls_offset`], added to
///   the module's tp-relative base and the addend — see the module docs' "Static-TLS model")
/// - anything else → [`RelocError::UnsupportedType`] (the exhaustive-dispatch gate).
///
/// All address arithmetic uses wrapping adds: relocation math is modular over the address space
/// (a load base near the top of the address space plus a large addend legitimately wraps), and
/// the *write* itself is still bounds-checked, so wrapping cannot cause an out-of-bounds write.
pub fn apply_one(
    image: &mut dyn RelocImage,
    resolver: &dyn SymbolResolver,
    rela: &Rela,
) -> Result<(), RelocError> {
    let base = image.base();
    let target_off = image_offset_of(image, base.wrapping_add(rela.offset))?;

    let value = match rela.r_type {
        R_X86_64_RELATIVE => base.wrapping_add(rela.addend as u64),
        R_X86_64_GLOB_DAT | R_X86_64_JUMP_SLOT => resolver
            .resolve_symbol(rela.sym_index)
            .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?,
        R_X86_64_64 => {
            let sym = resolver
                .resolve_symbol(rela.sym_index)
                .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?;
            sym.wrapping_add(rela.addend as u64)
        }
        R_X86_64_TPOFF64 => {
            let sym_tls = resolver
                .resolve_tls_offset(rela.sym_index)
                .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?;
            // tp-relative value = module's static-TLS base offset (negative, from %fs) + the
            // symbol's offset within that block + addend. (x86-64 psABI: S + A, with S measured
            // from the thread pointer.) 2026-06-05.
            (image.static_tls_offset() as u64)
                .wrapping_add(sym_tls)
                .wrapping_add(rela.addend as u64)
        }
        other => return Err(RelocError::UnsupportedType(other)),
    };

    image.write_u64(target_off, value)
}

/// Apply a `.rela` table ([`Rela`] entries from `.rela.dyn` and/or `.rela.plt`).
///
/// Applying `.rela.plt` here (alongside `.rela.dyn`) is exactly what `BIND_NOW` requires:
/// `R_X86_64_JUMP_SLOT` entries are resolved eagerly to their symbol addresses. Stops at the
/// first error (a real loader must not continue past a corrupt/unresolved relocation).
pub fn apply_rela(
    image: &mut dyn RelocImage,
    resolver: &dyn SymbolResolver,
    relas: &[Rela],
) -> Result<(), RelocError> {
    for rela in relas {
        apply_one(image, resolver, rela)?;
    }
    Ok(())
}

/// Apply a `DT_RELR` compressed relative-relocation table.
///
/// ## `DT_RELR` encoding (the standard `SHT_RELR`/`DT_RELR` scheme — public ELF knowledge)
/// `entries` is the raw `DT_RELR` array of `u64` words. Each word is either:
/// - **even** (LSB = 0): an **address** — the relocated location itself. The applier relocates
///   the word at that address as a `R_X86_64_RELATIVE` (`*addr += base`) and sets the bitmap
///   **cursor** to the word *after* it (`addr + 8`).
/// - **odd** (LSB = 1): a **bitmap** for the 63 words starting at the cursor. Excluding the LSB
///   flag, data bit `j` (for `j` in `0..=62`, i.e. `entry` bit `j+1`) set ⇒ relocate the word at
///   `cursor + j*8` as `R_X86_64_RELATIVE`. After the bitmap, the cursor advances by `63 * 8`
///   bytes so the next bitmap word covers the following 63-word run.
///
/// Each relocated word is read, has the load `base` added (`*p += base` — RELR encodes only
/// `R_X86_64_RELATIVE`, whose addend is the value already stored at the location), and written
/// back, all bounds-checked. A bitmap appearing before any address (no cursor set) is malformed;
/// its set bits are relative to a zero cursor, which `image_offset_of` will reject as out of
/// bounds — surfaced as a typed error, never UB.
pub fn apply_relr(image: &mut dyn RelocImage, entries: &[u64]) -> Result<(), RelocError> {
    let base = image.base();
    // `cursor` is a virtual address (base + in-object offset), matching the address words.
    let mut cursor: u64 = 0;

    for &entry in entries {
        if entry & 1 == 0 {
            // Address word: relocate this location, then set the cursor just past it.
            relr_relocate_addr(image, base, entry)?;
            cursor = entry.wrapping_add(8);
        } else {
            // Bitmap word: bit i (1..=63) relocates cursor + (i-1) words.
            let mut bits = entry >> 1;
            let mut addr = cursor;
            while bits != 0 {
                if bits & 1 != 0 {
                    relr_relocate_addr(image, base, addr)?;
                }
                bits >>= 1;
                addr = addr.wrapping_add(8);
            }
            // Advance past the 63 words this bitmap covered.
            cursor = cursor.wrapping_add(63 * 8);
        }
    }
    Ok(())
}

/// Relocate one `R_X86_64_RELATIVE` location named by an absolute address (the `DT_RELR` form):
/// `*(addr) += base`, bounds-checked.
fn relr_relocate_addr(image: &mut dyn RelocImage, base: u64, addr: u64) -> Result<(), RelocError> {
    let off = image_offset_of(image, addr)?;
    let current = image.read_u64(off)?;
    image.write_u64(off, current.wrapping_add(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load base used across the fixtures — a realistic non-zero PIE load address.
    const BASE: u64 = 0x5555_5000_0000;
    /// The module's static-TLS tp-relative base offset (negative: the block sits below %fs).
    const TLS_BASE_OFF: i64 = -0x40;

    /// A resolver returning fixed addresses/TLS offsets for a couple of symbol indices.
    struct FixedResolver;
    impl SymbolResolver for FixedResolver {
        fn resolve_symbol(&self, sym_index: u32) -> Option<u64> {
            match sym_index {
                1 => Some(0x7fff_aaaa_0000),
                2 => Some(0x7fff_bbbb_0000),
                _ => None,
            }
        }
        fn resolve_tls_offset(&self, sym_index: u32) -> Option<u64> {
            match sym_index {
                3 => Some(0x10), // symbol at +0x10 within the module's TLS block
                _ => None,
            }
        }
    }

    /// Build a zeroed image buffer of `words` u64 slots.
    fn image_bytes(words: usize) -> Vec<u8> {
        vec![0u8; words * 8]
    }

    /// Read a u64 word from a raw buffer at word index `w`.
    fn word(buf: &[u8], w: usize) -> u64 {
        u64::from_le_bytes(buf[w * 8..w * 8 + 8].try_into().unwrap())
    }

    #[test]
    fn relative_writes_base_plus_addend() {
        let mut buf = image_bytes(4);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // Relocate word index 2 (offset 16): *(base+16) = base + 0x1234.
        let rela = Rela {
            offset: 16,
            sym_index: 0,
            r_type: R_X86_64_RELATIVE,
            addend: 0x1234,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 2), BASE + 0x1234);
        // Other words untouched.
        assert_eq!(word(&buf, 0), 0);
        assert_eq!(word(&buf, 1), 0);
        assert_eq!(word(&buf, 3), 0);
    }

    #[test]
    fn glob_dat_and_jump_slot_write_symbol_address() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let glob = Rela {
            offset: 0,
            sym_index: 1,
            r_type: R_X86_64_GLOB_DAT,
            addend: 999, // GLOB_DAT ignores the addend → must NOT appear in the result
        };
        let jump = Rela {
            offset: 8,
            sym_index: 2,
            r_type: R_X86_64_JUMP_SLOT,
            addend: 0,
        };
        apply_rela(&mut img, &FixedResolver, &[glob, jump]).unwrap();
        assert_eq!(word(&buf, 0), 0x7fff_aaaa_0000); // sym 1, addend ignored
        assert_eq!(word(&buf, 1), 0x7fff_bbbb_0000); // sym 2
    }

    #[test]
    fn abs64_writes_symbol_plus_addend() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 1,
            r_type: R_X86_64_64,
            addend: 0x20,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 0), 0x7fff_aaaa_0000 + 0x20);
    }

    #[test]
    fn tpoff64_writes_tls_base_plus_sym_offset_plus_addend() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 3, // resolves to within-block TLS offset 0x10
            r_type: R_X86_64_TPOFF64,
            addend: 8,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        // tp-relative = TLS_BASE_OFF (-0x40) + 0x10 + 8 = -0x28, as a u64 bit pattern.
        let expected = (TLS_BASE_OFF as u64).wrapping_add(0x10).wrapping_add(8);
        assert_eq!(word(&buf, 0), expected);
        assert_eq!(expected as i64, -0x28);
    }

    #[test]
    fn unsupported_type_is_typed_err_not_panic() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // Type 4 = R_X86_64_PLT32 (not handled by this applier). Must be a typed Err, and the
        // image must be left untouched (the abort the apkenv linker hit, here a clean error).
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            r_type: 4,
            addend: 0,
        };
        let err = apply_one(&mut img, &FixedResolver, &rela).unwrap_err();
        assert_eq!(err, RelocError::UnsupportedType(4));
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn unresolved_symbol_is_typed_err() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 42, // resolver returns None
            r_type: R_X86_64_GLOB_DAT,
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::UnresolvedSymbol(42)
        );
        assert_eq!(word(&buf, 0), 0); // nothing written
    }

    #[test]
    fn out_of_bounds_offset_is_typed_err_no_write() {
        let mut buf = image_bytes(1); // 8 bytes total; only offset 0 is valid
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // offset 8 → [8,16) is past the end of an 8-byte image.
        let rela = Rela {
            offset: 8,
            sym_index: 0,
            r_type: R_X86_64_RELATIVE,
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::OutOfBounds(8)
        );
        // The one valid word stays zero — no partial/out-of-bounds write happened.
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn out_of_bounds_straddling_end_is_rejected() {
        // 12-byte image: offset 8 would write [8,16), straddling the end → rejected, no write.
        let mut buf = vec![0u8; 12];
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 8,
            sym_index: 0,
            r_type: R_X86_64_RELATIVE,
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::OutOfBounds(8)
        );
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn relr_single_address_word_relocates_one_location() {
        // 4-word image; pre-seed word 1 with an in-object offset so *(p) += base is observable.
        let mut buf = image_bytes(4);
        // word 1 holds 0x40 (a relative pointer); after RELATIVE it must become base + 0x40.
        buf[8..16].copy_from_slice(&0x40u64.to_le_bytes());
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // Address word = base + 8 (word index 1), even → relocate that single location.
        let entries = [BASE + 8];
        apply_relr(&mut img, &entries).unwrap();
        assert_eq!(word(&buf, 1), BASE + 0x40);
        // Neighbors untouched.
        assert_eq!(word(&buf, 0), 0);
        assert_eq!(word(&buf, 2), 0);
    }

    #[test]
    fn relr_bitmap_relocates_exactly_the_set_bits() {
        // Layout: address word points at word 0; the bitmap then covers words 1..=63 (the run
        // starting at the cursor, which the address word set to word 1). We pre-seed words 0,1,3
        // with distinct relative values and set the matching bitmap data bits.
        let mut buf = image_bytes(8);
        buf[0..8].copy_from_slice(&0x100u64.to_le_bytes()); // word 0 (the address-word target)
        buf[8..16].copy_from_slice(&0x200u64.to_le_bytes()); // word 1 = cursor+0 (data bit 0)
        buf[24..32].copy_from_slice(&0x300u64.to_le_bytes()); // word 3 = cursor+2 (data bit 2)
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);

        // Address word = base + 0 → relocates word 0, cursor := word 1.
        // Bitmap: data bits 0 and 2 set (words cursor+0 and cursor+2), shifted past the LSB flag.
        let data_bits = 0b101u64; // bit 0 and bit 2
        let bitmap = (data_bits << 1) | 1;
        let entries = [BASE, bitmap];
        apply_relr(&mut img, &entries).unwrap();

        assert_eq!(word(&buf, 0), BASE + 0x100); // address word target
        assert_eq!(word(&buf, 1), BASE + 0x200); // data bit 0 → cursor + 0
        assert_eq!(word(&buf, 2), 0); // data bit 1 clear → untouched
        assert_eq!(word(&buf, 3), BASE + 0x300); // data bit 2 → cursor + 2 words
        assert_eq!(word(&buf, 4), 0);
    }

    #[test]
    fn relr_multi_bitmap_and_address_advance() {
        // Exercise: address word, a bitmap covering the next 63 words (only data bit 0 set here),
        // then a SECOND address word that re-seeds the cursor elsewhere. Proves the cursor
        // advances by 63 words after a bitmap and is reset by an address word.
        // Use a 70-word image so word 64 (cursor after first bitmap) exists but we don't target it.
        let mut buf = image_bytes(70);
        buf[0..8].copy_from_slice(&0x11u64.to_le_bytes()); // word 0  (1st address word)
        buf[8..16].copy_from_slice(&0x22u64.to_le_bytes()); // word 1 = cursor+0 (data bit 0)
        buf[(65 * 8)..(65 * 8 + 8)].copy_from_slice(&0x33u64.to_le_bytes()); // word 65 (2nd address word)
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);

        let data_bits = 0b1u64; // only data bit 0 set (word 1)
        let bitmap = (data_bits << 1) | 1; // shift past the LSB flag, mark as a bitmap
        let entries = [
            BASE,          // address word → relocate word 0, cursor := word 1
            bitmap,        // bitmap → relocate word 1, cursor advances to word 1+63 = word 64
            BASE + 65 * 8, // address word → relocate word 65, cursor := word 66
        ];
        apply_relr(&mut img, &entries).unwrap();

        assert_eq!(word(&buf, 0), BASE + 0x11); // 1st address word
        assert_eq!(word(&buf, 1), BASE + 0x22); // bitmap data bit 0 → cursor + 0
        assert_eq!(word(&buf, 64), 0); // cursor landed here but no bit/address targeted it
        assert_eq!(word(&buf, 65), BASE + 0x33); // 2nd address word
    }

    #[test]
    fn relr_out_of_bounds_address_is_typed_err() {
        let mut buf = image_bytes(2); // valid words 0,1 only
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // Address word points past the end of the image.
        let entries = [BASE + 100];
        let err = apply_relr(&mut img, &entries).unwrap_err();
        assert!(matches!(err, RelocError::OutOfBounds(_)));
    }

    #[test]
    fn relr_address_below_base_is_typed_err() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        // An address word below the load base cannot be an in-image offset.
        let entries = [BASE - 8];
        let err = apply_relr(&mut img, &entries).unwrap_err();
        assert_eq!(err, RelocError::RelrAddressInvalid(BASE - 8));
    }

    #[test]
    fn rela_stops_at_first_error() {
        // First entry is fine; second is unsupported → apply_rela returns the second's error and
        // the first entry's write IS visible (we stop AT the bad one, not roll back).
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let good = Rela {
            offset: 0,
            sym_index: 0,
            r_type: R_X86_64_RELATIVE,
            addend: 0x10,
        };
        let bad = Rela {
            offset: 8,
            sym_index: 0,
            r_type: 0xff,
            addend: 0,
        };
        let err = apply_rela(&mut img, &FixedResolver, &[good, bad]).unwrap_err();
        assert_eq!(err, RelocError::UnsupportedType(0xff));
        assert_eq!(word(&buf, 0), BASE + 0x10); // good one applied
        assert_eq!(word(&buf, 1), 0); // bad one not applied
    }

    /// Exhaustiveness guard: every type this applier claims to support must NOT return
    /// `UnsupportedType`, and a representative unsupported type MUST. This is the regression
    /// guard tied to the apkenv linker's `unknown reloc type` abort — if a future edit drops a
    /// supported type from the dispatch, this fails.
    #[test]
    fn dispatch_is_exhaustive_over_supported_types() {
        let supported = [
            R_X86_64_64,
            R_X86_64_GLOB_DAT,
            R_X86_64_JUMP_SLOT,
            R_X86_64_RELATIVE,
            R_X86_64_TPOFF64,
        ];
        for &t in &supported {
            let mut buf = image_bytes(1);
            let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
            // sym_index 1 (resolves) for symbol types, 3 for TLS, 0 for RELATIVE.
            let sym = match t {
                R_X86_64_TPOFF64 => 3,
                R_X86_64_RELATIVE => 0,
                _ => 1,
            };
            let rela = Rela {
                offset: 0,
                sym_index: sym,
                r_type: t,
                addend: 0,
            };
            let res = apply_one(&mut img, &FixedResolver, &rela);
            assert!(
                !matches!(res, Err(RelocError::UnsupportedType(_))),
                "type {t} must be dispatched, not rejected as unsupported"
            );
        }
        // The exact type that walled the engine (18) is in the supported set above; a different
        // unhandled type is still a clean typed error.
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            r_type: 9, // R_X86_64_GOTPCREL — not handled
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::UnsupportedType(9)
        );
    }
}
