#![forbid(unsafe_code)]

use std::fmt;

use crate::abi::Abi;

const R_X86_64_NONE: u16 = 0;

const R_X86_64_64: u16 = 1;

const R_X86_64_GLOB_DAT: u16 = 6;

const R_X86_64_JUMP_SLOT: u16 = 7;

const R_X86_64_RELATIVE: u16 = 8;

const R_X86_64_TPOFF64: u16 = 18;

const R_X86_64_IRELATIVE: u16 = 37;

const R_AARCH64_NONE: u16 = 0;

const R_AARCH64_ABS64: u16 = 257;

const R_AARCH64_GLOB_DAT: u16 = 1025;

const R_AARCH64_JUMP_SLOT: u16 = 1026;

const R_AARCH64_RELATIVE: u16 = 1027;

const R_AARCH64_IRELATIVE: u16 = 1032;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocKind {
    None,

    Relative,

    GlobDat,

    JumpSlot,

    Abs64,

    TpOff64,

    IRelative,

    Unsupported(u16),
}

impl RelocKind {
    pub fn decode(abi: Abi, raw: u32) -> Option<Self> {
        let raw = u16::try_from(raw).ok()?;
        Some(match abi {
            Abi::X86_64 => match raw {
                R_X86_64_NONE => Self::None,
                R_X86_64_RELATIVE => Self::Relative,
                R_X86_64_GLOB_DAT => Self::GlobDat,
                R_X86_64_JUMP_SLOT => Self::JumpSlot,
                R_X86_64_64 => Self::Abs64,
                R_X86_64_TPOFF64 => Self::TpOff64,
                R_X86_64_IRELATIVE => Self::IRelative,
                _ => Self::Unsupported(raw),
            },
            Abi::Arm64V8a => match raw {
                R_AARCH64_NONE => Self::None,
                R_AARCH64_RELATIVE => Self::Relative,
                R_AARCH64_GLOB_DAT => Self::GlobDat,
                R_AARCH64_JUMP_SLOT => Self::JumpSlot,
                R_AARCH64_ABS64 => Self::Abs64,
                R_AARCH64_IRELATIVE => Self::IRelative,
                _ => Self::Unsupported(raw),
            },
        })
    }

    pub fn writes_symbol_address(self) -> bool {
        matches!(self, Self::GlobDat | Self::JumpSlot | Self::Abs64)
    }

    #[cfg(test)]
    pub fn encode(self, abi: Abi) -> u32 {
        if let Self::Unsupported(raw) = self {
            return raw.into();
        }
        (0..=u32::from(u16::MAX))
            .find(|&raw| Self::decode(abi, raw) == Some(self))
            .unwrap_or_else(|| panic!("{} code has no {self} relocation", abi.label()))
    }
}

impl fmt::Display for RelocKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "NONE"),
            Self::Relative => write!(f, "RELATIVE"),
            Self::GlobDat => write!(f, "GLOB_DAT"),
            Self::JumpSlot => write!(f, "JUMP_SLOT"),
            Self::Abs64 => write!(f, "ABS64"),
            Self::TpOff64 => write!(f, "TPOFF64"),
            Self::IRelative => write!(f, "IRELATIVE"),
            Self::Unsupported(raw) => write!(f, "type {raw}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rela {
    pub offset: u64,

    pub sym_index: u32,

    pub kind: RelocKind,

    pub addend: i64,
}

pub trait SymbolResolver {
    fn resolve_symbol(&self, sym_index: u32) -> Option<u64>;

    fn resolve_tls_offset(&self, sym_index: u32) -> Option<u64>;
}

pub trait RelocImage {
    fn abi(&self) -> Abi;

    fn base(&self) -> u64;

    fn static_tls_offset(&self) -> i64;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_u64(&self, offset: usize) -> Result<u64, RelocError>;

