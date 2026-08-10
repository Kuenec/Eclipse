use std::collections::HashMap;
use std::ffi::CString;

use super::elf::DynSym;
use super::reloc::SymbolResolver;

const STB_LOCAL: u8 = 0;

const STB_GLOBAL: u8 = 1;

const STB_WEAK: u8 = 2;

const STT_NOTYPE: u8 = 0;

const STT_OBJECT: u8 = 1;

const STT_FUNC: u8 = 2;

const STT_GNU_IFUNC: u8 = 10;

const SHN_UNDEF: u16 = 0;

const SHN_ABS: u16 = 0xfff1;

fn is_exported_definition(sym: &DynSym) -> bool {
    if sym.name.is_empty() {
        return false;
    }
    if sym.shndx == SHN_UNDEF || sym.shndx == SHN_ABS {
        return false;
    }
    let bind_ok = matches!(sym.bind, STB_GLOBAL | STB_WEAK);
    let type_ok = matches!(
        sym.sym_type,
        STT_NOTYPE | STT_OBJECT | STT_FUNC | STT_GNU_IFUNC
    );
    bind_ok && type_ok
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSym {
    pub addr: u64,

    pub weak: bool,
}

pub trait SymbolProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym>;
}

pub struct LoadedObjectProvider {
    base: u64,

    defs: HashMap<String, (u64, bool)>,
}

impl LoadedObjectProvider {
    pub fn new(base: u64, dynsyms: &[DynSym]) -> Self {
        let mut defs: HashMap<String, (u64, bool)> = HashMap::new();
        for sym in dynsyms {
            if !is_exported_definition(sym) {
                continue;
            }
            let weak = sym.bind == STB_WEAK;
            match defs.get(&sym.name) {
                Some((_, false)) if weak => {}
                _ => {
                    defs.insert(sym.name.clone(), (sym.value, weak));
                }
            }
        }
        Self { base, defs }
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn definition_count(&self) -> usize {
        self.defs.len()
    }
}

impl SymbolProvider for LoadedObjectProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        self.defs.get(name).map(|&(value, weak)| ResolvedSym {
            addr: self.base.wrapping_add(value),
            weak,
        })
    }
}

pub struct HostDlsymProvider;

impl SymbolProvider for HostDlsymProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        let cname = CString::new(name).ok()?;

        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(ResolvedSym {
                addr: ptr as u64,
                weak: false,
            })
        }
    }
}

#[derive(Default)]
pub struct Scope {
    providers: Vec<Box<dyn SymbolProvider>>,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn push(&mut self, provider: Box<dyn SymbolProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    pub fn into_providers(self) -> Vec<Box<dyn SymbolProvider>> {
        self.providers
    }

    pub fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        let mut weak_hit: Option<ResolvedSym> = None;
        for p in &self.providers {
            if let Some(found) = p.resolve(name) {
                if !found.weak {
                    return Some(found);
                }

                weak_hit.get_or_insert(found);
            }
        }
        weak_hit
    }
}

pub struct ScopedResolver<'a> {
    scope: &'a Scope,
    dynsyms: &'a [DynSym],
}

impl<'a> ScopedResolver<'a> {
    pub fn new(scope: &'a Scope, dynsyms: &'a [DynSym]) -> Self {
        Self { scope, dynsyms }
    }

    fn sym(&self, sym_index: u32) -> Option<&DynSym> {
        self.dynsyms.get(sym_index as usize)
    }
}

