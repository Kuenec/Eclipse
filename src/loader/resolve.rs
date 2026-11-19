use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::CString;

use super::elf::DynSym;
use super::reloc::SymbolResolver;
use crate::abi::Abi;

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

fn glibc_name_for_bionic(abi: Abi, name: &str) -> Option<Cow<'_, str>> {
    match (abi, name) {
        (_, "sigsetjmp") => Some(Cow::Borrowed("__sigsetjmp")),
        (Abi::Arm64V8a, name) => Some(Cow::Borrowed(name)),
        (Abi::X86_64, name) => glibc_binary128_name(name),
    }
}

fn glibc_binary128_name(name: &str) -> Option<Cow<'_, str>> {
    match name {
        "finitel" => Some(Cow::Borrowed("__finitef128")),
        "isinfl" => Some(Cow::Borrowed("__isinff128")),
        "isnanl" => Some(Cow::Borrowed("__isnanf128")),
        "nexttowardl" => Some(Cow::Borrowed("nextafterf128")),
        name if is_glibc_x87_name(name) => binary128_twin(name).map(Cow::Owned),
        other => Some(Cow::Borrowed(other)),
    }
}

fn binary128_twin(name: &str) -> Option<String> {
    for parser in ["strtold", "wcstold"] {
        if let Some((before, after)) = name.split_once(parser) {
            let stem = &parser[..parser.len() - 2];
            return Some(format!("{before}{stem}f128{after}"));
        }
    }
    let (body, reentrant) = match name.strip_suffix("_r") {
        Some(body) => (body, "_r"),
        None => (name, ""),
    };
    let stem = body.strip_suffix('l')?;
    Some(format!("{stem}f128{reentrant}"))
}

fn is_glibc_x87_name(name: &str) -> bool {
    matches!(
        name,
        "fmal"
            | "powl"
            | "strtold"
            | "strtold_l"
            | "wcstold"
            | "__clog10l"
            | "__finitel"
            | "__fpclassifyl"
            | "__iscanonicall"
            | "__iseqsigl"
            | "__isinfl"
            | "__isnanl"
            | "__issignalingl"
            | "__signbitl"
            | "__strtold_internal"
            | "__strtold_l"
            | "__wcstold_internal"
            | "__wcstold_l"
            | "acoshl"
            | "acosl"
            | "acospil"
            | "asinhl"
            | "asinl"
            | "asinpil"
            | "atan2l"
            | "atan2pil"
            | "atanhl"
            | "atanl"
            | "atanpil"
            | "cabsl"
            | "cacoshl"
            | "cacosl"
            | "canonicalizel"
            | "cargl"
            | "casinhl"
            | "casinl"
            | "catanhl"
            | "catanl"
            | "cbrtl"
            | "ccoshl"
            | "ccosl"
            | "ceill"
            | "cexpl"
            | "cimagl"
            | "clog10l"
            | "clogl"
            | "compoundnl"
            | "conjl"
            | "copysignl"
            | "coshl"
            | "cosl"
            | "cospil"
            | "cpowl"
            | "cprojl"
            | "creall"
            | "csinhl"
            | "csinl"
            | "csqrtl"
            | "ctanhl"
            | "ctanl"
            | "daddl"
            | "ddivl"
            | "dfmal"
            | "dmull"
            | "dreml"
            | "dsqrtl"
            | "dsubl"
            | "erfcl"
            | "erfl"
            | "exp10l"
            | "exp10m1l"
            | "exp2l"
            | "exp2m1l"
            | "expl"
            | "expm1l"
            | "fabsl"
            | "faddl"
            | "fdiml"
            | "fdivl"
            | "ffmal"
            | "finitel"
            | "floorl"
            | "fmaximum_mag_numl"
            | "fmaximum_magl"
            | "fmaximum_numl"
            | "fmaximuml"
            | "fmaxl"
            | "fmaxmagl"
            | "fminimum_mag_numl"
            | "fminimum_magl"
            | "fminimum_numl"
            | "fminimuml"
            | "fminl"
            | "fminmagl"
            | "fmodl"
            | "fmull"
            | "frexpl"
            | "fromfpl"
            | "fromfpxl"
            | "fsqrtl"
            | "fsubl"
            | "gammal"
            | "getpayloadl"
            | "hypotl"
            | "ilogbl"
            | "isinfl"
            | "isnanl"
            | "j0l"
            | "j1l"
            | "jnl"
            | "ldexpl"
            | "lgammal"
            | "lgammal_r"
            | "llogbl"
            | "llrintl"
            | "llroundl"
            | "log10l"
            | "log10p1l"
            | "log1pl"
            | "log2l"
            | "log2p1l"
            | "logbl"
            | "logl"
            | "logp1l"
            | "lrintl"
            | "lroundl"
            | "modfl"
            | "nanl"
            | "nearbyintl"
            | "nextafterl"
            | "nextdownl"
            | "nexttoward"
            | "nexttowardf"
            | "nexttowardl"
            | "nextupl"
            | "pownl"
            | "powrl"
            | "qecvt"
            | "qecvt_r"
            | "qfcvt"
            | "qfcvt_r"
            | "qgcvt"
            | "remainderl"
            | "remquol"
            | "rintl"
            | "rootnl"
            | "roundevenl"
            | "roundl"
            | "rsqrtl"
            | "scalbl"
            | "scalblnl"
            | "scalbnl"
            | "setpayloadl"
            | "setpayloadsigl"
            | "significandl"
            | "sincosl"
            | "sinhl"
            | "sinl"
            | "sinpil"
            | "sqrtl"
            | "strfroml"
            | "tanhl"
            | "tanl"
            | "tanpil"
            | "tgammal"
            | "totalorderl"
            | "totalordermagl"
            | "truncl"
            | "ufromfpl"
            | "ufromfpxl"
            | "wcstold_l"
            | "y0l"
            | "y1l"
            | "ynl"
    )
}