    fn write_u64(&mut self, offset: usize, value: u64) -> Result<(), RelocError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocError {
    UnsupportedType { abi: Abi, kind: RelocKind },

    UnresolvedSymbol(u32),

    OutOfBounds(usize),

    RelrAddressInvalid(u64),
}

impl fmt::Display for RelocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedType { abi, kind } => {
                write!(f, "unsupported {} relocation {kind}", abi.label())
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

pub struct SliceImage<'a> {
    abi: Abi,
    base: u64,
    static_tls_offset: i64,
    bytes: &'a mut [u8],
}

impl<'a> SliceImage<'a> {
    pub fn new(abi: Abi, base: u64, static_tls_offset: i64, bytes: &'a mut [u8]) -> Self {
        Self {
            abi,
            base,
            static_tls_offset,
            bytes,
        }
    }
}

impl RelocImage for SliceImage<'_> {
    fn abi(&self) -> Abi {
        self.abi
    }

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

fn image_offset_of(image: &dyn RelocImage, addr: u64) -> Result<usize, RelocError> {
    let rel = addr
        .checked_sub(image.base())
        .ok_or(RelocError::RelrAddressInvalid(addr))?;
    usize::try_from(rel).map_err(|_| RelocError::RelrAddressInvalid(addr))
}

pub fn apply_one(
    image: &mut dyn RelocImage,
    resolver: &dyn SymbolResolver,
    rela: &Rela,
) -> Result<(), RelocError> {
    let base = image.base();
    let target_off = image_offset_of(image, base.wrapping_add(rela.offset))?;

    let value = match rela.kind {
        RelocKind::Relative => base.wrapping_add(rela.addend as u64),
        RelocKind::GlobDat | RelocKind::JumpSlot => {
            let sym = resolver
                .resolve_symbol(rela.sym_index)
                .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?;
            match image.abi() {
                Abi::X86_64 => sym,
                Abi::Arm64V8a => sym.wrapping_add(rela.addend as u64),
            }
        }
        RelocKind::Abs64 => {
            let sym = resolver
                .resolve_symbol(rela.sym_index)
                .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?;
            sym.wrapping_add(rela.addend as u64)
        }
        RelocKind::TpOff64 => {
            let sym_tls = resolver
                .resolve_tls_offset(rela.sym_index)
                .ok_or(RelocError::UnresolvedSymbol(rela.sym_index))?;

            (image.static_tls_offset() as u64)
                .wrapping_add(sym_tls)
                .wrapping_add(rela.addend as u64)
        }
        kind @ (RelocKind::None | RelocKind::IRelative | RelocKind::Unsupported(_)) => {
            return Err(RelocError::UnsupportedType {
                abi: image.abi(),
                kind,
            })
        }
    };

    image.write_u64(target_off, value)
}

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

pub fn apply_relr(image: &mut dyn RelocImage, entries: &[u64]) -> Result<(), RelocError> {
    let base = image.base();

    let mut cursor: u64 = 0;

    for &entry in entries {
        if entry & 1 == 0 {
            relr_relocate_addr(image, base, entry)?;
            cursor = entry.wrapping_add(8);
        } else {
            let mut bits = entry >> 1;
            let mut addr = cursor;
            while bits != 0 {
                if bits & 1 != 0 {
                    relr_relocate_addr(image, base, addr)?;
                }
                bits >>= 1;
                addr = addr.wrapping_add(8);
            }

            cursor = cursor.wrapping_add(63 * 8);
        }
    }
    Ok(())
}

fn relr_relocate_addr(image: &mut dyn RelocImage, base: u64, addr: u64) -> Result<(), RelocError> {
    let off = image_offset_of(image, addr)?;
    let current = image.read_u64(off)?;
    image.write_u64(off, current.wrapping_add(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x5555_5000_0000;

    const TLS_BASE_OFF: i64 = -0x40;

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
                3 => Some(0x10),
                _ => None,
            }
        }
    }

    fn image_bytes(words: usize) -> Vec<u8> {
        vec![0u8; words * 8]
    }

    fn word(buf: &[u8], w: usize) -> u64 {
        u64::from_le_bytes(buf[w * 8..w * 8 + 8].try_into().unwrap())
    }

    #[test]
    fn relative_writes_base_plus_addend() {
        let mut buf = image_bytes(4);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let rela = Rela {
            offset: 16,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0x1234,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 2), BASE + 0x1234);