impl SymbolResolver for ScopedResolver<'_> {
    fn resolve_symbol(&self, sym_index: u32) -> Option<u64> {
        let sym = self.sym(sym_index)?;

        if sym.bind == STB_LOCAL {
            return None;
        }
        if let Some(found) = self.scope.resolve(&sym.name) {
            return Some(found.addr);
        }

        if sym.bind == STB_WEAK {
            Some(0)
        } else {
            None
        }
    }

    fn resolve_tls_offset(&self, _sym_index: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, value: u64) -> DynSym {
        DynSym {
            name: name.to_string(),
            value,
            size: 0,
            bind: STB_GLOBAL,
            sym_type: STT_FUNC,
            shndx: 1,
        }
    }

    fn weak_def(name: &str, value: u64) -> DynSym {
        DynSym {
            bind: STB_WEAK,
            ..def(name, value)
        }
    }

    fn undef(name: &str, bind: u8) -> DynSym {
        DynSym {
            name: name.to_string(),
            value: 0,
            size: 0,
            bind,
            sym_type: STT_FUNC,
            shndx: SHN_UNDEF,
        }
    }

    fn local_def(name: &str, value: u64) -> DynSym {
        DynSym {
            bind: STB_LOCAL,
            ..def(name, value)
        }
    }

    #[test]
    fn loaded_provider_resolves_defined_exports_only() {
        let syms = vec![
            def("exported", 0x100),
            local_def("private", 0x200),
            undef("imported", STB_GLOBAL),
        ];
        let p = LoadedObjectProvider::new(0x1000, &syms);

        assert_eq!(
            p.resolve("exported"),
            Some(ResolvedSym {
                addr: 0x1100,
                weak: false
            })
        );

        assert_eq!(p.resolve("private"), None);

        assert_eq!(p.resolve("imported"), None);

        assert_eq!(p.definition_count(), 1);
    }

    #[test]
    fn loaded_provider_skips_abs_and_tracks_weak() {
        let abs = DynSym {
            shndx: SHN_ABS,
            ..def("absval", 0x10)
        };
        let syms = vec![abs, weak_def("w", 0x20)];
        let p = LoadedObjectProvider::new(0x1000, &syms);
        assert_eq!(p.resolve("absval"), None);
        assert_eq!(
            p.resolve("w"),
            Some(ResolvedSym {
                addr: 0x1020,
                weak: true
            })
        );
    }

    struct Fixed(Vec<(&'static str, ResolvedSym)>);
    impl SymbolProvider for Fixed {
        fn resolve(&self, name: &str) -> Option<ResolvedSym> {
            self.0.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
        }
    }

    fn strong(addr: u64) -> ResolvedSym {
        ResolvedSym { addr, weak: false }
    }
    fn weak(addr: u64) -> ResolvedSym {
        ResolvedSym { addr, weak: true }
    }

    #[test]
    fn scope_first_strong_match_wins() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", strong(0xA))])))
            .push(Box::new(Fixed(vec![("f", strong(0xB))])));

        assert_eq!(scope.resolve("f"), Some(strong(0xA)));
    }

    #[test]
    fn scope_global_beats_earlier_weak() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", weak(0xA))])))
            .push(Box::new(Fixed(vec![("f", strong(0xB))])));

        assert_eq!(scope.resolve("f"), Some(strong(0xB)));
    }

    #[test]
    fn scope_only_weak_returns_first_weak() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", weak(0xA))])))
            .push(Box::new(Fixed(vec![("f", weak(0xB))])));

        assert_eq!(scope.resolve("f"), Some(weak(0xA)));
    }

    #[test]
    fn scope_no_match_is_none() {
        let mut scope = Scope::new();
        scope.push(Box::new(Fixed(vec![("g", strong(0xA))])));
        assert_eq!(scope.resolve("f"), None);
    }

    #[test]
    fn resolver_resolves_defined_symbol_to_base_plus_value() {
        let dynsyms = vec![def("self", 0x300)];
        let mut scope = Scope::new();
        scope.push(Box::new(LoadedObjectProvider::new(0x4000, &dynsyms)));
        let r = ScopedResolver::new(&scope, &dynsyms);

        assert_eq!(r.resolve_symbol(0), Some(0x4300));
    }

    #[test]
    fn resolver_weak_undef_resolves_to_zero() {
        let dynsyms = vec![undef("weakimp", STB_WEAK)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), Some(0));
    }

    #[test]
    fn resolver_strong_undef_is_unresolved() {
        let dynsyms = vec![undef("strongimp", STB_GLOBAL)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), None);
    }

    #[test]
    fn resolver_local_reference_is_not_globally_resolved() {
        let dynsyms = vec![local_def("loc", 0x10)];
        let mut scope = Scope::new();

        scope.push(Box::new(Fixed(vec![("loc", strong(0x99))])));
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), None);
    }

    #[test]
    fn resolver_out_of_range_index_is_unresolved() {
        let dynsyms = vec![def("only", 0x10)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);

        assert_eq!(r.resolve_symbol(5), None);
    }

    #[test]
    fn resolver_never_resolves_tls_offset() {
        let dynsyms = vec![def("x", 0x10)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);

        assert_eq!(r.resolve_tls_offset(0), None);
    }

    #[test]
    fn host_dlsym_resolves_known_libc_symbol() {
        let p = HostDlsymProvider;

        let got = p.resolve("memcpy");
        assert!(got.is_some(), "dlsym must resolve a known libc symbol");
        let got = got.unwrap();
        assert!(got.addr != 0, "resolved address must be non-null");
        assert!(!got.weak, "host dlsym definitions are treated as strong");

        assert!(p.resolve("malloc").is_some_and(|s| s.addr != 0));
    }

    #[test]
    fn host_dlsym_returns_none_for_gibberish() {
        let p = HostDlsymProvider;
        assert_eq!(
            p.resolve("__eclipse_definitely_no_such_symbol_4f2a9c__"),
            None
        );

        assert_eq!(p.resolve("bad\0name"), None);
    }

    #[test]
    fn resolver_u32_max_index_is_unresolved_no_panic() {
        let dynsyms = vec![def("only", 0x10)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(u32::MAX), None);
        assert_eq!(r.resolve_tls_offset(u32::MAX), None);
    }

    #[test]
    fn resolver_empty_dynsyms_resolves_nothing() {
        let dynsyms: Vec<DynSym> = Vec::new();
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), None);
        assert_eq!(r.resolve_symbol(7), None);
    }

    #[test]
    fn provider_base_plus_value_overflow_wraps_no_panic() {
        let dynsyms = vec![def("s", u64::MAX)];
        let p = LoadedObjectProvider::new(0x1000, &dynsyms);
        assert_eq!(
            p.resolve("s"),
            Some(ResolvedSym {
                addr: 0x1000u64.wrapping_add(u64::MAX),
                weak: false,
            })
        );
    }

    #[test]
    fn provider_handles_name_with_embedded_nul_safely() {
        let mut sym = def("a\0b", 0x10);
        sym.name = "a\0b".to_string();
        let p = LoadedObjectProvider::new(0x1000, &[sym]);
        assert!(p.resolve("a\0b").is_some());
        assert_eq!(p.resolve("a"), None);
    }
}
