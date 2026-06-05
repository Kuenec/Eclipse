//! Pure-Rust dependency-graph object loader — the orchestrator that ties the loader cores
//! ([`elf`](super::elf) decode, [`map`](super::map) map + relocate, [`resolve`](super::resolve)
//! symbol scope, [`tls`](super::tls) static-TLS layout) into the actual dynamic linker.
//!
//! 2026-06-05: The four cores load + relocate **one** object (proven fully on `libm.so.6` modulo
//! ifunc). This module is the next layer: given a root `.so`, it transitively loads its
//! `DT_NEEDED` dependency graph, builds the combined cross-object symbol scope + a multi-module
//! static-TLS layout, and relocates every loaded object against that global scope — i.e. it links
//! a whole dependency graph the way a System V dynamic linker does.
//!
//! ## Clean-room provenance
//! Every rule below is from the **public** System V gABI dynamic-linking model and Eclipse's own
//! loader cores: `DT_NEEDED` transitive load, soname-deduped objects, a global (breadth-first)
//! symbol scope with ELF first-wins, dependency-order relocation, and the variant-II static-TLS
//! stacking the [`tls`](super::tls) core already encodes. No dynamic-linker source was read.
//!
//! ## What it does
//! [`Linker::load`]:
//! 1. **Transitive load (BFS, soname-deduped, cycle-safe).** Read + [`elf::ElfImage::parse`] +
//!    [`map::MappedObject::map_and_relocate`] (reserve + place `PT_LOAD`, apply the base-only
//!    `RELATIVE`/`DT_RELR`) the root, then breadth-first each object's `DT_NEEDED` sonames,
//!    locating each across the search paths. Each soname is loaded **once** (a diamond
//!    `A → B,C → D` loads `D` once); a cycle does not re-enter (in-progress objects are already
//!    recorded). The load list is the deterministic BFS order from the root.
//! 2. **Combined scope.** Build the global symbol [`resolve::Scope`] = a
//!    [`resolve::LoadedObjectProvider`] for **every** loaded object, in BFS order (ELF first-wins =
//!    breadth order from the root), optionally a [`resolve::HostDlsymProvider`] **last** (opt-in via
//!    [`Linker::with_host_fallback`]; the eventual bionic load wants this **off** so glibc symbols
//!    do not satisfy bionic imports). Build a multi-module [`tls::TlsLayout`] by `add_module`-ing
//!    every loaded object that has a `PT_TLS`.
//! 3. **Relocate every object (deps-first).** For each object (reverse BFS — deps before
//!    dependents; sound because the scope is global), apply the symbol relocations
//!    (`GLOB_DAT`/`JUMP_SLOT`/`R_X86_64_64`) through the scope and the static-TLS relocations
//!    (`TPOFF64`) through the layout. `IRELATIVE` is **counted as deferred** (the ifunc tail —
//!    needs executing the resolver functions; nothing is executed here). An **unresolved strong**
//!    symbol is recorded (per object + index, all of them enumerated) and **never fabricated** —
//!    it does not corrupt the image (the apply pass is skipped for an object that has any, so no
//!    partial/inconsistent GOT is written).
//!
//! ## What it deliberately does NOT do (the runtime integration tail)
//! It maps + relocates the graph; it does **not** bind the assembled TLS block to a live thread
//! pointer (`%fs`/TCB), execute `IRELATIVE` ifunc resolvers, or run `DT_INIT`/`DT_INIT_ARRAY`. It
//! never jumps into or executes any loaded code — those are the documented follow-on integration
//! steps (AGENTS.md §5).
//!
//! ## Safety
//! `#![forbid(unsafe_code)]`: this module only orchestrates. All `unsafe` stays confined to
//! [`map`](super::map) (the `mmap`/`mprotect`/`munmap` syscalls) and [`resolve`](super::resolve)
//! (the one `dlsym` FFI), exactly as before.
//!
//! ## RAII
//! [`LoadedImageSet`] owns every object's [`map::MappedObject`]; dropping the set `munmap`s all of
//! them (no leak), via each `MappedObject`'s own `Drop`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::elf::{ElfError, ElfImage};
use super::map::{
    host_page_size, MapError, MapStats, MappedObject, SymbolRelocStats, TlsRelocStats,
};
use super::reloc::{self, Rela, SymbolResolver};
use super::resolve::{HostDlsymProvider, LoadedObjectProvider, Scope, ScopedResolver};
use super::tls::TlsLayout;

/// x86-64 `R_X86_64_IRELATIVE` (type 37): an ifunc relocation resolved by **executing** the
/// library's resolver — out of scope for this loader (nothing is executed). Counted as deferred.
const R_X86_64_IRELATIVE: u32 = 37;

