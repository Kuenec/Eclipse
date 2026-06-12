/*
 * Eclipse clean-room bionic stdio VARIADIC shim — 2026-06-12.
 *
 * bionic's public pre-API-23 stdio ABI (AOSP NDK <stdio.h> + <bits/struct_file.h>) declares
 * `extern FILE __sF[]` — an array of 152-byte (LP64) `struct __sFILE` STRUCTS — with
 * `stdin == &__sF[0]`, `stdout == &__sF[1]`, `stderr == &__sF[2]`. Eclipse backs `__sF` with a
 * bionic-shaped 3x152-byte sentinel object (src/loader/native_provider.rs), so a bionic-compiled
 * caller's `&__sF[i]` is a deterministic Eclipse-owned address — NOT a glibc FILE*. Every
 * FILE*-consuming stdio call must therefore remap those three sentinel addresses to the host glibc
 * streams before forwarding (core dump 782252: crashpad's `fputs(msg, &__sF[2])` handed glibc a
 * non-FILE pointer and the crash handler's own logging SIGSEGV'd).
 *
 * The fixed-arity FILE* natives are Rust (native_provider.rs). These three cannot be: `fprintf`
 * and `fscanf` are C-variadic (Rust stable cannot DEFINE a variadic `extern "C"` fn — the
 * `c_variadic` feature is nightly-only) and `vfprintf` takes a `va_list` (no stable Rust spelling).
 * Same pattern as src/loader/liblog_shim.c. Each remaps the stream via the Rust-exported
 * `eclipse_sf_translate_stream`, then forwards to the host glibc v*-routine.
 *
 * Provenance: written from the PUBLIC bionic/NDK stdio C-ABI signatures + the C standard library
 * (vfprintf/vfscanf/va_start/va_end). No bionic / NDK / linker source was read.
 *
 * Soundness: no global state (reentrant); no buffers; NULL formats are substituted with "" (the
 * defensive posture liblog_shim.c established) instead of handing glibc a NULL format.
 */

#include <stdarg.h>
#include <stdio.h>

/*
 * The Eclipse-owned stream translator, defined in Rust (`#[no_mangle] extern "C"`,
 * src/loader/native_provider.rs): maps the three bionic `&__sF[i]` sentinel addresses to the host
 * glibc stdin/stdout/stderr and passes every other pointer through unchanged.
 */
extern FILE *eclipse_sf_translate_stream(FILE *stream);

/*
 * int fprintf(FILE* stream, const char* fmt, ...)
 *
 * Public stdio contract: format and write to `stream`; return the byte count written, negative on
 * error. Registered under the bionic import name "fprintf" by the Eclipse native provider.
 */
int eclipse_fprintf(FILE *stream, const char *fmt, ...) {
    va_list ap;
    int written;

    if (fmt == NULL) {
        fmt = "";
    }

    va_start(ap, fmt);
    written = vfprintf(eclipse_sf_translate_stream(stream), fmt, ap);
    va_end(ap);
    return written;
}

/*
 * int fscanf(FILE* stream, const char* fmt, ...)
 *
 * Public stdio contract: scan from `stream` per `fmt`; return the number of conversions, or EOF on
 * input failure before any conversion. Registered under the bionic import name "fscanf".
 */
int eclipse_fscanf(FILE *stream, const char *fmt, ...) {
    va_list ap;
    int converted;

    if (fmt == NULL) {
        fmt = "";
    }

    va_start(ap, fmt);
    converted = vfscanf(eclipse_sf_translate_stream(stream), fmt, ap);
    va_end(ap);
    return converted;
}

/*
 * int vfprintf(FILE* stream, const char* fmt, va_list ap)
 *
 * Not variadic itself (the caller already materialized the va_list), but va_list has no stable
 * Rust spelling, so the remap+forward lives here too. Registered under the bionic import name
 * "vfprintf".
 */
int eclipse_vfprintf(FILE *stream, const char *fmt, va_list ap) {
    if (fmt == NULL) {
        fmt = "";
    }
    return vfprintf(eclipse_sf_translate_stream(stream), fmt, ap);
}
