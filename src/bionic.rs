//! bionic native-code loading & ABI shim (component-map D · 🟢 target / 🔴 v1).
//!
//! The hardest, most security-critical layer: load Roblox's bionic-linked `.so` on a glibc
//! host and bridge the bionic↔glibc ABI (stdio `FILE`, pthread, TLS layout, `errno`,
//! dlfcn, C++ ABI, Android packed relocations / `DT_ANDROID_REL[A]`).
//!
//! Strategy (stability first): **v1** FFI-bridges the proven C `bionic_translation` linker;
//! **target** reimplements on pure-Rust `elf_loader`/`dlopen-rs` with a custom symbol
//! resolver pointing unresolved bionic symbols at our Rust shim — behind an ABI
//! conformance test suite. This is the #1 Rust-port priority.
//!
//! Planned deps: `object`, `elf_loader`, `dlopen-rs`, `memmap2`, `libc`.
//! TODO(M3): port the loader/shim to Rust incrementally; do NOT do this first.
