//! Eclipse build script — 2026-06-05.
//!
//! Compiles the clean-room liblog VARIADIC shim (`src/loader/liblog_shim.c`) into a static
//! library that is linked into the crate. The shim DEFINES the two C-variadic bionic liblog
//! functions (`__android_log_print` / `__android_log_assert`) that Rust stable cannot define
//! (the `c_variadic` feature is nightly-only); they forward to the Rust sink `eclipse_liblog_emit`.
//! See `src/loader/liblog_shim.c` and `src/loader/native_provider.rs` for the full rationale.
//!
//! ## Why the `cc` crate (justified per AGENTS.md §2.1 / §5)
//! `cc` is the standard, well-established Rust build-time bridge for compiling a small C source as
//! part of a Cargo build. It is the minimal, idiomatic way to obtain a varargs-defining object on
//! stable Rust. It was already present transitively in `Cargo.lock`, so promoting it to a direct
//! `[build-dependencies]` adds no new transitive crates.
//!
//! ## Portability (AGENTS.md §2 / CLAUDE.md "Build & Environment Portability")
//! `cc` DISCOVERS the host C compiler (honoring the `CC`/`CFLAGS` environment, then standard
//! compiler names) — no hardcoded paths, no vendor/SDK assumptions. If no C compiler is available,
//! `cc::Build::compile` fails the build with an actionable error naming the missing tool. A C
//! compiler is the documented build requirement for this shim.

fn main() {
    // Rebuild if the shim changes (the only C input). 2026-06-05.
    println!("cargo:rerun-if-changed=src/loader/liblog_shim.c");

    // `compile` emits `cargo:rustc-link-lib=static=eclipse_liblog_shim` + the link-search path, so
    // the archive is linked into the lib, the bin, AND the test harness. The shim's two symbols are
    // pulled in because Rust takes their addresses (see native_provider.rs), and the shim's one
    // undefined symbol (`eclipse_liblog_emit`) is satisfied by the Rust `#[no_mangle]` sink.
    // 2026-06-05: `cc` discovers the toolchain; if no C compiler exists it panics here with an
    // actionable "Failed to find tool ... Is the C compiler installed?" message (the documented
    // build requirement), per AGENTS.md §2 portability.
    cc::Build::new()
        .file("src/loader/liblog_shim.c")
        .compile("eclipse_liblog_shim");
}
