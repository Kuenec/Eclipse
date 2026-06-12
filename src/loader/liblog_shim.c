/*
 * Eclipse clean-room liblog VARIADIC shim — 2026-06-05.
 *
 * Rust stable cannot DEFINE a C-variadic `extern "C"` function (the `c_variadic` feature is
 * nightly-only), and Eclipse builds on stable (clean-checkout portability, AGENTS.md §2.11). The
 * `libroblox.so` bionic imports below are exactly such variadic functions (plus one `va_list`
 * variant, which has no stable Rust spelling either):
 *
 *     int  __android_log_print (int prio, const char* tag, const char* fmt, ...);
 *     void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...);
 *     int  __android_log_vprint(int prio, const char* tag, const char* fmt, va_list ap);
 *
 * (2026-06-12: `__android_log_vprint` added — `libbacktrace-native.so` imports it as one of the 2
 * unresolved strong imports that failed its Eclipse pre-load, sending its `System.loadLibrary`
 * into the apkenv shim linker's fatal NULL `_r_debug_ptr` write — core 866509.)
 *
 * This tiny shim DEFINES them per the PUBLIC liblog C-ABI (the documented signatures in
 * <android/log.h>): it formats the variadic argument list with vsnprintf into a bounded stack
 * buffer, then forwards the finished message to the Eclipse-owned NON-variadic sink
 * `eclipse_liblog_emit`, which is defined in Rust and routes to Eclipse's `tracing` log. Rust can
 * DECLARE these variadic externs and take their addresses on stable, so the loader binds the
 * engine's relocations to these shim definitions.
 *
 * Provenance: written from the PUBLIC liblog C-ABI signatures + the C standard library
 * (vsnprintf/va_start/va_end/abort). No bionic / NDK / liblog / linker source was read.
 *
 * Soundness: no global state (reentrant); a fixed stack buffer (no heap, no UB); vsnprintf always
 * NUL-terminates and never writes past the buffer; truncation is detected and handled safely.
 */

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

/*
 * The Eclipse-owned non-variadic sink, defined in Rust (`#[no_mangle] extern "C"`). It receives the
 * already-formatted message and routes it to Eclipse's logging. `prio` is the bionic
 * android_LogPriority; `tag`/`msg` are NUL-terminated C strings (never NULL — this shim substitutes
 * "" for a NULL tag/fmt).
 */
extern void eclipse_liblog_emit(int prio, const char *tag, const char *msg);

/*
 * 4 KiB matches bionic's per-message log line cap (LOGGER_ENTRY_MAX_PAYLOAD class). A larger
 * message is truncated and NUL-terminated by vsnprintf — the documented liblog behavior for an
 * over-long line — never a buffer overrun.
 */
#define ECLIPSE_LIBLOG_BUF 4096

/*
 * int __android_log_print(int prio, const char* tag, const char* fmt, ...)
 *
 * Public liblog contract: format `fmt` + varargs and write the line under `tag` at priority `prio`;
 * return the number of bytes written (> 0) on success, < 0 on error. This shim formats into a
 * bounded stack buffer and forwards to the Eclipse sink, returning the emitted byte count.
 */
int __android_log_print(int prio, const char *tag, const char *fmt, ...) {
    char buf[ECLIPSE_LIBLOG_BUF];
    va_list ap;

    if (fmt == NULL) {
        fmt = "";
    }

    va_start(ap, fmt);
    /* vsnprintf NUL-terminates within `buf` and returns the length that WOULD have been written
     * (excluding the NUL), or a negative value on an output/encoding error. */
    int written = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);

    if (written < 0) {
        /* Encoding/output error — emit nothing meaningful; report the error per contract. */
        return written;
    }

    eclipse_liblog_emit(prio, (tag != NULL) ? tag : "", buf);

    /* On truncation vsnprintf returns the untruncated length; the bytes actually emitted are at
     * most sizeof(buf)-1. Report the emitted byte count (> 0 on success per the liblog contract). */
    int emitted = (written < (int)sizeof(buf)) ? written : (int)(sizeof(buf) - 1);
    return (emitted > 0) ? emitted : 1;
}

/*
 * int __android_log_vprint(int prio, const char* tag, const char* fmt, va_list ap)
 *
 * Public liblog contract (<android/log.h>): identical to __android_log_print, but the caller has
 * already materialized the va_list. 2026-06-12: same bounded vsnprintf → eclipse_liblog_emit path
 * and the same return contract as __android_log_print above.
 */
int __android_log_vprint(int prio, const char *tag, const char *fmt, va_list ap) {
    char buf[ECLIPSE_LIBLOG_BUF];

    if (fmt == NULL) {
        fmt = "";
    }

    /* vsnprintf NUL-terminates within `buf` and returns the would-be length (excluding the NUL),
     * or a negative value on an output/encoding error. */
    int written = vsnprintf(buf, sizeof(buf), fmt, ap);

    if (written < 0) {
        /* Encoding/output error — emit nothing meaningful; report the error per contract. */
        return written;
    }

    eclipse_liblog_emit(prio, (tag != NULL) ? tag : "", buf);

    /* On truncation vsnprintf returns the untruncated length; the bytes actually emitted are at
     * most sizeof(buf)-1. Report the emitted byte count (> 0 on success per the liblog contract). */
    int emitted = (written < (int)sizeof(buf)) ? written : (int)(sizeof(buf) - 1);
    return (emitted > 0) ? emitted : 1;
}

/*
 * void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...)
 *
 * Public liblog contract: log an assertion failure at FATAL priority then abort the process
 * (noreturn). If `fmt` is non-NULL it is the formatted assertion message; otherwise bionic
 * synthesises a message from `cond`. This shim formats the message, emits it at FATAL via the
 * Eclipse sink, then calls abort() — matching the documented noreturn behavior.
 */
void __android_log_assert(const char *cond, const char *tag, const char *fmt, ...) {
    char buf[ECLIPSE_LIBLOG_BUF];

    if (fmt != NULL) {
        va_list ap;
        va_start(ap, fmt);
        int written = vsnprintf(buf, sizeof(buf), fmt, ap);
        va_end(ap);
        if (written < 0) {
            buf[0] = '\0';
        }
    } else {
        /* No format string: synthesise "Assertion failed: <cond>" (bionic's fallback shape). */
        (void)snprintf(buf, sizeof(buf), "Assertion failed: %s",
                       (cond != NULL) ? cond : "(unknown)");
    }

    /* ANDROID_LOG_FATAL == 7 (public <android/log.h> android_LogPriority). */
    eclipse_liblog_emit(7, (tag != NULL) ? tag : "", buf);

    abort(); /* noreturn — matches __android_log_assert's documented contract. */
}