/// Typed errors from linking a dependency graph. Each carries the offending object's soname (or the
/// requested name, when the file could not even be located) so a failure is actionable.
#[derive(Debug)]
pub enum LinkError {
    /// A `DT_NEEDED` dependency's file could not be found on any search path. Carries the requested
    /// soname and the dependent object that needed it.
    MissingDependency {
        /// The `DT_NEEDED` soname that could not be located.
        soname: String,
        /// The soname of the object that declared the dependency.
        needed_by: String,
    },
    /// Reading an object's file bytes failed. Carries the path and the I/O error string.
    Io {
        /// The file path that failed to read.
        path: PathBuf,
        /// The underlying I/O error, rendered.
        error: String,
    },
    /// Decoding an object failed. Carries the object's identifier (soname or path) and the error.
    Parse {
        /// The object's soname or path, for context.
        object: String,
        /// The decode error.
        error: ElfError,
    },
    /// Mapping / base-relocating an object failed. Carries the object's soname and the error.
    Map {
        /// The object's soname, for context.
        object: String,
        /// The map/relocate error.
        error: MapError,
    },
    /// Reading an object's `DT_NEEDED` / `DT_SONAME` strings failed. Carries the object's
    /// identifier and the decode error.
    DynStrings {
        /// The object's soname or path, for context.
        object: String,
        /// The string-table decode error.
        error: ElfError,
    },
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDependency { soname, needed_by } => write!(
                f,
                "dependency {soname:?} (needed by {needed_by:?}) not found on any search path"
            ),
            Self::Io { path, error } => write!(f, "reading {}: {error}", path.display()),
            Self::Parse { object, error } => write!(f, "decoding {object}: {error}"),
            Self::Map { object, error } => write!(f, "mapping {object}: {error}"),
            Self::DynStrings { object, error } => {
                write!(f, "reading dynamic strings of {object}: {error}")
            }
        }
    }
}

impl std::error::Error for LinkError {}

/// One unresolved **strong** (non-weak) symbol surfaced while relocating an object: the gABI says a
/// strong undefined reference with no definition in scope is an error (never fabricated). Recorded,
/// not aborted on, so the whole graph's gaps are reported together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedSymbol {
    /// The soname of the object whose relocation referenced the symbol.
    pub object: String,
    /// The unresolved symbol's name (from the object's dynamic symtab).
    pub name: String,
    /// The relocation's dynamic-symtab index in that object.
    pub sym_index: u32,
}

/// Aggregate relocation counts across the whole linked graph, for verification/reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelocStats {
    /// Total `R_X86_64_RELATIVE` relocations applied (base pass, all objects).
    pub relative_applied: usize,
    /// Total `DT_RELR`-encoded relative relocations applied (base pass, all objects).
    pub relr_applied: usize,
    /// Total `R_X86_64_GLOB_DAT` relocations applied (symbol pass, all objects).
    pub glob_dat_applied: usize,
    /// Total `R_X86_64_JUMP_SLOT` relocations applied (symbol pass, all objects).
    pub jump_slot_applied: usize,
    /// Total `R_X86_64_64` relocations applied (symbol pass, all objects).
    pub abs64_applied: usize,
    /// Total `R_X86_64_TPOFF64` (static-TLS) relocations applied (TLS pass, all objects).
    pub tpoff64_applied: usize,
    /// Total `R_X86_64_IRELATIVE` (ifunc) relocations **deferred** across all objects — the
    /// documented ifunc tail (needs executing resolvers), not a failure.
    pub irelative_deferred: usize,
}

impl RelocStats {
    /// Fold one object's per-pass stats into the aggregate.
    fn accumulate(&mut self, map: MapStats, sym: SymbolRelocStats, tls: TlsRelocStats) {
        self.relative_applied += map.relative_applied;
        self.relr_applied += map.relr_applied;
        self.glob_dat_applied += sym.glob_dat_applied;
        self.jump_slot_applied += sym.jump_slot_applied;
        self.abs64_applied += sym.abs64_applied;
        self.tpoff64_applied += tls.tpoff64_applied;
    }
}

/// One loaded object in the graph: its soname, the run-time mapping, and the per-object reloc
/// counts. The [`MappedObject`] owns the mapping (RAII-`munmap`ped when the [`LoadedImageSet`] that
/// holds it drops).
pub struct LoadedObject {
    /// The object's `DT_SONAME` (or, lacking one, the file name it was loaded from). Dedup key.
    pub soname: String,
    /// The absolute path the object was loaded from.
    pub path: PathBuf,
    /// The raw file bytes (kept so the parsed [`ElfImage`] can be re-derived for callers).
    pub bytes: Vec<u8>,
    /// The mapped, base-relocated object (owns its `mmap`; `munmap`s on drop).
    pub mapped: MappedObject,
    /// The base pass counts (segments, RELATIVE, RELR).
    pub map_stats: MapStats,
    /// The symbol pass counts (GLOB_DAT/JUMP_SLOT/64 applied; `default` if skipped due to an
    /// unresolved-strong symbol in this object).
    pub sym_stats: SymbolRelocStats,
    /// The static-TLS pass counts (TPOFF64 applied / IRELATIVE deferred).
    pub tls_stats: TlsRelocStats,
}

impl LoadedObject {
    /// Re-parse this object's [`ElfImage`] from its kept bytes. The parse is a pure, bounds-checked
    /// data decode (the same one [`Linker::load`] performed); it maps/executes nothing.
    pub fn image(&self) -> Result<ElfImage<'_>, ElfError> {
        ElfImage::parse(&self.bytes)
    }

    /// The run-time load base of this object's mapping.
    pub fn load_base(&self) -> u64 {
        self.mapped.load_base()
    }
}

