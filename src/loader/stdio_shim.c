
#include <stdarg.h>
#include <stdio.h>

extern FILE* eclipse_sf_translate_stream(FILE* stream);

int eclipse_fprintf(FILE* stream, const char* fmt, ...) {
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

int eclipse_fscanf(FILE* stream, const char* fmt, ...) {
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

int eclipse_vfprintf(FILE* stream, const char* fmt, va_list ap) {
  if (fmt == NULL) {
    fmt = "";
  }
  return vfprintf(eclipse_sf_translate_stream(stream), fmt, ap);
}