        assert_eq!(word(&buf, 0), 0);
        assert_eq!(word(&buf, 1), 0);
        assert_eq!(word(&buf, 3), 0);
    }

    #[test]
    fn glob_dat_and_jump_slot_write_symbol_address() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let glob = Rela {
            offset: 0,
            sym_index: 1,
            kind: RelocKind::GlobDat,
            addend: 999,
        };
        let jump = Rela {
            offset: 8,
            sym_index: 2,
            kind: RelocKind::JumpSlot,
            addend: 0,
        };
        apply_rela(&mut img, &FixedResolver, &[glob, jump]).unwrap();
        assert_eq!(word(&buf, 0), 0x7fff_aaaa_0000);
        assert_eq!(word(&buf, 1), 0x7fff_bbbb_0000);
    }

    #[test]
    fn reloc_kind_decodes_every_listed_number_for_both_abis() {
        let table = [
            (
                Abi::X86_64,
                [
                    (0, RelocKind::None),
                    (8, RelocKind::Relative),
                    (6, RelocKind::GlobDat),
                    (7, RelocKind::JumpSlot),
                    (1, RelocKind::Abs64),
                    (18, RelocKind::TpOff64),
                    (37, RelocKind::IRelative),
                ]
                .as_slice(),
            ),
            (
                Abi::Arm64V8a,
                [
                    (0, RelocKind::None),
                    (1027, RelocKind::Relative),
                    (1025, RelocKind::GlobDat),
                    (1026, RelocKind::JumpSlot),
                    (257, RelocKind::Abs64),
                    (1032, RelocKind::IRelative),
                ]
                .as_slice(),
            ),
        ];
        for (abi, numbers) in table {
            for &(raw, kind) in numbers {
                assert_eq!(
                    RelocKind::decode(abi, raw),
                    Some(kind),
                    "{abi:?} type {raw}"
                );
                assert_eq!(kind.encode(abi), raw, "{abi:?} {kind}");
            }
        }

        for (abi, raw) in [
            (Abi::X86_64, 1027),
            (Abi::X86_64, 16),
            (Abi::Arm64V8a, 8),
            (Abi::Arm64V8a, 1028),
            (Abi::Arm64V8a, 1029),
            (Abi::Arm64V8a, 1030),
            (Abi::Arm64V8a, 1031),
        ] {
            assert_eq!(
                RelocKind::decode(abi, raw),
                Some(RelocKind::Unsupported(raw as u16)),
                "{abi:?} type {raw}"
            );
        }
        for abi in [Abi::X86_64, Abi::Arm64V8a] {
            assert_eq!(RelocKind::decode(abi, 0x1_0000), None, "{abi:?}");
        }

        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::Arm64V8a, BASE, 0, &mut buf);
        let tlsdesc = Rela {
            offset: 0,
            sym_index: 1,
            kind: RelocKind::Unsupported(1031),
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &tlsdesc)
                .unwrap_err()
                .to_string(),
            "unsupported ARM64 relocation type 1031"
        );
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn decoded_relocations_stay_as_small_as_raw_elf64_rela_entries() {
        assert_eq!(std::mem::size_of::<Rela>(), 24);
    }

    #[test]
    fn glob_dat_adds_the_addend_only_on_aarch64() {
        for (abi, expected) in [
            (Abi::X86_64, 0x7fff_aaaa_0000),
            (Abi::Arm64V8a, 0x7fff_aaaa_0008),
        ] {
            for kind in [RelocKind::GlobDat, RelocKind::JumpSlot] {
                let mut buf = image_bytes(1);
                let mut img = SliceImage::new(abi, BASE, 0, &mut buf);
                let rela = Rela {
                    offset: 0,
                    sym_index: 1,
                    kind,
                    addend: 8,
                };
                apply_one(&mut img, &FixedResolver, &rela).unwrap();
                assert_eq!(word(&buf, 0), expected, "{abi:?} {kind}");
            }
        }
    }

    #[test]
    fn abs64_writes_symbol_plus_addend() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 1,
            kind: RelocKind::Abs64,
            addend: 0x20,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 0), 0x7fff_aaaa_0000 + 0x20);
    }

    #[test]
    fn tpoff64_writes_tls_base_plus_sym_offset_plus_addend() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 3,
            kind: RelocKind::TpOff64,
            addend: 8,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();

        let expected = (TLS_BASE_OFF as u64).wrapping_add(0x10).wrapping_add(8);
        assert_eq!(word(&buf, 0), expected);
        assert_eq!(expected as i64, -0x28);
    }

    #[test]
    fn unsupported_type_is_typed_err_not_panic() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let rela = Rela {
            offset: 0,
            sym_index: 0,
            kind: RelocKind::Unsupported(4),
            addend: 0,
        };
        let err = apply_one(&mut img, &FixedResolver, &rela).unwrap_err();
        assert_eq!(
            err,
            RelocError::UnsupportedType {
                abi: Abi::X86_64,
                kind: RelocKind::Unsupported(4)
            }
        );
        assert_eq!(err.to_string(), "unsupported x86-64 relocation type 4");
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn unresolved_symbol_is_typed_err() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 42,
            kind: RelocKind::GlobDat,
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::UnresolvedSymbol(42)
        );
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn out_of_bounds_offset_is_typed_err_no_write() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let rela = Rela {
            offset: 8,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0,
        };
        assert_eq!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::OutOfBounds(8)
        );

        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn out_of_bounds_straddling_end_is_rejected() {
        let mut buf = vec![0u8; 12];
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 8,
            sym_index: 0,
            kind: RelocKind::Relative,
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
        let mut buf = image_bytes(4);

        buf[8..16].copy_from_slice(&0x40u64.to_le_bytes());
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let entries = [BASE + 8];
        apply_relr(&mut img, &entries).unwrap();
        assert_eq!(word(&buf, 1), BASE + 0x40);

        assert_eq!(word(&buf, 0), 0);
        assert_eq!(word(&buf, 2), 0);
    }

    #[test]
    fn relr_bitmap_relocates_exactly_the_set_bits() {
        let mut buf = image_bytes(8);
        buf[0..8].copy_from_slice(&0x100u64.to_le_bytes());
        buf[8..16].copy_from_slice(&0x200u64.to_le_bytes());
        buf[24..32].copy_from_slice(&0x300u64.to_le_bytes());
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let data_bits = 0b101u64;
        let bitmap = (data_bits << 1) | 1;
        let entries = [BASE, bitmap];
        apply_relr(&mut img, &entries).unwrap();

        assert_eq!(word(&buf, 0), BASE + 0x100);
        assert_eq!(word(&buf, 1), BASE + 0x200);
        assert_eq!(word(&buf, 2), 0);
        assert_eq!(word(&buf, 3), BASE + 0x300);
        assert_eq!(word(&buf, 4), 0);
    }

    #[test]
    fn relr_multi_bitmap_and_address_advance() {
        let mut buf = image_bytes(70);
        buf[0..8].copy_from_slice(&0x11u64.to_le_bytes());
        buf[8..16].copy_from_slice(&0x22u64.to_le_bytes());
        buf[(65 * 8)..(65 * 8 + 8)].copy_from_slice(&0x33u64.to_le_bytes());
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let data_bits = 0b1u64;
        let bitmap = (data_bits << 1) | 1;
        let entries = [BASE, bitmap, BASE + 65 * 8];
        apply_relr(&mut img, &entries).unwrap();

        assert_eq!(word(&buf, 0), BASE + 0x11);
        assert_eq!(word(&buf, 1), BASE + 0x22);
        assert_eq!(word(&buf, 64), 0);
        assert_eq!(word(&buf, 65), BASE + 0x33);
    }

    #[test]
    fn relr_out_of_bounds_address_is_typed_err() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let entries = [BASE + 100];
        let err = apply_relr(&mut img, &entries).unwrap_err();
        assert!(matches!(err, RelocError::OutOfBounds(_)));
    }

    #[test]
    fn relr_address_below_base_is_typed_err() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let entries = [BASE - 8];
        let err = apply_relr(&mut img, &entries).unwrap_err();
        assert_eq!(err, RelocError::RelrAddressInvalid(BASE - 8));
    }

    #[test]
    fn rela_stops_at_first_error() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let good = Rela {
            offset: 0,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0x10,
        };
        let bad = Rela {
            offset: 8,
            sym_index: 0,
            kind: RelocKind::Unsupported(0xff),
            addend: 0,
        };
        let err = apply_rela(&mut img, &FixedResolver, &[good, bad]).unwrap_err();
        assert_eq!(
            err,
            RelocError::UnsupportedType {
                abi: Abi::X86_64,
                kind: RelocKind::Unsupported(0xff)
            }
        );
        assert_eq!(word(&buf, 0), BASE + 0x10);
        assert_eq!(word(&buf, 1), 0);
    }

    #[test]
    fn dispatch_is_exhaustive_over_supported_types() {
        let supported = [
            RelocKind::Abs64,
            RelocKind::GlobDat,
            RelocKind::JumpSlot,
            RelocKind::Relative,
            RelocKind::TpOff64,
        ];
        for kind in supported {
            let mut buf = image_bytes(1);
            let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

            let sym = match kind {
                RelocKind::TpOff64 => 3,
                RelocKind::Relative => 0,
                _ => 1,
            };
            let rela = Rela {
                offset: 0,
                sym_index: sym,
                kind,
                addend: 0,
            };
            let res = apply_one(&mut img, &FixedResolver, &rela);
            assert!(
                !matches!(res, Err(RelocError::UnsupportedType { .. })),
                "{kind} must be dispatched, not rejected as unsupported"
            );
        }

        for kind in [
            RelocKind::None,
            RelocKind::IRelative,
            RelocKind::Unsupported(9),
        ] {
            let mut buf = image_bytes(1);
            let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
            let rela = Rela {
                offset: 0,
                sym_index: 0,
                kind,
                addend: 0,
            };
            assert_eq!(
                apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
                RelocError::UnsupportedType {
                    abi: Abi::X86_64,
                    kind
                }
            );
        }
    }

    #[test]
    fn write_at_exact_end_boundary_succeeds() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 8,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0x7,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 1), BASE + 0x7);
    }

    #[test]
    fn offset_near_usize_max_is_out_of_bounds_no_overflow() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: u64::MAX - BASE,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0,
        };
        let err = apply_one(&mut img, &FixedResolver, &rela).unwrap_err();
        assert!(
            matches!(
                err,
                RelocError::OutOfBounds(_) | RelocError::RelrAddressInvalid(_)
            ),
            "expected a bounds error, got {err:?}"
        );
        assert_eq!(word(&buf, 0), 0);
    }

    #[test]
    fn relative_addend_overflow_wraps_then_bounds_checks_write() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: -1,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        assert_eq!(word(&buf, 0), BASE.wrapping_sub(1));
    }

    #[test]
    fn abs64_symbol_plus_addend_overflow_wraps_no_panic() {
        let mut buf = image_bytes(1);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 1,
            kind: RelocKind::Abs64,
            addend: i64::MAX,
        };
        apply_one(&mut img, &FixedResolver, &rela).unwrap();
        let expected = 0x7fff_aaaa_0000u64.wrapping_add(i64::MAX as u64);
        assert_eq!(word(&buf, 0), expected);
    }

    #[test]
    fn relr_bitmap_address_overflow_is_typed_err_not_panic() {
        let mut buf = image_bytes(2);
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);

        let addr_word = u64::MAX - 1;
        let bitmap = (1u64 << 1) | 1;
        let entries = [addr_word, bitmap];
        let err = apply_relr(&mut img, &entries).unwrap_err();
        assert!(
            matches!(
                err,
                RelocError::RelrAddressInvalid(_) | RelocError::OutOfBounds(_)
            ),
            "expected a bounds error, got {err:?}"
        );
    }

    #[test]
    fn empty_image_rejects_any_write() {
        let mut buf: Vec<u8> = Vec::new();
        let mut img = SliceImage::new(Abi::X86_64, BASE, TLS_BASE_OFF, &mut buf);
        assert!(img.is_empty());
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            kind: RelocKind::Relative,
            addend: 0,
        };
        assert!(matches!(
            apply_one(&mut img, &FixedResolver, &rela).unwrap_err(),
            RelocError::OutOfBounds(0)
        ));
    }
}
