#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use super::elf::{DynSym, TlsSegment};
use super::reloc::SymbolResolver;

const STT_TLS: u8 = 6;

const SHN_UNDEF: u16 = 0;

fn round_up(value: u64, align: u64) -> Option<u64> {
    if align <= 1 {
        return Some(value);
    }
    let mask = align - 1;
    value.checked_add(mask).map(|v| v & !mask)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsError {
    BadAlign(u64),

    FileLargerThanMem(u64, u64),

    Overflow(&'static str),

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsModule {
    pub tp_offset: i64,

    pub block_offset: usize,

    pub size: usize,
}

#[derive(Debug, Clone, Default)]
pub struct TlsLayout {
    init_block: Vec<u8>,

    modules: Vec<TlsModule>,

    accumulated: u64,

    tls_defs: HashMap<String, (i64, u64)>,
}

impl TlsLayout {
    pub fn new() -> Self {
        Self::default()
    }

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

        let aligned_acc = round_up(self.accumulated, tls.align)
            .ok_or(TlsError::Overflow("accumulated alignment"))?;
        let aligned_size =
            round_up(tls.mem_size, tls.align).ok_or(TlsError::Overflow("module size alignment"))?;
        let offset_i = aligned_acc
            .checked_add(aligned_size)
            .ok_or(TlsError::Overflow("offset accumulation"))?;
        let tp_offset =
            -(i64::try_from(offset_i).map_err(|_| TlsError::Overflow("offset as i64"))?);

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

        for sym in dynsyms {
            if sym.sym_type == STT_TLS && sym.shndx != SHN_UNDEF && !sym.name.is_empty() {
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

    pub fn init_block(&self) -> &[u8] {
        &self.init_block
    }

    pub fn modules(&self) -> &[TlsModule] {
        &self.modules
    }

    pub fn tp_offset_of(&self, name: &str) -> Option<i64> {
        self.tls_defs
            .get(name)
            .map(|&(tp_offset, value)| tp_offset.wrapping_add(value as i64))
    }
}

pub struct TlsResolver<'a, R: SymbolResolver> {
    inner: &'a R,
    dynsyms: &'a [DynSym],
    layout: &'a TlsLayout,

    own_tp_offset: Option<i64>,
}

impl<'a, R: SymbolResolver> TlsResolver<'a, R> {
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
        if sym_index == 0 {
            return self.own_tp_offset.map(|v| v as u64);
        }
        let sym = self.dynsyms.get(sym_index as usize)?;

        self.layout.tp_offset_of(&sym.name).map(|v| v as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(file_off: u64, file_size: u64, mem_size: u64, align: u64) -> TlsSegment {
        TlsSegment {
            vaddr: file_off,
            file_size,
            mem_size,
            align,
        }
    }

    fn tls_def(name: &str, value: u64) -> DynSym {
        DynSym {
            name: name.to_string(),
            value,
            size: 0,
            bind: 1,
            sym_type: STT_TLS,
            shndx: 1,
        }
    }

    fn tls_undef(name: &str) -> DynSym {
        DynSym {
            shndx: SHN_UNDEF,
            ..tls_def(name, 0)
        }
    }

    struct InnerFixed;
    impl SymbolResolver for InnerFixed {
        fn resolve_symbol(&self, i: u32) -> Option<u64> {
            match i {
                1 => Some(0x7fff_1234_0000),
                _ => None,
            }
        }
        fn resolve_tls_offset(&self, _i: u32) -> Option<u64> {
            None
        }
    }

    #[test]
    fn round_up_matches_alignment_identity() {
        assert_eq!(round_up(0, 16), Some(0));
        assert_eq!(round_up(1, 16), Some(16));
        assert_eq!(round_up(16, 16), Some(16));
        assert_eq!(round_up(17, 16), Some(32));
        assert_eq!(round_up(7, 1), Some(7));
        assert_eq!(round_up(7, 0), Some(7));
    }

    #[test]
    fn single_module_offset_is_minus_roundup_size() {
        let file = b"ABCDEFGH".to_vec();
        let dynsyms = vec![tls_def("x", 4)];
        let mut layout = TlsLayout::new();
        let m = layout
            .add_module(&seg(0, 8, 16, 8), &file, 0, &dynsyms)
            .unwrap();

        assert_eq!(m.tp_offset, -16);
        assert_eq!(m.size, 16);

        assert_eq!(layout.tp_offset_of("x"), Some(-12));

        assert_eq!(&layout.init_block()[0..8], b"ABCDEFGH");
        assert_eq!(&layout.init_block()[8..16], &[0u8; 8]);
    }

    #[test]
    fn single_module_size_is_rounded_up_for_offset() {
        let file = vec![0u8; 5];
        let mut layout = TlsLayout::new();
        let m = layout.add_module(&seg(0, 5, 13, 8), &file, 0, &[]).unwrap();
        assert_eq!(m.tp_offset, -16);
    }

    #[test]
    fn multi_module_stacking_and_alignment() {
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

        assert_eq!(layout.tp_offset_of("a"), Some(-16));
        assert_eq!(layout.tp_offset_of("b"), Some(-28));
        assert_eq!(layout.tp_offset_of("c"), Some(-40));
        assert_eq!(layout.tp_offset_of("missing"), None);
    }

    #[test]
    fn tdata_copied_and_tbss_zeroed_in_assembled_block() {
        let file = vec![0xAAu8; 4];
        let mut layout = TlsLayout::new();
        let m = layout.add_module(&seg(0, 4, 8, 4), &file, 0, &[]).unwrap();
        let block = &layout.init_block()[m.block_offset..m.block_offset + m.size];
        assert_eq!(&block[0..4], &[0xAA; 4]);
        assert_eq!(&block[4..8], &[0u8; 4]);
    }

    #[test]
    fn bad_align_is_typed_err() {
        let mut layout = TlsLayout::new();

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

        assert_eq!(
            layout.add_module(&seg(0, 8, 8, 8), &[0u8; 4], 0, &[]),
            Err(TlsError::TdataOutOfFile(0))
        );
    }

    #[test]
    fn tpoff64_applied_through_reloc_writes_tp_offset_plus_addend() {
        use crate::loader::reloc::{apply_one, Rela, SliceImage, R_X86_64_TPOFF64};

        let file = vec![0u8; 0x20];
        let dynsyms = vec![tls_undef(""), tls_undef("tlsvar")];
        let mut layout = TlsLayout::new();

        layout
            .add_module(&seg(0, 0, 0x40, 0x10), &file, 0, &[tls_def("tlsvar", 0x10)])
            .unwrap();

        let inner = InnerFixed;

        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);

        const BASE: u64 = 0x5555_5000_0000;
        let mut buf = vec![0u8; 8];
        let mut img = SliceImage::new(BASE, 0, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 1,
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
        use crate::loader::reloc::{apply_one, Rela, SliceImage, R_X86_64_TPOFF64};

        let dynsyms = vec![tls_undef("")];
        let layout = TlsLayout::new();
        let inner = InnerFixed;

        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, Some(-0x80));

        const BASE: u64 = 0x5555_5000_0000;
        let mut buf = vec![0u8; 8];
        let mut img = SliceImage::new(BASE, 0, &mut buf);
        let rela = Rela {
            offset: 0,
            sym_index: 0,
            r_type: R_X86_64_TPOFF64,
            addend: 0x40,
        };
        apply_one(&mut img, &resolver, &rela).unwrap();

        let written = u64::from_le_bytes(buf[..8].try_into().unwrap()) as i64;
        assert_eq!(written, -0x80 + 0x40);
        assert_eq!(written, -0x40);

        let no_own = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(no_own.resolve_tls_offset(0), None);
    }

    #[test]
    fn non_tls_symbol_still_goes_through_inner_resolver() {
        let dynsyms = vec![tls_undef(""), tls_undef("ignored")];
        let layout = TlsLayout::new();
        let inner = InnerFixed;

        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);

        assert_eq!(resolver.resolve_symbol(1), Some(0x7fff_1234_0000));
        assert_eq!(resolver.resolve_symbol(2), None);

        assert_eq!(resolver.resolve_tls_offset(0), None);
    }

    #[test]
    fn unresolved_tls_import_is_none() {
        let dynsyms = vec![tls_undef(""), tls_undef("errno")];
        let mut layout = TlsLayout::new();
        layout
            .add_module(&seg(0, 0, 16, 8), &[], 0, &[tls_def("other", 0)])
            .unwrap();
        let inner = InnerFixed;
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(resolver.resolve_tls_offset(1), None);
    }

    #[test]
    fn mem_size_align_round_up_overflow_is_typed_err() {
        let mut layout = TlsLayout::new();
        let err = layout
            .add_module(&seg(0, 0, u64::MAX, 16), &[], 0, &[])
            .unwrap_err();
        assert!(
            matches!(err, TlsError::Overflow(_)),
            "expected Overflow, got {err:?}"
        );
    }

    #[test]
    fn offset_accumulation_overflow_is_typed_err() {
        let mut layout = TlsLayout::new();

        let err = layout
            .add_module(&seg(0, 0, (1u64 << 63) + 8, 8), &[], 0, &[])
            .unwrap_err();
        assert!(
            matches!(err, TlsError::Overflow(_)),
            "expected Overflow for an absurd module size, got {err:?}"
        );
    }

    #[test]
    fn resolver_out_of_range_named_index_is_none() {
        let dynsyms = vec![tls_undef("")];
        let mut layout = TlsLayout::new();
        layout
            .add_module(&seg(0, 0, 16, 8), &[], 0, &[tls_def("x", 0)])
            .unwrap();
        let inner = InnerFixed;
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(resolver.resolve_tls_offset(9), None);
        assert_eq!(resolver.resolve_tls_offset(u32::MAX), None);
    }

    #[test]
    fn sym0_self_reference_without_own_tls_is_none() {
        let dynsyms = vec![tls_undef("")];
        let layout = TlsLayout::new();
        let inner = InnerFixed;
        let resolver = TlsResolver::new(&inner, &dynsyms, &layout, None);
        assert_eq!(resolver.resolve_tls_offset(0), None);
    }
}
