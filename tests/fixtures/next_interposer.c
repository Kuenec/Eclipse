#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE

#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char fixture_path[] = "/eclipse-fixture/next-interposer";
static const uid_t fixture_uid = 4242;

typedef int (*fixture_open_fn)(const char*, int, ...);
typedef int (*fixture_access_fn)(const char*, int);
typedef int (*fixture_stat64_fn)(const char*, struct stat64*);

static void* fixture_next(const char* name) {
  void* next = dlsym(RTLD_NEXT, name);
  if (next == NULL) {
    abort();
  }
  return next;
}

__attribute__((visibility("default"))) int open(const char* path, int flags,
                                                ...) {
  va_list args;
  va_start(args, flags);
  mode_t mode = (flags & O_CREAT) != 0 ? (mode_t)va_arg(args, int) : 0;
  va_end(args);
  fixture_open_fn next_open = (fixture_open_fn)fixture_next("open");
  if (strcmp(path, fixture_path) == 0) {
    return next_open("/dev/null", flags, mode);
  }
  return next_open(path, flags, mode);
}

__attribute__((visibility("default"))) int access(const char* path, int mode) {
  if (strcmp(path, fixture_path) == 0) {
    return 0;
  }
  return ((fixture_access_fn)fixture_next("access"))(path, mode);
}

__attribute__((visibility("default"))) int stat64(const char* path,
                                                  struct stat64* out) {
  int result = ((fixture_stat64_fn)fixture_next("stat64"))(path, out);
  if (result == 0) {
    out->st_uid = fixture_uid;
  }
  return result;
}