/// The result of linking a dependency graph: every loaded object (BFS order from the root), the
/// combined symbol scope, the multi-module static-TLS layout, the aggregate reloc counts, and any
/// recorded unresolved-strong symbols. Dropping it `munmap`s every object's mapping (no leak).
pub struct LoadedImageSet {
    /// Loaded objects in deterministic BFS order from the root (index 0 is the root).
    pub objects: Vec<LoadedObject>,
    /// The combined global symbol scope (a `LoadedObjectProvider` per object, optional host
    /// fallback last). Built once; used to relocate every object.
    pub scope: Scope,
    /// The multi-module variant-II static-TLS layout (one module per loaded object that has
    /// `PT_TLS`).
    pub tls_layout: TlsLayout,
    /// Aggregate relocation counts across the graph.
    pub stats: RelocStats,
    /// Unresolved **strong** symbols recorded across the graph (empty = every strong reference
    /// resolved). Never fabricated; an object with any has its symbol pass skipped (no partial GOT).
    pub unresolved: Vec<UnresolvedSymbol>,
}

impl LoadedImageSet {
    /// Find a loaded object by soname.
    pub fn object(&self, soname: &str) -> Option<&LoadedObject> {
        self.objects.iter().find(|o| o.soname == soname)
    }
}

/// A dependency-graph linker over a list of search paths.
///
/// Build with [`Linker::new`] (the search-path list) and optionally [`Linker::with_host_fallback`]
/// (append a [`HostDlsymProvider`] to the scope as a last-resort tier — **off** by default, since
/// the eventual bionic load must not satisfy bionic imports from host glibc). Then [`Linker::load`]
/// a root `.so`.
pub struct Linker {
    search_paths: Vec<PathBuf>,
    host_fallback: bool,
}

impl Linker {
    /// A linker that locates `DT_NEEDED` dependencies across `search_paths` (searched in order).
    /// Host-symbol fallback is **off** by default.
    pub fn new<I, P>(search_paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self {
            search_paths: search_paths.into_iter().map(Into::into).collect(),
            host_fallback: false,
        }
    }

    /// Enable (or disable) the host-symbol fallback tier: when on, a [`HostDlsymProvider`] is
    /// appended **last** to the scope so a symbol no loaded object defines is resolved from the host
    /// process (`dlsym(RTLD_DEFAULT)`). Off for the bionic load (host glibc must not leak in).
    pub fn with_host_fallback(mut self, enabled: bool) -> Self {
        self.host_fallback = enabled;
        self
    }