pub(super) unsafe fn dlsym_bionic_import(
    handle: *mut libc::c_void,
    name: &str,
) -> Option<ResolvedSym> {
    let cname = CString::new(glibc_name_for_bionic(Abi::HOST, name)?.as_ref()).ok()?;

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
    stands_in_for_missing_functions: bool,
}

impl Scope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, provider: Box<dyn SymbolProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    pub fn stand_in_for_missing_functions(&mut self) -> &mut Self {
        self.stands_in_for_missing_functions = true;
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
            return Some(0);
        }
        if self.scope.stands_in_for_missing_functions && sym.sym_type == STT_FUNC {
            return super::stand_in::for_function(&sym.name);
        }
        None
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
    fn a_missing_function_gets_a_stand_in_only_where_the_scope_allows_it() {
        let object = DynSym {
            sym_type: STT_OBJECT,
            ..undef("eclipse_test_missing_object", STB_GLOBAL)
        };
        let dynsyms = vec![
            undef("AMediaExtractor_seekTo", STB_GLOBAL),
            object,
            undef("eclipse_test_missing_weak", STB_WEAK),
        ];
        let strict = Scope::new();
        assert_eq!(
            ScopedResolver::new(&strict, &dynsyms).resolve_symbol(0),
            None
        );

        let mut lenient = Scope::new();
        lenient.stand_in_for_missing_functions();
        let resolver = ScopedResolver::new(&lenient, &dynsyms);
        let stand_in = resolver.resolve_symbol(0).expect("a stand-in");
        assert_ne!(stand_in, 0);
        assert_eq!(resolver.resolve_symbol(0), Some(stand_in));
        assert_eq!(resolver.resolve_symbol(1), None, "data has no stand-in");
        assert_eq!(resolver.resolve_symbol(2), Some(0), "weak stays null");
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

    #[test]
    fn host_dlsym_binds_sigsetjmp_to_glibcs_entry_point() {
        let glibc = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"__sigsetjmp".as_ptr()) } as u64;
        assert_ne!(glibc, 0, "glibc exports __sigsetjmp");
        assert_eq!(
            HostDlsymProvider.resolve("sigsetjmp"),
            Some(ResolvedSym { addr: glibc })
        );
    }

    #[test]
    fn glibc_names_follow_each_abis_long_double_format() {
        for (name, x86_64, arm64) in [
            ("sigsetjmp", Some("__sigsetjmp"), Some("__sigsetjmp")),
            ("strtold", Some("strtof128"), Some("strtold")),
            ("wcstold_l", Some("wcstof128_l"), Some("wcstold_l")),
            ("powl", Some("powf128"), Some("powl")),
            ("logl", Some("logf128"), Some("logl")),
            ("roundl", Some("roundf128"), Some("roundl")),
            ("llroundl", Some("llroundf128"), Some("llroundl")),
            ("lgammal_r", Some("lgammaf128_r"), Some("lgammal_r")),
            ("isnanl", Some("__isnanf128"), Some("isnanl")),
            ("nexttowardl", Some("nextafterf128"), Some("nexttowardl")),
            ("nexttoward", None, Some("nexttoward")),
            ("qecvt_r", None, Some("qecvt_r")),
            ("memcpy", Some("memcpy"), Some("memcpy")),
        ] {
            assert_eq!(
                glibc_name_for_bionic(Abi::X86_64, name).as_deref(),
                x86_64,
                "{name}"
            );
            assert_eq!(
                glibc_name_for_bionic(Abi::Arm64V8a, name).as_deref(),
                arm64,
                "{name}"
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    type Binary128 = core::arch::x86_64::__m128;

    #[cfg(target_arch = "aarch64")]
    type Binary128 = core::arch::aarch64::uint64x2_t;

    fn binary128(high: u64) -> Binary128 {
        unsafe { std::mem::transmute::<[u64; 2], Binary128>([0, high]) }
    }

    fn binary128_bits(value: Binary128) -> [u64; 2] {
        unsafe { std::mem::transmute::<Binary128, [u64; 2]>(value) }
    }

    #[test]
    fn bionic_long_double_imports_bind_to_glibc_binary128_entry_points() {
        use std::ffi::{c_char, c_int};

        const TWO: u64 = 0x4000_0000_0000_0000;
        const TWO_AND_A_HALF: u64 = 0x4000_4000_0000_0000;
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
        let powl: extern "C" fn(Binary128, Binary128) -> Binary128 =
            unsafe { std::mem::transmute(addr("powl")) };
        let fmal: extern "C" fn(Binary128, Binary128, Binary128) -> Binary128 =
            unsafe { std::mem::transmute(addr("fmal")) };
        let strtold: unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> Binary128 =
            unsafe { std::mem::transmute(addr("strtold")) };
        let strtold_l: unsafe extern "C" fn(
            *const c_char,
            *mut *mut c_char,
            libc::locale_t,
        ) -> Binary128 = unsafe { std::mem::transmute(addr("strtold_l")) };
        let wcstold: unsafe extern "C" fn(
            *const libc::wchar_t,
            *mut *mut libc::wchar_t,
        ) -> Binary128 = unsafe { std::mem::transmute(addr("wcstold")) };
        let newlocale: unsafe extern "C" fn(
            c_int,
            *const c_char,
            libc::locale_t,
        ) -> libc::locale_t = unsafe { std::mem::transmute(addr("newlocale")) };
        let roundl: extern "C" fn(Binary128) -> Binary128 =
            unsafe { std::mem::transmute(addr("roundl")) };
        let llroundl: extern "C" fn(Binary128) -> std::ffi::c_longlong =
            unsafe { std::mem::transmute(addr("llroundl")) };

        assert_eq!(
            binary128_bits(roundl(binary128(TWO_AND_A_HALF))),
            [0, THREE]
        );
        assert_eq!(llroundl(binary128(FIFTEEN_HUNDRED)), 1500);

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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn host_dlsym_binds_long_double_imports_to_binary128_twins_never_to_x87() {
        let p = HostDlsymProvider;
        let host = |name: &str| {
            let cname = std::ffi::CString::new(name).unwrap();
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) as u64 }
        };
        for (name, twin) in [
            ("logl", Some("logf128")),
            ("sqrtl", Some("sqrtf128")),
            ("roundl", Some("roundf128")),
            ("llroundl", Some("llroundf128")),
            ("nexttoward", None),
            ("wcstold_l", Some("wcstof128_l")),
            ("cabsl", Some("cabsf128")),
            ("__isnanl", Some("__isnanf128")),
            ("exp10l", Some("exp10f128")),
            ("roundevenl", Some("roundevenf128")),
            ("nextupl", Some("nextupf128")),
            ("j0l", Some("j0f128")),
            ("ynl", Some("ynf128")),
            ("llogbl", Some("llogbf128")),
            ("totalorderl", Some("totalorderf128")),
            ("canonicalizel", Some("canonicalizef128")),
            ("fromfpl", Some("fromfpf128")),
            ("getpayloadl", Some("getpayloadf128")),
            ("clog10l", Some("clog10f128")),
            ("strfroml", Some("strfromf128")),
            ("__issignalingl", Some("__issignalingf128")),
            ("__strtold_internal", Some("__strtof128_internal")),
            ("gammal", None),
            ("dreml", None),
            ("scalbl", None),
            ("faddl", None),
            ("dfmal", None),
            ("qecvt", None),
            ("qfcvt_r", None),
            ("qgcvt", None),
        ] {
            let x87 = host(name);
            assert_ne!(x87, 0, "glibc exports {name}");
            let binary128 = twin.map(host).filter(|addr| *addr != 0);
            assert_eq!(p.resolve(name).map(|found| found.addr), binary128, "{name}");
            assert_ne!(binary128, Some(x87), "{name} has its own binary128 entry");
        }
        for name in ["logl", "sqrtl", "roundl", "llroundl"] {
            assert!(p.resolve(name).is_some(), "{name} binds");
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
