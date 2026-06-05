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
//! `elf.rs` decodes the file format; `reloc.rs` applies relocations; `map.rs` lays the segments
//! out and drives both base and (via `resolve.rs`) symbol relocations — a clean boundary (the
//! decoded `reloc::Rela` is the applier's input type, with no glue).
//!
//! ## What this module deliberately does NOT do (the broader loader, built on this core)
//! This is the **decode + map + relocate core**, not a full working loader. It does **not**
//! allocate the static-TLS block, set up the thread pointer (`%fs`/TCB), so `R_X86_64_TPOFF64`
//! stays deferred; nor does it execute the library's `IRELATIVE` ifunc resolvers, run init
//! functions, model the bionic two-namespace scope, or replace/augment the apkenv linker. Those
//! are the next steps that build on this core
//! (see the submodule docs and AGENTS.md §5 next-actions). Wiring it into the engine-load path
//! requires that broader loader and is **main-loop / dev-host only** (the cyber-safeguard
//! false-positives on linker work).

pub mod elf;
pub mod map;
pub mod reloc;
pub mod resolve;