    /// Load + relocate the dependency graph rooted at `root_path`.
    ///
    /// Transitively loads the `DT_NEEDED` graph (BFS, soname-deduped, cycle-safe), builds the
    /// combined symbol scope + multi-module static-TLS layout, and relocates every object
    /// deps-first. Returns a [`LoadedImageSet`] (objects + scope + layout + aggregate stats +
    /// recorded unresolved-strong symbols). Does **not** execute ifunc/init or bind `%fs`.
    pub fn load(&self, root_path: impl AsRef<Path>) -> Result<LoadedImageSet, LinkError> {
        let page = host_page_size();
        let root_path = root_path.as_ref().to_path_buf();

        // ---- 1) Transitive load (BFS, soname-deduped, cycle-safe) -------------------------------
        // `loaded` maps soname → index into `objects`; an entry exists as soon as the object is
        // mapped, so a cycle / diamond re-reference finds it and does not re-enter. `queue` drives
        // the breadth-first walk; the load order (objects' index order) is the BFS order.
        let mut objects: Vec<LoadedObject> = Vec::new();
        let mut loaded: HashMap<String, usize> = HashMap::new();
        // BFS frontier: each entry is (path-to-load, requested-soname-or-None-for-root,
        // needed-by-soname-for-error-context).
        let mut queue: Vec<PendingLoad> = vec![PendingLoad {
            path: root_path.clone(),
            requested: None,
        }];
        let mut head = 0usize;

        while head < queue.len() {
            let pending = queue[head].clone();
            head += 1;

            // Dedup by requested soname *before* loading (so a diamond/cycle loads each once).
            if let Some(req) = &pending.requested {
                if loaded.contains_key(req) {
                    continue;
                }
            }

            let (object, needed) = self.load_one(&pending, page)?;
            let soname = object.soname.clone();
            // Dedup by the resolved soname too (the root, or a dep whose soname differs from the
            // requested name): if already loaded, drop this duplicate mapping.
            if loaded.contains_key(&soname) {
                continue;
            }
            let idx = objects.len();
            objects.push(object);
            loaded.insert(soname.clone(), idx);
            if let Some(req) = &pending.requested {
                // Also index under the requested name so a sibling DT_NEEDED dedups even if it
                // names the dependency by a soname-equal string.
                loaded.entry(req.clone()).or_insert(idx);
            }

            // Enqueue this object's DT_NEEDED dependencies (located lazily when dequeued).
            for dep in &needed {
                if loaded.contains_key(dep) {
                    continue; // already loaded / already queued-and-loaded
                }
                let Some(dep_path) = self.locate(dep) else {
                    return Err(LinkError::MissingDependency {
                        soname: dep.clone(),
                        needed_by: soname.clone(),
                    });
                };
                queue.push(PendingLoad {
                    path: dep_path,
                    requested: Some(dep.clone()),
                });
            }
        }

        // ---- 2) Combined scope + multi-module static-TLS layout ---------------------------------
        // Scope: a LoadedObjectProvider per object, in BFS order (ELF first-wins = breadth order
        // from the root). Optional HostDlsymProvider appended LAST.
        let mut scope = Scope::new();
        for obj in &objects {
            let img = obj.image().map_err(|error| LinkError::Parse {
                object: obj.soname.clone(),
                error,
            })?;
            scope.push(Box::new(LoadedObjectProvider::new(
                obj.load_base(),
                &img.dynsyms,
            )));
        }
        if self.host_fallback {
            scope.push(Box::new(HostDlsymProvider));
        }

        // TLS layout: add every object that has a PT_TLS, in BFS order, so a cross-module TPOFF64
        // (e.g. libm's errno import) resolves to the defining module's block. Record each object's
        // OWN module tp-relative base (keyed by index) for its self-referential (sym-0) TPOFF64.
        let mut tls_layout = TlsLayout::new();
        let mut own_tp_offset: HashMap<usize, i64> = HashMap::new();
        for (idx, obj) in objects.iter().enumerate() {
            let img = obj.image().map_err(|error| LinkError::Parse {
                object: obj.soname.clone(),
                error,
            })?;
            if let Some(tls) = img.tls {
                let tdata_off = img
                    .vaddr_to_off(tls.vaddr)
                    .map_err(|error| LinkError::Parse {
                        object: obj.soname.clone(),
                        error,
                    })?;
                let module =
                    MappedObjectTls::add(&mut tls_layout, &tls, &obj.bytes, tdata_off as u64, &img)
                        .map_err(|error| LinkError::Map {
                            object: obj.soname.clone(),
                            error,
                        })?;
                own_tp_offset.insert(idx, module.tp_offset);
            }
        }

        // ---- 3) Relocate every object deps-first (the scope is global, so order is for clarity) --
        let mut stats = RelocStats::default();
        let mut unresolved: Vec<UnresolvedSymbol> = Vec::new();
        // Reverse BFS = deps before dependents.
        for idx in (0..objects.len()).rev() {
            // Destructure the object so the immutable borrow of `bytes` (held by the parsed image)
            // and the mutable borrow of `mapped` are disjoint field borrows, not two borrows of the
            // whole `objects[idx]`.
            let obj = &mut objects[idx];
            let soname = obj.soname.clone();
            let map_stats = obj.map_stats;
            let LoadedObject {
                bytes,
                mapped,
                sym_stats: obj_sym_stats,
                tls_stats: obj_tls_stats,
                ..
            } = obj;

            let img = ElfImage::parse(bytes).map_err(|error| LinkError::Parse {
                object: soname.clone(),
                error,
            })?;

            // Record this object's IRELATIVE (ifunc) count and any unresolved-strong symbols
            // BEFORE applying, so the report is complete and no partial GOT is ever written.
            let relas = img.relocations().map_err(|error| LinkError::DynStrings {
                object: soname.clone(),
                error,
            })?;
            stats.irelative_deferred += relas
                .iter()
                .filter(|r| r.r_type == R_X86_64_IRELATIVE)
                .count();

            let object_unresolved = enumerate_unresolved_strong(&relas, &img, &scope, &soname);

            let (sym_stats, tls_stats) = if object_unresolved.is_empty() {
                // Apply the symbol relocations (GLOB_DAT/JUMP_SLOT/64) through the global scope.
                let sym_stats = mapped
                    .relocate_symbols(&img, &scope, page)
                    .map_err(|error| LinkError::Map {
                        object: soname.clone(),
                        error,
                    })?;
                // Apply the static-TLS relocations (TPOFF64) through the layout. The inner resolver
                // is the same global scope (for any non-TLS lookup the TLS pass might delegate).
                // `own_tp_offset[idx]` resolves a self-referential (sym-0) TPOFF64 to this object's
                // own TLS block (e.g. libc's 15 sym-0 entries).
                let inner = ScopedResolver::new(&scope, &img.dynsyms);
                let tls_stats = mapped
                    .relocate_tls(
                        &img,
                        &inner,
                        &tls_layout,
                        own_tp_offset.get(&idx).copied(),
                        page,
                    )
                    .map_err(|error| LinkError::Map {
                        object: soname.clone(),
                        error,
                    })?;
                (sym_stats, tls_stats)
            } else {
                // Skip the apply for an object with unresolved-strong references: applying would
                // abort mid-pass on the first one and leave an inconsistent GOT. Record them all.
                unresolved.extend(object_unresolved);
                (SymbolRelocStats::default(), TlsRelocStats::default())
            };

            stats.accumulate(map_stats, sym_stats, tls_stats);
            *obj_sym_stats = sym_stats;
            *obj_tls_stats = tls_stats;
        }

        Ok(LoadedImageSet {
            objects,
            scope,
            tls_layout,
            stats,
            unresolved,
        })
    }

    /// Read, parse, and map+base-relocate one object, returning it plus its `DT_NEEDED` sonames.
    fn load_one(
        &self,
        pending: &PendingLoad,
        page: u64,
    ) -> Result<(LoadedObject, Vec<String>), LinkError> {
        let bytes = std::fs::read(&pending.path).map_err(|e| LinkError::Io {
            path: pending.path.clone(),
            error: e.to_string(),
        })?;
        // Identify the object by its requested soname (deps) or file name (root) for error context
        // before the soname is decoded.
        let ident = pending.requested.clone().unwrap_or_else(|| {
            pending
                .path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| pending.path.display().to_string())
        });

        let img = ElfImage::parse(&bytes).map_err(|error| LinkError::Parse {
            object: ident.clone(),
            error,
        })?;

        // The dedup key: DT_SONAME if present, else the requested name, else the file name.
        let soname = img
            .soname()
            .map_err(|error| LinkError::DynStrings {
                object: ident.clone(),
                error,
            })?
            .or_else(|| pending.requested.clone())
            .unwrap_or_else(|| ident.clone());

        let needed = img.needed().map_err(|error| LinkError::DynStrings {
            object: soname.clone(),
            error,
        })?;

