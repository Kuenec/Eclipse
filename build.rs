//! Eclipse build script — 2026-06-05.
//!
//! Compiles the clean-room C VARIADIC shims (`src/loader/liblog_shim.c`,
//! `src/loader/bionic_syscall_shim.c`) into static libraries linked into the crate. They DEFINE the
//! C-variadic bionic functions Rust stable cannot define (the `c_variadic` feature is nightly-only):
//! the liblog `__android_log_print` / `__android_log_assert` (forward to the Rust sink
//! `eclipse_liblog_emit`), and `eclipse_bionic_syscall(long, ...)` (forwards varargs to the host
//! `syscall(3)` for the bionic `syscall` import — called directly for `SYS_gettid` in the init path).
//! See `src/loader/liblog_shim.c` / `src/loader/bionic_syscall_shim.c` / `src/loader/native_provider.rs`
//! / `src/loader/bionic_pthread.rs` for the full rationale.
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
    // Rebuild if either C shim changes. 2026-06-05.
    println!("cargo:rerun-if-changed=src/loader/liblog_shim.c");
    println!("cargo:rerun-if-changed=src/loader/bionic_syscall_shim.c");

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

    // The clean-room bionic `syscall(2)` VARIADIC shim: DEFINES `eclipse_bionic_syscall(long, ...)`
    // (forwards varargs to the host `syscall(3)`) so the engine's bionic `syscall` import — called
    // directly for `SYS_gettid` in the init path — binds to a real, ABI-correct kernel trampoline.
    // Variadic *definitions* need nightly Rust; this C shim provides it on stable, like liblog above.
    // See `src/loader/bionic_pthread.rs` for why a host forward of this stateless trampoline is sound.
    cc::Build::new()
        .file("src/loader/bionic_syscall_shim.c")
        .compile("eclipse_bionic_syscall_shim");
}
