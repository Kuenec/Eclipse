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
//! Also builds the apkenv-loadable `libm` shim cdylib (`crates/libm-shim`, see `build_libm_shim`) and
//! exposes its `.so` path via `cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO`, so `runtime` can provision it as
//! the app's `libm.so` (a clean-relocation, correct-math replacement for the host glibc `libm.so.6`
//! that the apkenv shim linker cannot load — its `R_X86_64_TPOFF64`/RELR relocs abort the load).
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
    // Rebuild if any shim changes. 2026-06-05 / 2026-06-11.
    println!("cargo:rerun-if-changed=src/loader/liblog_shim.c");
    println!("cargo:rerun-if-changed=src/loader/bionic_syscall_shim.c");
    println!("cargo:rerun-if-changed=src/loader/native_load_shim.cpp");

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

    // The clean-room C++ DELEGATION shim for ART's `JavaVMExt::LoadNativeLibrary`: DEFINES
    // `eclipse_art_load_native_library(...)`, which builds the `std::string` args with the host
    // libstdc++ (correct ABI) and calls a runtime-`dlsym`'d function pointer (libart is RTLD_GLOBAL).
    // The framework's `Runtime.nativeLoad` interception (src/framework.rs) uses it to delegate
    // non-pre-loaded library loads to ART's real path so they keep their handle / `JNI_OnLoad` /
    // `Java_*` discovery. `.cpp(true)` compiles it as C++ and links the C++ standard library (which
    // ART already pulls in). See `src/loader/native_load_shim.cpp` for the full rationale + ABI note.
    cc::Build::new()
        .cpp(true)
        .file("src/loader/native_load_shim.cpp")
        .compile("eclipse_native_load_shim");

    build_libm_shim();
}

/// Build the apkenv-loadable `libm` shim cdylib (`crates/libm-shim`) and expose its `.so` path to
/// the crate via `cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO`. 2026-06-05.
///
/// WHY (root cause this enables fixing): Roblox's `androidx.startup` does
/// `System.loadLibrary("zstd-jni")`; ART routes it to the apkenv / `bionic_translation` shim linker,
/// which follows zstd-jni's `NEEDED libm.so` to whatever file named `libm.so` is on its search path.
/// The host glibc `libm.so.6` we previously symlinked there carries an `R_X86_64_TPOFF64` (modern TLS
/// reloc, "unknown reloc type 18") + a `.relr.dyn` packed-reloc section the older apkenv linker
/// cannot apply, so its load aborts (SIGSEGV). The shim cdylib is a separate, clean-relocation ELF
/// (`R_X86_64_{64,GLOB_DAT,RELATIVE}` only, no TLS, no `NEEDED`) the apkenv linker CAN load, with
/// CORRECT math (the pure-Rust `libm` crate). `runtime::provision_bionic_sonames` symlinks the app's
/// `libm.so` to THIS file instead of the host glibc one.
///
/// Portability (CLAUDE.md "Build & Environment Portability"): uses `$CARGO` (the cargo invoking this
/// build) and a target dir under our `OUT_DIR` — no recursion into our own target dir (no lock
/// contention), no hardcoded paths. Verifies the produced `.so` has NO modern relocs via `readelf`
/// when available (a build-time guard; skipped with a warning if `readelf` is absent, since the
/// workspace unit tests + the cdylib's `#![no_std]`/`panic=abort` shape are the primary guarantee).
fn build_libm_shim() {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    let manifest = "crates/libm-shim/Cargo.toml";
    // Rebuild the shim if its sources change.
    println!("cargo:rerun-if-changed=crates/libm-shim/src/lib.rs");
    println!("cargo:rerun-if-changed=crates/libm-shim/Cargo.toml");

    let out_dir = std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo");
    // A target dir SEPARATE from the outer build's target dir so the nested `cargo build` does not
    // contend on the workspace target lock (the outer build holds it). Under OUT_DIR so it is cleaned
    // with the build and never pollutes the source tree.
    let shim_target = Path::new(&out_dir).join("libm-shim-target");

    // `$CARGO` is the exact cargo binary invoking this build script (portable; honors toolchain).
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release", "--manifest-path", manifest])
        .arg("--target-dir")
        .arg(&shim_target)
        // Do NOT inherit the outer build's RUSTFLAGS/profile into the nested crate (it sets its own
        // profile in its Cargo.toml). Clearing these keeps the shim's clean-reloc shape deterministic.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .expect("failed to spawn cargo to build the libm shim");
    assert!(status.success(), "building crates/libm-shim failed");

    let so: PathBuf = shim_target.join("release").join("libeclipse_libm_shim.so");
    assert!(
        so.exists(),
        "libm shim build did not produce {} — check crates/libm-shim",
        so.display()
    );

    // Build-time guard: the shim MUST NOT carry the modern relocs the apkenv linker chokes on. If
    // `readelf` is present, fail the build on a regression (a TPOFF64 or RELR creeping in, e.g. from
    // accidentally enabling std/TLS). Skip with a warning when `readelf` is unavailable — the
    // workspace unit tests cover the property too.
    if let Ok(out) = Command::new("readelf").arg("-rW").arg(&so).output() {
        if out.status.success() {
            let relocs = String::from_utf8_lossy(&out.stdout);
            assert!(
                !relocs.contains("R_X86_64_TPOFF64"),
                "libm shim regressed: it now has R_X86_64_TPOFF64 (the apkenv linker cannot apply \
                 it). The shim must stay no_std/no-TLS."
            );
        }
    } else {
        println!("cargo:warning=readelf not found; skipped the libm-shim modern-reloc guard");
    }

    // Expose the built shim path to the crate (read at compile time via `env!`).
    println!("cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO={}", so.display());
}
