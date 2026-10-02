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
        self.defs.get(name).map(|&(value, _)| ResolvedSym {
            addr: self.base.wrapping_add(value),
        })
    }
}

pub struct HostDlsymProvider;

fn glibc_name_for_bionic(name: &str) -> Option<&str> {
    match name {
        "fmal" => Some("fmaf128"),
        "powl" => Some("powf128"),
        "strtold" => Some("strtof128"),
        "strtold_l" => Some("strtof128_l"),
        "wcstold" => Some("wcstof128"),
        "__clog10l" | "__finitel" | "__fpclassifyl" | "__iscanonicall" | "__iseqsigl"
        | "__isinfl" | "__isnanl" | "__issignalingl" | "__signbitl" | "__strtold_internal"
        | "__strtold_l" | "__wcstold_internal" | "__wcstold_l" | "acoshl" | "acosl" | "acospil"
        | "asinhl" | "asinl" | "asinpil" | "atan2l" | "atan2pil" | "atanhl" | "atanl"
        | "atanpil" | "cabsl" | "cacoshl" | "cacosl" | "canonicalizel" | "cargl" | "casinhl"
        | "casinl" | "catanhl" | "catanl" | "cbrtl" | "ccoshl" | "ccosl" | "ceill" | "cexpl"
        | "cimagl" | "clog10l" | "clogl" | "compoundnl" | "conjl" | "copysignl" | "coshl"
        | "cosl" | "cospil" | "cpowl" | "cprojl" | "creall" | "csinhl" | "csinl" | "csqrtl"
        | "ctanhl" | "ctanl" | "daddl" | "ddivl" | "dfmal" | "dmull" | "dreml" | "dsqrtl"
        | "dsubl" | "erfcl" | "erfl" | "exp10l" | "exp10m1l" | "exp2l" | "exp2m1l" | "expl"
        | "expm1l" | "fabsl" | "faddl" | "fdiml" | "fdivl" | "ffmal" | "finitel" | "floorl"
        | "fmaximum_mag_numl" | "fmaximum_magl" | "fmaximum_numl" | "fmaximuml" | "fmaxl"
        | "fmaxmagl" | "fminimum_mag_numl" | "fminimum_magl" | "fminimum_numl" | "fminimuml"
        | "fminl" | "fminmagl" | "fmodl" | "fmull" | "frexpl" | "fromfpl" | "fromfpxl"
        | "fsqrtl" | "fsubl" | "gammal" | "getpayloadl" | "hypotl" | "ilogbl" | "isinfl"
        | "isnanl" | "j0l" | "j1l" | "jnl" | "ldexpl" | "lgammal" | "lgammal_r" | "llogbl"
        | "llrintl" | "llroundl" | "log10l" | "log10p1l" | "log1pl" | "log2l" | "log2p1l"
        | "logbl" | "logl" | "logp1l" | "lrintl" | "lroundl" | "modfl" | "nanl" | "nearbyintl"
        | "nextafterl" | "nextdownl" | "nexttoward" | "nexttowardf" | "nexttowardl" | "nextupl"
        | "pownl" | "powrl" | "qecvt" | "qecvt_r" | "qfcvt" | "qfcvt_r" | "qgcvt"
        | "remainderl" | "remquol" | "rintl" | "rootnl" | "roundevenl" | "roundl" | "rsqrtl"
        | "scalbl" | "scalblnl" | "scalbnl" | "setpayloadl" | "setpayloadsigl" | "significandl"
        | "sincosl" | "sinhl" | "sinl" | "sinpil" | "sqrtl" | "strfroml" | "tanhl" | "tanl"
        | "tanpil" | "tgammal" | "totalorderl" | "totalordermagl" | "truncl" | "ufromfpl"
        | "ufromfpxl" | "wcstold_l" | "y0l" | "y1l" | "ynl" => None,
        other => Some(other),
    }
}

