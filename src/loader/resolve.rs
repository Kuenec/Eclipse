//! Pure-Rust ELF symbol-resolution scope — the loader's fifth piece (the `SymbolResolver`).
//!
//! 2026-06-05: [`map`](super::map) maps a `.so` and applies the **base-only** relocations
//! (`R_X86_64_RELATIVE` + `DT_RELR`), deferring the **symbol-dependent** ones — `GLOB_DAT`,
//! `JUMP_SLOT`, `R_X86_64_64` — because they reference dynamic symbols, not just the load base.
//! This module supplies the missing seam: a [`reloc::SymbolResolver`] backed by an ordered
//! [`Scope`] of pluggable [`SymbolProvider`]s, so those relocations now resolve + apply.
//!
//! ## Clean-room provenance
//! Every rule below is from the **public** System V gABI symbol-resolution semantics and the
//! **x86-64 psABI**, plus the public `dlsym(3)`/`RTLD_DEFAULT` contract — general ELF/POSIX
//! knowledge. No dynamic-linker source was read.
//!
//! ## The resolution model (System V gABI, the parts that matter here)
//! A relocation names a symbol by its **dynamic-symtab index** in the *relocated* object. Each
//! `Elf64_Sym` carries a name, a binding (`STB_LOCAL`/`STB_GLOBAL`/`STB_WEAK`), a type
//! (`STT_FUNC`/`STT_OBJECT`/`STT_NOTYPE`/`STT_GNU_IFUNC`/…), and a section index (`st_shndx`,
//! where `SHN_UNDEF` = 0 marks an *imported* — undefined — symbol). The loader resolves the
//! **name** against a search scope of objects/providers and writes the winning definition's
//! address into the relocated slot. The rules implemented:
//!
//! 1. **Only defined, exported symbols satisfy a reference.** A provider contributes a name only
//!    if it *defines* that symbol (`st_shndx != SHN_UNDEF`) and exports it (binding `GLOBAL` or
//!    `WEAK`; `LOCAL` symbols are file-private and never resolve a cross-object reference). The
//!    type must be one that has an address: `FUNC`/`OBJECT`/`NOTYPE`/`GNU_IFUNC`. `TLS` symbols
//!    are not resolved through this non-TLS path (`TPOFF64` is a separate, deferred step). `ABS`
//!    section symbols are not load-relocated and are skipped here.
//! 2. **First scope match wins — except a strong definition always beats a weak one.** Scanning
//!    the scope in order, the first definition found is provisional; if it is **weak**, scanning
//!    continues so a later **global** (strong) definition can override it (the gABI rule that a
//!    strong symbol anywhere in scope wins over a weak one). A global stops the scan immediately.
//! 3. **A reference to a WEAK undefined symbol with no definition in scope resolves to 0.** The
//!    psABI's weak-undef value is the null address — this is *not* an error (e.g. `__gmon_start__`,
//!    `_ITM_*` left unbound). [`ScopedResolver::resolve_symbol`] returns `Some(0)` for it.
//! 4. **A reference to a STRONG undefined symbol with no definition in scope is unresolved.**
//!    [`ScopedResolver::resolve_symbol`] returns `None`, which [`reloc::apply_one`] turns into a
//!    typed [`reloc::RelocError::UnresolvedSymbol`] — we never fabricate an address.
//!
//! ## The self-reference pattern (why a `LoadedObjectProvider` of the object itself matters)
//! A `.so`'s dynamic symtab often contains **both** a defined and an undefined entry for the same
//! name (e.g. `libm.so.6` lists `__signgam` UND for its `GLOB_DAT` reference *and* `__signgam`
//! DEFINED as one of its exports). A relocation references the UND index; resolving by **name**
//! through a scope that includes a [`LoadedObjectProvider`] of the object itself finds the defined
//! entry — exactly how the dynamic linker satisfies an object's references to its own globals.
//!
//! ## Safety
//! Only [`HostDlsymProvider`] needs `unsafe` (the `dlsym` FFI), confined to one small block with a
//! dated `// SAFETY:` note. The scope/provider arithmetic is otherwise plain safe Rust; `reloc.rs`
//! and `elf.rs` stay `#![forbid(unsafe_code)]`.