        let (mapped, map_stats) =
            MappedObject::map_and_relocate(&img, &bytes, page).map_err(|error| LinkError::Map {
                object: soname.clone(),
                error,
            })?;

        // Drop the borrow of `bytes` (held by `img`) before moving `bytes` into the object.
        drop(img);

        Ok((
            LoadedObject {
                soname,
                path: pending.path.clone(),
                bytes,
                mapped,
                map_stats,
                sym_stats: SymbolRelocStats::default(),
                tls_stats: TlsRelocStats::default(),
            },
            needed,
        ))
    }

    /// Locate a `DT_NEEDED` soname across the search paths. An absolute or origin-relative soname
    /// (containing a path separator) is honored directly if it exists; otherwise each search path
    /// is tried in order. Returns the first existing path.
    fn locate(&self, soname: &str) -> Option<PathBuf> {
        // Honor an absolute / path-bearing name directly (a DT_NEEDED may carry a path).
        if soname.contains('/') {
            let p = PathBuf::from(soname);
            if p.exists() {
                return Some(p);
            }
        }
        for dir in &self.search_paths {
            let candidate = dir.join(soname);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }
}

/// A queued object to load: its file path and the soname that requested it (None for the root).
#[derive(Clone)]
struct PendingLoad {
    path: PathBuf,
    requested: Option<String>,
}

/// Enumerate **every** unresolved strong (non-weak) symbol among `relas`: a `GLOB_DAT`/`JUMP_SLOT`/
/// `R_X86_64_64` whose referenced dynsym is a strong (`!WEAK`) symbol the scope does not define.
/// Weak-undef resolves to 0 (not unresolved); a defined symbol resolves. This is the gABI rule the
/// [`ScopedResolver`] enforces, enumerated here so all gaps are reported (not just the first).
fn enumerate_unresolved_strong(
    relas: &[Rela],
    img: &ElfImage<'_>,
    scope: &Scope,
    object_soname: &str,
) -> Vec<UnresolvedSymbol> {
    let resolver = ScopedResolver::new(scope, &img.dynsyms);
    let mut out = Vec::new();
    for r in relas {
        let is_symbol_reloc = matches!(
            r.r_type,
            reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
        );
        if !is_symbol_reloc {
            continue;
        }
        // `resolve_symbol` returns None only for an unresolved STRONG reference (weak-undef → 0,
        // defined → addr). That None is exactly the gABI's typed unresolved-strong case.
        if resolver.resolve_symbol(r.sym_index).is_none() {
            let name = img
                .dynsyms
                .get(r.sym_index as usize)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            out.push(UnresolvedSymbol {
                object: object_soname.to_string(),
                name,
                sym_index: r.sym_index,
            });
        }
    }
    out
}

/// Thin adapter that adds a module's `PT_TLS` to a [`TlsLayout`] — isolates the [`TlsLayout`]'s
/// `add_module` call so its `TlsError` maps cleanly into a [`MapError::Reloc`]-style context. (The
/// layout's error type is the `tls` core's `TlsError`; here it is folded into the link error via the
/// caller's `LinkError::Map` mapping by routing through a `MapError`.)
struct MappedObjectTls;

impl MappedObjectTls {
    fn add(
        layout: &mut TlsLayout,
        tls: &super::elf::TlsSegment,
        file: &[u8],
        tdata_off: u64,
        img: &ElfImage<'_>,
    ) -> Result<super::tls::TlsModule, MapError> {
        layout
            .add_module(tls, file, tdata_off, &img.dynsyms)
            .map_err(|e| MapError::SpanOverflow(tls_err_static(&e)))
    }
}

