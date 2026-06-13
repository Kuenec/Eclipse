/*
 * Eclipse clean-room `sigaltstack(2)` CALLER-ATTRIBUTION shim — 2026-06-12 (core 1223806).
 *
 * Core 1223806 was a silent kernel `force_sigsegv()` kill: the signal-frame write to a
 * registered-but-unwritable SA_ONSTACK alternate stack faulted, the disposition reset to SIG_DFL,
 * and zero handler instructions ran. The altstack's OWNER was unprovable because the engine's
 * `sigaltstack@LIBC` import passed through to host glibc invisibly. Eclipse now owns the import:
 * this shim captures WHO called it (`__builtin_return_address(0)` — stable Rust has no spelling
 * for the caller's return address; this is the established clean-room C-shim pattern of
 * `liblog_shim.c`) and forwards to the Rust callee, which performs the actual layout-identical
 * forward to host glibc and logs/records the registration with caller attribution.
 *
 * Provenance: written from the PUBLIC `sigaltstack(2)` C ABI (two `stack_t` pointers, int return;
 * bionic and glibc `stack_t` are layout-identical on x86-64 — `{void* ss_sp; int ss_flags;
 * size_t ss_size}`) and the GCC/Clang `__builtin_return_address` documentation. No bionic / NDK /
 * linker source was read. The pointers are passed through OPAQUELY (declared `void*` here so the
 * shim needs no libc headers); the Rust side types them as `libc::stack_t`.
 *
 * Soundness: no state, no allocation — one builtin read + a tail call. Named
 * `eclipse_sigaltstack` so it never collides with the host libc `sigaltstack` at link time; Rust
 * takes its address and registers it under the bionic import name "sigaltstack".
 */

/* The Rust callee (src/loader/native_provider.rs, `#[no_mangle]`): forwards to host glibc
 * `sigaltstack` and records the registration. */
extern int eclipse_sigaltstack_record(const void* ss, void* old_ss, const void* caller);

int eclipse_sigaltstack(const void* ss, void* old_ss) {
    return eclipse_sigaltstack_record(ss, old_ss, __builtin_return_address(0));
}
