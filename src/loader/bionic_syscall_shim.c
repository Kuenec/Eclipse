/*
 * Eclipse clean-room bionic `syscall(2)` VARIADIC shim — 2026-06-05.
 *
 * `libroblox.so` imports `long syscall(long number, ...)` and the init path calls it directly for
 * `SYS_gettid` (the libc++/protobuf static-init guard reads the kernel thread id this way). `syscall`
 * is C-variadic; Rust stable cannot DEFINE a variadic `extern "C"` function (the `c_variadic`
 * feature is nightly-only), and Eclipse builds on stable (clean-checkout portability, AGENTS.md
 * §2.11). This tiny shim DEFINES it.
 *
 * Why a host forward is CORRECT here (unlike the rest of the pthread family): `syscall` is a thin
 * kernel trampoline with NO libc-private state and an ABI identical between glibc and bionic on
 * x86-64 Linux — it marshals the (up to 6) integer/pointer arguments into the kernel calling
 * convention and traps. Forwarding to the host glibc `syscall(3)` therefore performs the EXACT same
 * kernel call bionic would. (Eclipse's own pthread shim still owns all the stateful `pthread_*`
 * objects; only this stateless trampoline is shared with the host.)
 *
 * Provenance: written from the PUBLIC `syscall(2)` C-ABI (the Linux man page: up to 6 long args after
 * the number) and the x86-64 kernel calling convention. No bionic / NDK / linker source was read.
 *
 * Soundness: no global state (reentrant); pulls a fixed 6 `long` varargs (the kernel uses only the
 * args the specific syscall defines; surplus args are ignored, never written through). Renamed to
 * `eclipse_bionic_syscall` so it does NOT collide with the host libc `syscall` symbol at link time;
 * Rust takes its address and registers it under the bionic import name "syscall".
 */

#include <stdarg.h>

/* The host libc variadic `syscall(3)` (declared by <unistd.h>; declared here to avoid pulling the
 * whole header and to keep the shim self-contained). */
extern long syscall(long number, ...);

/*
 * long eclipse_bionic_syscall(long number, ...)
 *
 * Pull up to 6 long-sized arguments after `number` (the Linux syscall ABI maximum) and forward them
 * to the host `syscall`. A syscall taking fewer arguments ignores the surplus (they sit in unused
 * argument registers), so passing all 6 is always safe.
 */
long eclipse_bionic_syscall(long number, ...) {
    va_list ap;
    va_start(ap, number);
    long a0 = va_arg(ap, long);
    long a1 = va_arg(ap, long);
    long a2 = va_arg(ap, long);
    long a3 = va_arg(ap, long);
    long a4 = va_arg(ap, long);
    long a5 = va_arg(ap, long);
    va_end(ap);
    return syscall(number, a0, a1, a2, a3, a4, a5);
}