use std::collections::HashMap;
use std::ffi::CString;

use super::elf::DynSym;
use super::reloc::SymbolResolver;

// ---- Symbol binding / type / section constants (public System V gABI) ---------------------------

/// `st_info >> 4` value `STB_LOCAL`: file-private, never satisfies a cross-object reference.
const STB_LOCAL: u8 = 0;
/// `st_info >> 4` value `STB_GLOBAL`: a strong, exported definition.
const STB_GLOBAL: u8 = 1;
/// `st_info >> 4` value `STB_WEAK`: a weak, exported definition (a global overrides it).
const STB_WEAK: u8 = 2;

/// `st_info & 0xf` value `STT_NOTYPE`: an untyped symbol (still has an address).
const STT_NOTYPE: u8 = 0;
/// `st_info & 0xf` value `STT_OBJECT`: a data object.
const STT_OBJECT: u8 = 1;
/// `st_info & 0xf` value `STT_FUNC`: a function.
const STT_FUNC: u8 = 2;
/// `st_info & 0xf` value `STT_GNU_IFUNC`: an indirect (ifunc) function — has an address (its
/// resolver), which is what a `GLOB_DAT`/`JUMP_SLOT` against it would take if defined here.
const STT_GNU_IFUNC: u8 = 10;

/// `st_shndx` value `SHN_UNDEF`: the symbol is *undefined* (imported), not a definition.
const SHN_UNDEF: u16 = 0;
/// `st_shndx` value `SHN_ABS`: an absolute (non-load-relocated) value — skipped by this provider.
const SHN_ABS: u16 = 0xfff1;

/// True if a dynamic symbol is a **defined, exported** symbol this resolver can hand out: it has a
/// real section (`st_shndx != SHN_UNDEF`, and not the absolute pseudo-section), an exported binding
/// (`GLOBAL`/`WEAK`, never `LOCAL`), and an addressable type (`FUNC`/`OBJECT`/`NOTYPE`/`GNU_IFUNC`).
fn is_exported_definition(sym: &DynSym) -> bool {
    if sym.name.is_empty() {
        return false; // the null symbol / unnamed locals never resolve a named reference
    }
    if sym.shndx == SHN_UNDEF || sym.shndx == SHN_ABS {
        return false; // undefined (import) or absolute → not a load-relocated definition
    }
    let bind_ok = matches!(sym.bind, STB_GLOBAL | STB_WEAK);
    let type_ok = matches!(
        sym.sym_type,
        STT_NOTYPE | STT_OBJECT | STT_FUNC | STT_GNU_IFUNC
    );
    bind_ok && type_ok
}

/// A symbol definition a provider found, carrying whether the definition is **weak** so the
/// [`Scope`] can apply the gABI "a global anywhere beats a weak" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSym {
    /// The symbol's run-time address.
    pub addr: u64,
    /// True if the definition's binding is `STB_WEAK` (a later `STB_GLOBAL` definition overrides
    /// it). Host (`dlsym`) definitions are treated as strong.
    pub weak: bool,
}

/// A source of symbol definitions, looked up by name. The two concrete providers are a mapped
/// Eclipse-loaded object ([`LoadedObjectProvider`]) and the host process's symbols
/// ([`HostDlsymProvider`]); a [`Scope`] is an ordered list of them.
pub trait SymbolProvider {
    /// Resolve `name` to a definition this provider supplies, or `None` if it defines no such
    /// exported symbol. Implementations must only return *defined, exported* symbols.
    fn resolve(&self, name: &str) -> Option<ResolvedSym>;
}

