//! Eclipse-owned, from-scratch Rust bionic loader (component-map D · 🟢 target).
//!
//! 2026-06-05: This module is the **first foundational, self-contained, unit-testable piece**
//! of Eclipse's own pure-Rust dynamic loader for Roblox's bionic-linked `.so` files — the
//! charter's #1 Rust-port priority (`docs/component-map.md`, `docs/bionic-loader-strategy.md`).
//!
//! ## Why this exists / the wall it targets
//! Eclipse boots ART and reaches Roblox's own `Application.onCreate`, but
//! `System.loadLibrary("roblox")`'s transitive native libs **fail to relocate** in the
//! vendored apkenv-era C `bionic_translation` shim linker: it aborts on
//! `unknown reloc type 18` (= `R_X86_64_TPOFF64`, static-TLS) and lacks `DT_RELR` (compressed
//! relative relocations) + `BIND_NOW` (eager binding). Those are pervasive modern-toolchain
//! defaults (`docs/bionic-loader-strategy.md` §1) — the limitation is the linker, not the libs.
//!
//! ## What this module provides *now*
//! [`elf`] — a **pure-Rust x86-64 `ET_DYN` decoder** that reads a `.so`'s bytes (ELF header,
//! program headers, `.dynamic`, dynamic symbol table) and produces exactly the inputs [`reloc`]
//! consumes: a `Vec<reloc::Rela>` (from `.rela.dyn` + `.rela.plt`), the raw `DT_RELR` word table,
//! the dynamic symbols, the parsed `DynInfo` (incl. `BIND_NOW` detection), and the `PT_LOAD`
//! layout (for the later `mmap` step). It converts the dynamic section's virtual addresses to
//! file offsets via the `PT_LOAD` table, is `#![forbid(unsafe_code)]`, and is a total parser
//! (every read bounds-checked into a typed [`elf::ElfError`], never a panic/UB).
//!
//! [`reloc`] — a **pure-Rust x86-64 ELF relocation applier** over a [`reloc::RelocImage`]
//! abstraction (a `&mut [u8]` library image + a [`reloc::SymbolResolver`] + the module's
//! static-TLS offset). It applies exactly the relocation types that wall `libroblox.so`:
//! `R_X86_64_RELATIVE`/`GLOB_DAT`/`JUMP_SLOT`/`64`/`TPOFF64`, plus `DT_RELR` bitmap decoding,
//! all bounds-checked with typed [`reloc::RelocError`] (never UB), and an exhaustive type
//! dispatch (unknown type → `Err`, the exact gap the apkenv linker hit).
//!
//! [`map`] — the **PT_LOAD segment mapper + base relocator**: it reserves one contiguous
//! anonymous region for a parsed `ElfImage`, copies each `PT_LOAD`'s file bytes to `base + vaddr`
//! (the bss tail zero-filled by fresh anonymous pages, page-overlap correct by construction),
//! applies the **base-only** relocations through [`reloc`] (`R_X86_64_RELATIVE` + `DT_RELR`), and
//! `mprotect`s each segment to its final `p_flags`. RAII: [`map::MappedObject`] `munmap`s on drop.
//! This is the one module that uses `unsafe` (the `mmap`/`mprotect`/`munmap` syscalls + the write
//! through the mapping), each block carrying a `// SAFETY:` justification (AGENTS.md §2.3).
//!
//! [`resolve`] — the **symbol-resolution scope**: a [`reloc::SymbolResolver`] backed by an ordered
//! list of pluggable providers (a [`resolve::LoadedObjectProvider`] over a mapped object's exported
//! definitions; a [`resolve::HostDlsymProvider`] over the host process via `dlsym(RTLD_DEFAULT)`).
//! It applies the System V gABI rules (defined-export-only, first-wins with a global overriding a
//! weak, weak-undef → 0, strong-undef → unresolved). With it, [`map`] applies the
//! symbol-dependent relocations (`GLOB_DAT`/`JUMP_SLOT`/`R_X86_64_64`) it previously deferred.
//!
//! [`tls`] — the **static-TLS layout + `R_X86_64_TPOFF64` offsets**: a [`tls::TlsLayout`] stacks
//! one or more modules' `PT_TLS` blocks below the thread pointer per the x86-64 variant-II model
//! (`offset_i = offset_{i-1} + roundup(size_i, align_i)`; a symbol's tp-relative value is
//! `-offset_i + st_value`), assembles the init block (`.tdata` copied, `.tbss` zeroed, aligned),
//! and a [`tls::TlsResolver`] resolves a `TPOFF64` symbol to that tp-relative value (delegating
//! non-TLS relocations to the inner [`resolve`] resolver). With it, [`map`] applies the last
//! non-ifunc relocation class. **It computes the layout/offsets + applies `TPOFF64`; it does NOT
//! bind the block to a live thread pointer (`%fs`/TCB) — that is a separate integration step (see
//! `tls.rs` and AGENTS.md §5).**
//!
//! [`link`] — the **dependency-graph orchestrator** built on the four cores: given a root `.so`,
//! it transitively loads the `DT_NEEDED` graph (BFS, soname-deduped, cycle-safe), builds the
//! combined cross-object symbol [`resolve::Scope`] + a multi-module [`tls::TlsLayout`], and
//! relocates every loaded object against that global scope (base + symbol + static-TLS). It counts
//! `IRELATIVE` as deferred (the ifunc tail), records unresolved-strong symbols without fabricating
//! addresses, and RAII-`munmap`s the whole graph on drop. After relocation it honors `PT_GNU_RELRO`
//! ([`map::MappedObject::apply_relro`] — `mprotect`s the read-only-after-reloc region RO). A
//! **root-only / env-provided-deps** mode ([`link::Linker::with_tolerate_missing_deps`]) records an
//! absent `DT_NEEDED` instead of erroring, so a root maps + base-relocates with its deps supplied by
//! the env/shim (the bionic load shape — e.g. `libroblox.so`'s 10 bionic deps). It maps + relocates;
//! it does **not** bind `%fs`/TCB, execute ifunc resolvers, or run init — the runtime integration tail.
//!
//! [`bionic_env`] — the **first bionic-env resolution scope** tailored to `libroblox.so`: a
//! configurable, ordered [`resolve::Scope`] of providers (host `libEGL`/`libGLESv2` via `dlopen`
//! if present, then a host libc/m/dl/pthread [`resolve::HostDlsymProvider`]) that resolves the
//! subset of the engine's 584 UND imports the **host** can supply, plus a name-based categorizer
//! ([`bionic_env::categorize_imports`]) that buckets every import into the Eclipse-bionic-native
//! work-list. **HONEST BASELINE:** host glibc/GL addresses prove the symbol-relocation pipeline but
//! are **not** bionic-ABI-correct execution (struct/errno/pthread/FILE differ); the scope is built
//! so Eclipse-owned bionic natives can be prepended later (see `bionic_env.rs` + AGENTS.md §5).
//!
//! `elf.rs` decodes the file format; `reloc.rs` applies relocations; `map.rs` lays the segments
//! out and drives both base and (via `resolve.rs`) symbol relocations — a clean boundary (the
//! decoded `reloc::Rela` is the applier's input type, with no glue). `link.rs` ties them into a
//! whole-graph loader.
//!
//! ## What this module deliberately does NOT do (the broader loader, built on this core)
//! This is the **decode + map + relocate core**, not a full working loader. It assembles the
//! static-TLS block and computes its tp-relative offsets ([`tls`]) but does **not** bind that block
//! to a live thread pointer (`%fs`/TCB), so `R_X86_64_TPOFF64`'s computed offsets are correct but
//! not yet *reachable at runtime*; nor does it execute the library's `IRELATIVE` ifunc resolvers,
//! run init functions, model the bionic two-namespace scope, or replace/augment the apkenv linker.
//! Those are the next steps that build on this core
//! (see the submodule docs and AGENTS.md §5 next-actions). Wiring it into the engine-load path
//! requires that broader loader and is **main-loop / dev-host only** (the cyber-safeguard
//! false-positives on linker work).

pub mod bionic_env;
pub mod bionic_pthread;
pub mod bionic_sysconf;
pub mod elf;
pub mod engine;
pub mod init_run;
pub mod jni_mangle;
pub mod jni_register;
pub mod link;
pub mod looper;
pub mod map;
pub mod native_provider;
pub mod ndk_registry;
pub mod opensl;
pub mod reloc;
pub mod resolve;
pub mod tls;