pub(super) unsafe fn dlsym_bionic_import(
    handle: *mut libc::c_void,
    name: &str,
) -> Option<ResolvedSym> {
    let cname = CString::new(glibc_name_for_bionic(name)?).ok()?;

    let ptr = unsafe { libc::dlsym(handle, cname.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(ResolvedSym { addr: ptr as u64 })
    }
}

impl SymbolProvider for HostDlsymProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        unsafe { dlsym_bionic_import(libc::RTLD_DEFAULT, name) }
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
        self.providers
            .iter()
            .find_map(|provider| provider.resolve(name))
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

        assert_eq!(p.resolve("exported"), Some(ResolvedSym { addr: 0x1100 }));

        assert_eq!(p.resolve("private"), None);

        assert_eq!(p.resolve("imported"), None);

        assert_eq!(p.definition_count(), 1);
    }

    #[test]
    fn loaded_provider_skips_abs_and_exports_weak() {
        let abs = DynSym {
            shndx: SHN_ABS,
            ..def("absval", 0x10)
        };
        let syms = vec![abs, weak_def("w", 0x20)];
        let p = LoadedObjectProvider::new(0x1000, &syms);
        assert_eq!(p.resolve("absval"), None);
        assert_eq!(p.resolve("w"), Some(ResolvedSym { addr: 0x1020 }));
    }

    struct Fixed(Vec<(&'static str, ResolvedSym)>);
    impl SymbolProvider for Fixed {
        fn resolve(&self, name: &str) -> Option<ResolvedSym> {
            self.0.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
        }
    }

    fn found(addr: u64) -> ResolvedSym {
        ResolvedSym { addr }
    }

    #[test]
    fn scope_first_match_wins() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", found(0xA))])))
            .push(Box::new(Fixed(vec![("f", found(0xB))])));

        assert_eq!(scope.resolve("f"), Some(found(0xA)));
    }

    #[test]
    fn scope_earlier_weak_definition_beats_later_global_definition() {
        let dynsyms = vec![weak_def("_Znwm", 0x40), undef("_Znwm", STB_GLOBAL)];
        let mut scope = Scope::new();
        scope
            .push(Box::new(LoadedObjectProvider::new(0x1000, &dynsyms)))
            .push(Box::new(Fixed(vec![("_Znwm", found(0x7f00_0000))])));

        assert_eq!(scope.resolve("_Znwm"), Some(found(0x1040)));
        assert_eq!(
            ScopedResolver::new(&scope, &dynsyms).resolve_symbol(1),
            Some(0x1040)
        );
    }

    #[test]
    fn scope_no_match_is_none() {
        let mut scope = Scope::new();
        scope.push(Box::new(Fixed(vec![("g", found(0xA))])));
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

        scope.push(Box::new(Fixed(vec![("loc", found(0x99))])));
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

    fn binary128(high: u64) -> core::arch::x86_64::__m128 {
        unsafe { std::mem::transmute::<[u64; 2], core::arch::x86_64::__m128>([0, high]) }
    }

    fn binary128_bits(value: core::arch::x86_64::__m128) -> [u64; 2] {
        unsafe { std::mem::transmute::<core::arch::x86_64::__m128, [u64; 2]>(value) }
    }

    #[test]
    fn bionic_long_double_imports_bind_to_glibc_binary128_entry_points() {
        use core::arch::x86_64::__m128;
        use std::ffi::{c_char, c_int};

        const TWO: u64 = 0x4000_0000_0000_0000;
        const THREE: u64 = 0x4000_8000_0000_0000;
        const FOUR: u64 = 0x4001_0000_0000_0000;
        const TEN: u64 = 0x4002_4000_0000_0000;
        const ONE_HUNDRED: u64 = 0x4005_9000_0000_0000;
        const ONE_THOUSAND_TWENTY_FOUR: u64 = 0x4009_0000_0000_0000;
        const FIFTEEN_HUNDRED: u64 = 0x4009_7700_0000_0000;

        let env = super::super::bionic_env::BionicEnv::with_host_baseline(true, true);
        let addr = |name: &str| {
            env.scope()
                .resolve(name)
                .unwrap_or_else(|| panic!("{name} must resolve"))
                .addr as usize
        };
        let powl: extern "C" fn(__m128, __m128) -> __m128 =
            unsafe { std::mem::transmute(addr("powl")) };
        let fmal: extern "C" fn(__m128, __m128, __m128) -> __m128 =
            unsafe { std::mem::transmute(addr("fmal")) };
        let strtold: unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> __m128 =
            unsafe { std::mem::transmute(addr("strtold")) };
        let strtold_l: unsafe extern "C" fn(
            *const c_char,
            *mut *mut c_char,
            libc::locale_t,
        ) -> __m128 = unsafe { std::mem::transmute(addr("strtold_l")) };
        let wcstold: unsafe extern "C" fn(*const libc::wchar_t, *mut *mut libc::wchar_t) -> __m128 =
            unsafe { std::mem::transmute(addr("wcstold")) };
        let newlocale: unsafe extern "C" fn(
            c_int,
            *const c_char,
            libc::locale_t,
        ) -> libc::locale_t = unsafe { std::mem::transmute(addr("newlocale")) };

        assert_eq!(
            binary128_bits(powl(binary128(TEN), binary128(TWO))),
            [0, ONE_HUNDRED]
        );
        assert_eq!(
            binary128_bits(powl(binary128(TWO), binary128(TEN))),
            [0, ONE_THOUSAND_TWENTY_FOUR]
        );
        assert_eq!(
            binary128_bits(fmal(binary128(TWO), binary128(THREE), binary128(FOUR))),
            [0, TEN]
        );
        assert_eq!(
            binary128_bits(unsafe { strtold(c"1.5e3".as_ptr(), std::ptr::null_mut()) }),
            [0, FIFTEEN_HUNDRED]
        );
        let locale = unsafe { newlocale(libc::LC_ALL_MASK, c"C".as_ptr(), std::ptr::null_mut()) };
        assert!(!locale.is_null(), "the C locale must exist");
        let parsed = unsafe { strtold_l(c"1.5e3".as_ptr(), std::ptr::null_mut(), locale) };
        unsafe { libc::freelocale(locale) };
        assert_eq!(binary128_bits(parsed), [0, FIFTEEN_HUNDRED]);
        let wide: Vec<libc::wchar_t> = "1.5e3\0".chars().map(|c| c as libc::wchar_t).collect();
        assert_eq!(
            binary128_bits(unsafe { wcstold(wide.as_ptr(), std::ptr::null_mut()) }),
            [0, FIFTEEN_HUNDRED]
        );
    }

    #[test]
    fn host_dlsym_refuses_glibc_x87_long_double_functions() {
        let p = HostDlsymProvider;
        for name in [
            "logl",
            "sqrtl",
            "nexttoward",
            "wcstold_l",
            "cabsl",
            "__isnanl",
            "exp10l",
            "roundevenl",
            "nextupl",
            "j0l",
            "ynl",
            "llogbl",
            "totalorderl",
            "canonicalizel",
            "fromfpl",
            "getpayloadl",
            "clog10l",
            "strfroml",
            "__issignalingl",
            "__strtold_internal",
            "gammal",
            "dreml",
            "scalbl",
            "faddl",
            "dfmal",
            "qecvt",
            "qfcvt_r",
            "qgcvt",
        ] {
            let cname = std::ffi::CString::new(name).unwrap();
            assert!(
                unsafe { !libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()).is_null() },
                "glibc exports {name}"
            );
            assert_eq!(
                p.resolve(name),
                None,
                "{name} must not bind to glibc's x87 version"
            );
        }
        for name in [
            "strtol",
            "strtol_l",
            "wcstol",
            "wcstol_l",
            "__strtol_internal",
            "atol",
            "labs",
            "signal",
        ] {
            assert!(
                p.resolve(name).is_some_and(|s| s.addr != 0),
                "{name} still binds"
            );
        }
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