/// A provider wrapping one mapped object: its load base plus a name→(within-object value, weak)
/// map of the symbols it **defines and exports**. `resolve(name)` returns `base + value`.
///
/// Built from the object's decoded [`DynSym`] table ([`super::elf::ElfImage::dynsyms`]) and its
/// run-time load base ([`super::map::MappedObject::load_base`]). Only [`is_exported_definition`]
/// symbols are indexed; imports/locals/abs are excluded so this never satisfies a reference with a
/// non-definition. If both a weak and a global definition of one name exist in the same object, the
/// global is kept (a defining object should not also offer the weaker one).
pub struct LoadedObjectProvider {
    base: u64,
    /// name → (st_value within the object, is_weak).
    defs: HashMap<String, (u64, bool)>,
}

impl LoadedObjectProvider {
    /// Build a provider from a mapped object's load `base` and its decoded dynamic symbols.
    pub fn new(base: u64, dynsyms: &[DynSym]) -> Self {
        let mut defs: HashMap<String, (u64, bool)> = HashMap::new();
        for sym in dynsyms {
            if !is_exported_definition(sym) {
                continue;
            }
            let weak = sym.bind == STB_WEAK;
            match defs.get(&sym.name) {
                // Keep an existing strong definition over a new weak one; let a new strong replace
                // a previously stored weak. (Within one object, a strong def is the canonical one.)
                Some((_, false)) if weak => {}
                _ => {
                    defs.insert(sym.name.clone(), (sym.value, weak));
                }
            }
        }
        Self { base, defs }
    }

    /// The object's load base (the value added to each symbol's within-object `st_value`).
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Number of exported definitions this provider offers (for reporting/tests).
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

/// A provider that resolves a name via the **host process's** dynamic symbols
/// (`dlsym(RTLD_DEFAULT, name)`). This satisfies a relocated object's imports from already-loaded
/// libraries (e.g. a glibc `.so` resolving its libc imports — `malloc`/`memcpy`/…) and models the
/// "already-loaded provider" tier of the scope. A non-null `dlsym` result is treated as a strong
/// definition (host symbols are real, exported definitions). A null result (no such symbol, or a
/// non-`dlsym`-visible private symbol) is `None`, so the scope falls through to weak-undef = 0 or a
/// typed unresolved-strong error per the gABI rules.
pub struct HostDlsymProvider;

impl SymbolProvider for HostDlsymProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        // A name containing an interior NUL can never be a valid C symbol → not found.
        let cname = CString::new(name).ok()?;
        // SAFETY: 2026-06-05 — `dlsym(RTLD_DEFAULT, ptr)` reads a NUL-terminated C string at `ptr`
        // and returns the symbol's address or NULL. `cname` owns a valid NUL-terminated buffer that
        // outlives the call; `RTLD_DEFAULT` is the standard pseudo-handle for the default search
        // order. The call has no other side effects and cannot write through our pointer. The
        // returned address is opaque (we only test it for non-null and pass it on as a `u64`).
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

/// An ordered list of [`SymbolProvider`]s forming a resolution scope. `resolve(name)` applies the
/// gABI rule: the **first** match wins, **except** a global (strong) definition anywhere in scope
/// overrides a weak one found earlier.
#[derive(Default)]
pub struct Scope {
    providers: Vec<Box<dyn SymbolProvider>>,
}

impl Scope {
    /// An empty scope (resolves nothing).
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Append a provider to the end of the scope (later providers are searched after earlier ones).
    pub fn push(&mut self, provider: Box<dyn SymbolProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    /// Consume the scope into its ordered provider list (so callers can prepend a
    /// [`LoadedObjectProvider`] of the relocated object itself, then chain these env providers — the
    /// gABI scope `[own-object, env...]`). 2026-06-05: used by the bionic-env first cut.
    pub fn into_providers(self) -> Vec<Box<dyn SymbolProvider>> {
        self.providers
    }

    /// Resolve `name` across the scope. Returns the winning definition, or `None` if no provider
    /// defines it. A weak definition is provisional: scanning continues so a later global overrides
    /// it; a global match short-circuits.
    pub fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        let mut weak_hit: Option<ResolvedSym> = None;
        for p in &self.providers {
            if let Some(found) = p.resolve(name) {
                if !found.weak {
                    return Some(found); // strong definition wins immediately (first-wins among strongs)
                }
                // Remember the first weak hit; keep scanning for a strong override.
                weak_hit.get_or_insert(found);
            }
        }
        weak_hit
    }
}

/// A [`reloc::SymbolResolver`] over a [`Scope`] and the *relocated object's own* dynamic symbol
/// table. It maps a relocation's `sym_index` → that object's [`DynSym`] (name + binding), resolves
/// the **name** through the scope, and applies the gABI weak/strong/undef rules:
/// - scope hit → `Some(addr)`,
/// - no hit, referenced symbol is `STB_WEAK` (or the dynsym entry is itself a weak undef) →
///   `Some(0)` (weak-undef = 0, *not* an error),
/// - no hit, referenced symbol is strong → `None` (→ [`reloc::RelocError::UnresolvedSymbol`]).
///
/// TLS resolution ([`reloc::SymbolResolver::resolve_tls_offset`]) is **not** handled here:
/// `R_X86_64_TPOFF64` needs the static-TLS block + `%fs`/TCB (a separate deferred step), so this
/// resolver returns `None` for it — a TPOFF64 reaching it surfaces as a typed error rather than a
/// wrong offset. Callers (see [`super::map`]) partition TPOFF64 out and do not apply it yet.
pub struct ScopedResolver<'a> {
    scope: &'a Scope,
    dynsyms: &'a [DynSym],
}