/// Render a `TlsError` to a short static description (the `MapError::SpanOverflow` payload is
/// `&'static str`). Keeps the TLS-layout failure category visible in the link error message.
fn tls_err_static(e: &super::tls::TlsError) -> &'static str {
    use super::tls::TlsError;
    match e {
        TlsError::BadAlign(_) => "PT_TLS p_align not a power of two",
        TlsError::FileLargerThanMem(_, _) => "PT_TLS filesz exceeds memsz",
        TlsError::Overflow(_) => "static-TLS layout arithmetic overflow",
        TlsError::TdataOutOfFile(_) => "PT_TLS .tdata past file bytes",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::elf::{PF_R, PF_W, PF_X};
    use crate::loader::reloc::R_X86_64_GLOB_DAT;
    use std::io::Write;

    // ---- Minimal in-memory ELF fixture builder (a small, valid x86-64 ET_DYN) ------------------
    //
    // Builds a one-PT_LOAD .so with a configurable DT_SONAME, DT_NEEDED list, and one exported
    // symbol + (optionally) one GLOB_DAT import resolved cross-object. Page size 0x1000; the single
    // PT_LOAD maps the file 1:1 (vaddr == file offset), so vaddr_to_off is the identity.

    const PAGE: u64 = 0x1000;
    const PH_OFF: usize = 0x40;
    const DYN_OFF: u64 = 0x200;
    const RELA_OFF: u64 = 0x400;
    const SYM_OFF: u64 = 0x600;
    const STR_OFF: u64 = 0x800;
    const GLOB_TARGET: u64 = 0xc00; // a GOT slot in the (writable) image
    const IMG_SIZE: usize = 0x2000;

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
    const SYM_SIZE: usize = 24;
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_NEEDED: i64 = 1;
    const DT_RELA: i64 = 7;
    const DT_RELASZ: i64 = 8;
    const DT_RELAENT: i64 = 9;
    const DT_STRTAB: i64 = 5;
    const DT_STRSZ: i64 = 10;
    const DT_SYMTAB: i64 = 6;
    const DT_SYMENT: i64 = 11;
    const DT_SONAME: i64 = 14;
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

    /// Build a fixture .so. `soname` is its DT_SONAME; `needed` its DT_NEEDED list; if `export` is
    /// Some, it defines that name (an exported GLOBAL FUNC at value 0x1500); if `import` is Some, it
    /// has a GLOB_DAT against that undefined name (resolved cross-object). All names share one
    /// string table built here.
    fn build_so(
        soname: &str,
        needed: &[&str],
        export: Option<&str>,
        import: Option<&str>,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; IMG_SIZE];

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
        put_u16(&mut buf, 56, 2); // LOAD, DYNAMIC

        // One RW PT_LOAD over the whole image (so the GOT slot is writable) + PT_DYNAMIC.
        put_phdr(
            &mut buf,
            0,
            PT_LOAD,
            PF_R | PF_W | PF_X,
            0,
            0,
            IMG_SIZE as u64,
            IMG_SIZE as u64,
            PAGE,
        );
        put_phdr(
            &mut buf,
            1,
            PT_DYNAMIC,
            PF_R | PF_W,
            DYN_OFF,
            DYN_OFF,
            0x100,
            0x100,
            8,
        );

        // Build the string table: \0 then each unique name NUL-terminated; record offsets.
        let mut strtab = vec![0u8]; // leading NUL
        let name_off = |strtab: &mut Vec<u8>, s: &str| -> u64 {
            let off = strtab.len() as u64;
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
            off
        };
        let soname_off = name_off(&mut strtab, soname);
        let needed_offs: Vec<u64> = needed.iter().map(|n| name_off(&mut strtab, n)).collect();
        let export_off = export.map(|e| name_off(&mut strtab, e));
        let import_off = import.map(|i| name_off(&mut strtab, i));
        let strsz = strtab.len() as u64;
        buf[STR_OFF as usize..STR_OFF as usize + strtab.len()].copy_from_slice(&strtab);

        // .dynsym: sym[0] = null; sym[1] = export (if any); sym[2] = import (if any). One slot each;
        // the import is the last so its index is deterministic.
        let mut sym_count = 1u64; // null
        let mut export_index = 0u32;
        let mut import_index = 0u32;
        if let Some(eo) = export_off {
            let s = SYM_OFF as usize + (sym_count as usize) * SYM_SIZE;
            put_u32(&mut buf, s, eo as u32);
            buf[s + 4] = (1 << 4) | 2; // STB_GLOBAL FUNC
            put_u16(&mut buf, s + 6, 1); // defined
            put_u64(&mut buf, s + 8, 0x1500); // st_value
            export_index = sym_count as u32;
            sym_count += 1;
        }
        if let Some(io) = import_off {
            let s = SYM_OFF as usize + (sym_count as usize) * SYM_SIZE;
            put_u32(&mut buf, s, io as u32);
            buf[s + 4] = (1 << 4) | 2; // STB_GLOBAL FUNC (strong import)
            put_u16(&mut buf, s + 6, 0); // SHN_UNDEF
            import_index = sym_count as u32;
            sym_count += 1;
        }
        let _ = export_index;

        // .rela.dyn: one GLOB_DAT against the import (if any).
        let rela_count = if import.is_some() { 1 } else { 0 };
        if import.is_some() {
            put_u64(&mut buf, RELA_OFF as usize, GLOB_TARGET); // r_offset
            let r_info = ((import_index as u64) << 32) | R_X86_64_GLOB_DAT as u64;
            put_u64(&mut buf, RELA_OFF as usize + 8, r_info);
            put_u64(&mut buf, RELA_OFF as usize + 16, 0); // addend
        }

        // .dynamic.
        let mut slot = 0usize;
        let mut d = |buf: &mut [u8], tag: i64, val: u64| {
            let off = DYN_OFF as usize + slot * DYN_SIZE;
            put_u64(buf, off, tag as u64);
            put_u64(buf, off + 8, val);
            slot += 1;
        };
        d(&mut buf, DT_SONAME, soname_off);
        for no in &needed_offs {
            d(&mut buf, DT_NEEDED, *no);
        }
        d(&mut buf, DT_SYMTAB, SYM_OFF);
        d(&mut buf, DT_SYMENT, SYM_SIZE as u64);
        d(&mut buf, DT_STRTAB, STR_OFF);
        d(&mut buf, DT_STRSZ, strsz);
        if rela_count > 0 {
            d(&mut buf, DT_RELA, RELA_OFF);
            d(&mut buf, DT_RELASZ, RELA_ENT * rela_count);
            d(&mut buf, DT_RELAENT, RELA_ENT);
        }
        d(&mut buf, DT_NULL, 0);

        // The symtab must end before DT_STRTAB (elf.rs caps the symtab scan at strtab). SYM_OFF +
        // sym_count*24 <= STR_OFF holds for our small counts.
        assert!(SYM_OFF as usize + (sym_count as usize) * SYM_SIZE <= STR_OFF as usize);
        buf
    }

    /// Write `bytes` to `dir/name` and return the path.
    fn write_so(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create fixture .so");
        f.write_all(bytes).expect("write fixture .so");
        path
    }

    /// A fresh temp dir under the OS temp root, unique per test.
    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "eclipse-link-test-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    #[test]
    fn root_with_one_dep_resolves_cross_object_symbol() {
        // root imports "shared_fn" (GLOB_DAT); dep defines + exports it. The linker must load both,
        // build a global scope, and the root's GOT slot must hold dep_base + 0x1500.
        let dir = temp_dir("crossobj");
        let root = build_so("root.so", &["dep.so"], None, Some("shared_fn"));
        let dep = build_so("dep.so", &[], Some("shared_fn"), None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "dep.so", &dep);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&root_path).expect("link succeeds");

        assert_eq!(set.objects.len(), 2, "root + dep");
        assert_eq!(set.objects[0].soname, "root.so");
        assert_eq!(set.objects[1].soname, "dep.so");
        assert!(set.unresolved.is_empty(), "no unresolved strong symbols");
        assert_eq!(set.stats.glob_dat_applied, 1);

        // The root's GOT slot at GLOB_TARGET now holds the dep's exported symbol address.
        let dep_base = set.object("dep.so").unwrap().load_base();
        let root_obj = &set.objects[0];
        let got = read_word(root_obj, GLOB_TARGET);
        assert_eq!(got, dep_base.wrapping_add(0x1500));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Read an 8-byte word from a loaded object's mapped image at region-relative `vaddr`
    /// (test-only; the fixture's PIE has region_start 0, so vaddr == region offset). Uses the safe
    /// [`MappedObject::read_u64`] accessor — no `unsafe` here (this module is `forbid(unsafe_code)`).
    fn read_word(obj: &LoadedObject, vaddr: u64) -> u64 {
        obj.mapped
            .read_u64(vaddr as usize)
            .expect("GOT slot is within the mapped region")
    }

    #[test]
    fn diamond_dedups_shared_dependency() {
        // A -> B, C ; B -> D ; C -> D. D must be loaded exactly once (soname dedup).
        let dir = temp_dir("diamond");
        let a = build_so("A.so", &["B.so", "C.so"], None, None);
        let b = build_so("B.so", &["D.so"], None, None);
        let c = build_so("C.so", &["D.so"], None, None);
        let d = build_so("D.so", &[], Some("d_sym"), None);
        let a_path = write_so(&dir, "A.so", &a);
        write_so(&dir, "B.so", &b);
        write_so(&dir, "C.so", &c);
        write_so(&dir, "D.so", &d);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&a_path).expect("link succeeds");

        // A, B, C, D — D once despite two referrers.
        assert_eq!(set.objects.len(), 4, "A,B,C,D — D deduped to one");
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        assert_eq!(sonames.iter().filter(|s| **s == "D.so").count(), 1);
        // BFS order from the root: A, then B, C (A's deps), then D (first seen via B).
        assert_eq!(sonames, vec!["A.so", "B.so", "C.so", "D.so"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_dependency_is_typed_error() {
        let dir = temp_dir("missing");
        let root = build_so("root.so", &["nonexistent.so"], None, None);
        let root_path = write_so(&dir, "root.so", &root);

        let linker = Linker::new([dir.clone()]);
        match linker.load(&root_path) {
            Err(LinkError::MissingDependency { soname, needed_by }) => {
                assert_eq!(soname, "nonexistent.so");
                assert_eq!(needed_by, "root.so");
            }
            Err(other) => panic!("expected MissingDependency, got {other:?}"),
            Ok(_) => panic!("expected MissingDependency error, but load succeeded"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependency_order_is_deterministic_bfs() {
        // root -> X, Y ; X -> Z. Expected BFS: root, X, Y, Z (stable across runs).
        let dir = temp_dir("order");
        let root = build_so("root.so", &["X.so", "Y.so"], None, None);
        let x = build_so("X.so", &["Z.so"], None, None);
        let y = build_so("Y.so", &[], None, None);
        let z = build_so("Z.so", &[], None, None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "X.so", &x);
        write_so(&dir, "Y.so", &y);
        write_so(&dir, "Z.so", &z);

        let linker = Linker::new([dir.clone()]);
        for _ in 0..5 {
            let set = linker.load(&root_path).expect("link succeeds");
            let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
            assert_eq!(sonames, vec!["root.so", "X.so", "Y.so", "Z.so"]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cycle_does_not_re_enter() {
        // P -> Q ; Q -> P. A cycle must terminate with each loaded once.
        let dir = temp_dir("cycle");
        let p = build_so("P.so", &["Q.so"], None, None);
        let q = build_so("Q.so", &["P.so"], None, None);
        let p_path = write_so(&dir, "P.so", &p);
        write_so(&dir, "Q.so", &q);

        let linker = Linker::new([dir.clone()]);
        let set = linker.load(&p_path).expect("link terminates on a cycle");
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        assert_eq!(sonames, vec!["P.so", "Q.so"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unresolved_strong_is_recorded_not_fabricated() {
        // root imports a strong "missing_sym" that no loaded object defines and host fallback is
        // off → it must be RECORDED (not an Err, not a fabricated address, no GOT write).
        let dir = temp_dir("unresolved");
        let root = build_so("root.so", &[], None, Some("missing_sym"));
        let root_path = write_so(&dir, "root.so", &root);

        let linker = Linker::new([dir.clone()]); // host fallback OFF by default
        let set = linker
            .load(&root_path)
            .expect("load still succeeds; gap recorded");
        assert_eq!(set.unresolved.len(), 1);
        assert_eq!(set.unresolved[0].object, "root.so");
        assert_eq!(set.unresolved[0].name, "missing_sym");
        // The symbol pass was skipped → no GLOB_DAT applied, GOT slot left at its pre-reloc 0.
        assert_eq!(set.stats.glob_dat_applied, 0);
        assert_eq!(read_word(&set.objects[0], GLOB_TARGET), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drop_unmaps_whole_graph_without_leak() {
        // Repeatedly link a small graph; if Drop didn't munmap every object, address space would
        // exhaust. Exercises LoadedImageSet → LoadedObject → MappedObject Drop chaining.
        let dir = temp_dir("dropleak");
        let root = build_so("root.so", &["dep.so"], None, Some("shared_fn"));
        let dep = build_so("dep.so", &[], Some("shared_fn"), None);
        let root_path = write_so(&dir, "root.so", &root);
        write_so(&dir, "dep.so", &dep);

        let linker = Linker::new([dir.clone()]);
        for _ in 0..128 {
            let set = linker.load(&root_path).expect("link");
            assert_eq!(set.objects.len(), 2);
            drop(set);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- REAL test: link /usr/lib/libm.so.6 + its DT_NEEDED graph (skips cleanly if absent) -----

    /// Standard host lib dirs to search for the real graph. Detect-don't-assume: we only use what
    /// exists; the test skips entirely if libm is absent.
    const HOST_LIB_DIRS: &[&str] = &[
        "/usr/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/lib64",
        "/usr/lib64",
    ];

    fn find_libm() -> Option<PathBuf> {
        for d in HOST_LIB_DIRS {
            let p = Path::new(d).join("libm.so.6");
            if p.exists() {
                return Some(p);
            }
        }
        None
    }

    #[test]
    fn real_libm_graph_links_and_relocates() {
        // 2026-06-05: the orchestrator on a REAL multi-object graph. libm -> libc, ld-linux;
        // libc -> ld-linux (diamond on ld-linux). Host fallback OFF: everything must resolve
        // within the loaded graph. SKIP (not fail) if libm is absent — never fabricate.
        let Some(libm_path) = find_libm() else {
            eprintln!("real_libm_graph_links_and_relocates: no host libm.so.6; skipping");
            return;
        };
        let search: Vec<PathBuf> = HOST_LIB_DIRS
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect();

        // Host fallback OFF — prove the loaded graph satisfies the relocations on its own.
        let linker = Linker::new(search).with_host_fallback(false);
        let set = linker
            .load(&libm_path)
            .unwrap_or_else(|e| panic!("link libm graph: {e}"));

        // libm + libc + ld-linux all loaded + mapped (ld-linux deduped: libm and libc both need it).
        let sonames: Vec<&str> = set.objects.iter().map(|o| o.soname.as_str()).collect();
        eprintln!(
            "real_libm_graph: objects={sonames:?} stats={:?} unresolved={}",
            set.stats,
            set.unresolved.len()
        );
        assert_eq!(set.objects[0].soname, "libm.so.6", "root is libm");
        assert!(
            set.object("libc.so.6").is_some(),
            "libc.so.6 loaded as a dep"
        );
        // ld-linux must appear exactly once despite two referrers (libm + libc).
        let ld_count = sonames.iter().filter(|s| s.starts_with("ld-linux")).count();
        assert_eq!(ld_count, 1, "ld-linux deduped to one object: {sonames:?}");

        // Every object mapped with a real base + span.
        for obj in &set.objects {
            assert!(obj.load_base() != 0, "{}: real base", obj.soname);
            assert!(
                obj.map_stats.segments_mapped > 0,
                "{}: segments",
                obj.soname
            );
        }

        // libm's symbol relocations (32 GLOB_DAT on glibc) resolve within the graph (host off):
        // the 3 weak GNU/ITM ones are weak-undef → 0 (legal), the rest non-null. No unresolved
        // STRONG for libm.
        let libm = &set.objects[0];
        assert!(
            libm.sym_stats.total_applied() >= 30,
            "libm symbol relocs applied: {:?}",
            libm.sym_stats
        );
        let libm_unresolved: Vec<&UnresolvedSymbol> = set
            .unresolved
            .iter()
            .filter(|u| u.object == "libm.so.6")
            .collect();
        assert!(
            libm_unresolved.is_empty(),
            "libm must have no unresolved strong symbols: {libm_unresolved:?}"
        );

        // Cross-module errno TLS: libm's one TPOFF64 references errno, defined in libc's PT_TLS.
        // With libc in the multi-module TlsLayout, it applies (not host-dlsym).
        assert_eq!(
            libm.tls_stats.tpoff64_applied, 1,
            "libm's errno TPOFF64 applies via libc's PT_TLS in the multi-module layout"
        );
        // The layout indexes errno → a negative (below-TP) offset from libc's block.
        let errno_off = set.tls_layout.tp_offset_of("errno");
        assert!(
            errno_off.is_some_and(|v| v < 0),
            "errno tp-relative offset must be negative (variant-II): {errno_off:?}"
        );

        // libc's IRELATIVE (ifunc) count is the documented deferred ifunc tail (large on glibc),
        // NOT a failure. Aggregate must be > 0 (libm has 0; libc has many).
        assert!(
            set.stats.irelative_deferred > 0,
            "libc's ifunc IRELATIVE are deferred (the documented ifunc tail): {:?}",
            set.stats
        );

        // No leak: dropping the set munmaps the whole graph.
        drop(set);
    }
}
