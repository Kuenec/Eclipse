
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>

extern void eclipse_liblog_emit(int prio, const char *tag, const char *msg);

#define ECLIPSE_LIBLOG_BUF 4096

int __android_log_print(int prio, const char *tag, const char *fmt, ...) {
    char buf[ECLIPSE_LIBLOG_BUF];
    va_list ap;

    if (fmt == NULL) {
        fmt = "";
    }

    va_start(ap, fmt);

    int written = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);

    if (written < 0) {

        return written;
    }

    eclipse_liblog_emit(prio, (tag != NULL) ? tag : "", buf);

    int emitted = (written < (int)sizeof(buf)) ? written : (int)(sizeof(buf) - 1);
    return (emitted > 0) ? emitted : 1;
}

int __android_log_vprint(int prio, const char *tag, const char *fmt, va_list ap) {
    char buf[ECLIPSE_LIBLOG_BUF];

    if (fmt == NULL) {
        fmt = "";
    }

    int written = vsnprintf(buf, sizeof(buf), fmt, ap);

    if (written < 0) {

        return written;
    }

    eclipse_liblog_emit(prio, (tag != NULL) ? tag : "", buf);

    int emitted = (written < (int)sizeof(buf)) ? written : (int)(sizeof(buf) - 1);
    return (emitted > 0) ? emitted : 1;
}

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

        (void)snprintf(buf, sizeof(buf), "Assertion failed: %s",
                       (cond != NULL) ? cond : "(unknown)");
    }

    eclipse_liblog_emit(7, (tag != NULL) ? tag : "", buf);

    abort();
}