impl<'a> ScopedResolver<'a> {
    /// Build a resolver over `scope` for an object whose relocations index into `dynsyms`.
    pub fn new(scope: &'a Scope, dynsyms: &'a [DynSym]) -> Self {
        Self { scope, dynsyms }
    }

    /// The dynamic symbol a relocation's `sym_index` names, if in range.
    fn sym(&self, sym_index: u32) -> Option<&DynSym> {
        self.dynsyms.get(sym_index as usize)
    }
}

impl SymbolResolver for ScopedResolver<'_> {
    fn resolve_symbol(&self, sym_index: u32) -> Option<u64> {
        // An out-of-range index is malformed input → unresolved (typed error in the applier).
        let sym = self.sym(sym_index)?;
        // A LOCAL reference is not resolved through the global scope. Returning None lets the
        // applier surface it (a symbol-using reloc against a LOCAL is unusual/malformed here).
        if sym.bind == STB_LOCAL {
            return None;
        }
        if let Some(found) = self.scope.resolve(&sym.name) {
            return Some(found.addr);
        }
        // No definition in scope: weak-undef resolves to 0; strong-undef is unresolved.
        if sym.bind == STB_WEAK {
            Some(0)
        } else {
            None
        }
    }

    fn resolve_tls_offset(&self, _sym_index: u32) -> Option<u64> {
        // TPOFF64 / static-TLS is a deferred step (no %fs/TCB yet); never resolve a TLS offset here.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a defined, exported `DynSym` (GLOBAL FUNC by default).
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

    /// Build a weak, defined, exported `DynSym`.
    fn weak_def(name: &str, value: u64) -> DynSym {
        DynSym {
            bind: STB_WEAK,
            ..def(name, value)
        }
    }

    /// Build an undefined (imported) `DynSym` with the given binding.
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

    /// Build a LOCAL `DynSym` (defined but file-private).
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
        // Defined export → base + value.
        assert_eq!(
            p.resolve("exported"),
            Some(ResolvedSym {
                addr: 0x1100,
                weak: false
            })
        );
        // LOCAL definition is file-private → not exported.
        assert_eq!(p.resolve("private"), None);
        // UNDEF import is not a definition → not offered.
        assert_eq!(p.resolve("imported"), None);
        // Only the one exported symbol is indexed.
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
        assert_eq!(p.resolve("absval"), None); // ABS section skipped
        assert_eq!(
            p.resolve("w"),
            Some(ResolvedSym {
                addr: 0x1020,
                weak: true
            })
        );
    }

    /// A test provider returning a fixed table, to drive Scope ordering tests deterministically.
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
        // First provider's strong definition wins (first-wins among strongs).
        assert_eq!(scope.resolve("f"), Some(strong(0xA)));
    }

    #[test]
    fn scope_global_beats_earlier_weak() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", weak(0xA))]))) // earlier weak
            .push(Box::new(Fixed(vec![("f", strong(0xB))]))); // later global
                                                              // The later global overrides the earlier weak (gABI: a strong anywhere beats a weak).
        assert_eq!(scope.resolve("f"), Some(strong(0xB)));
    }

    #[test]
    fn scope_only_weak_returns_first_weak() {
        let mut scope = Scope::new();
        scope
            .push(Box::new(Fixed(vec![("f", weak(0xA))])))
            .push(Box::new(Fixed(vec![("f", weak(0xB))])));
        // No global in scope → the first weak hit is the result.
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
        // The relocated object references its own export by name (the self-reference pattern).
        let dynsyms = vec![def("self", 0x300)];
        let mut scope = Scope::new();
        scope.push(Box::new(LoadedObjectProvider::new(0x4000, &dynsyms)));
        let r = ScopedResolver::new(&scope, &dynsyms);
        // sym_index 0 names "self" → 0x4000 + 0x300.
        assert_eq!(r.resolve_symbol(0), Some(0x4300));
    }

    #[test]
    fn resolver_weak_undef_resolves_to_zero() {
        // dynsym[0] is a WEAK UNDEF with no definition in scope → resolves to 0 (not an error).
        let dynsyms = vec![undef("weakimp", STB_WEAK)];
        let scope = Scope::new(); // empty: nothing defines it
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), Some(0));
    }

    #[test]
    fn resolver_strong_undef_is_unresolved() {
        // dynsym[0] is a GLOBAL UNDEF with no definition in scope → None (→ typed error in reloc).
        let dynsyms = vec![undef("strongimp", STB_GLOBAL)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), None);
    }

    #[test]
    fn resolver_local_reference_is_not_globally_resolved() {
        let dynsyms = vec![local_def("loc", 0x10)];
        let mut scope = Scope::new();
        // Even if some provider had the name, a LOCAL reference is not resolved through global scope.
        scope.push(Box::new(Fixed(vec![("loc", strong(0x99))])));
        let r = ScopedResolver::new(&scope, &dynsyms);
        assert_eq!(r.resolve_symbol(0), None);
    }

    #[test]
    fn resolver_out_of_range_index_is_unresolved() {
        let dynsyms = vec![def("only", 0x10)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        // Index 5 is past the 1-entry table → None (malformed input, surfaced as a typed error).
        assert_eq!(r.resolve_symbol(5), None);
    }

    #[test]
    fn resolver_never_resolves_tls_offset() {
        let dynsyms = vec![def("x", 0x10)];
        let scope = Scope::new();
        let r = ScopedResolver::new(&scope, &dynsyms);
        // TPOFF64/static-TLS is deferred → always None here.
        assert_eq!(r.resolve_tls_offset(0), None);
    }

    // ---- HostDlsymProvider sanity (uses the real host process's symbols) -----------------------

    #[test]
    fn host_dlsym_resolves_known_libc_symbol() {
        let p = HostDlsymProvider;
        // `memcpy` is in libc, which is linked into this test binary → dlsym(RTLD_DEFAULT) finds it.
        let got = p.resolve("memcpy");
        assert!(got.is_some(), "dlsym must resolve a known libc symbol");
        let got = got.unwrap();
        assert!(got.addr != 0, "resolved address must be non-null");
        assert!(!got.weak, "host dlsym definitions are treated as strong");
        // `malloc` too — a second known symbol.
        assert!(p.resolve("malloc").is_some_and(|s| s.addr != 0));
    }

    #[test]
    fn host_dlsym_returns_none_for_gibberish() {
        let p = HostDlsymProvider;
        assert_eq!(
            p.resolve("__eclipse_definitely_no_such_symbol_4f2a9c__"),
            None
        );
        // A name with an interior NUL is not a valid C symbol → None (no panic).
        assert_eq!(p.resolve("bad\0name"), None);
    }
}
