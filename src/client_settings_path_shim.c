#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE

#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char eclipse_android_settings_path[] =
    "/data/local/tmp/ClientAppSettings.json";
static const char eclipse_host_settings_env[] =
    "ECLIPSE_CLIENT_APP_SETTINGS_PATH";

typedef int (*eclipse_open_fn)(const char*, int, ...);
typedef int (*eclipse_openat_fn)(int, const char*, int, ...);
typedef int (*eclipse_open_2_fn)(const char*, int);
typedef int (*eclipse_access_fn)(const char*, int);
typedef int (*eclipse_stat64_fn)(const char*, struct stat64*);

static const char* eclipse_redirect_settings_path(const char* path) {
  if (path == NULL || strcmp(path, eclipse_android_settings_path) != 0) {
    return path;
  }
  const char* host_path = getenv(eclipse_host_settings_env);
  return host_path != NULL && host_path[0] != '\0' ? host_path : path;
}

static mode_t eclipse_open_mode(int flags, va_list* args) {
  int needs_mode = (flags & O_CREAT) != 0;
#ifdef O_TMPFILE
  needs_mode = needs_mode || (flags & O_TMPFILE) == O_TMPFILE;
#endif
  return needs_mode ? (mode_t)va_arg(*args, int) : (mode_t)0;
}

static void* eclipse_next(void** slot, const char* name) {
  void* next = __atomic_load_n(slot, __ATOMIC_ACQUIRE);
  if (next != NULL) {
    return next;
  }
  next = dlsym(RTLD_NEXT, name);
  if (next == NULL) {
    static const char prefix[] =
        "eclipse client-settings shim: no next definition of ";
    (void)!write(STDERR_FILENO, prefix, sizeof prefix - 1);
    (void)!write(STDERR_FILENO, name, strlen(name));
    (void)!write(STDERR_FILENO, "\n", 1);
    abort();
  }
  __atomic_store_n(slot, next, __ATOMIC_RELEASE);
  return next;
}

__attribute__((visibility("default"))) int open(const char* path, int flags,
                                                ...) {
  static void* next;
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  eclipse_open_fn next_open = (eclipse_open_fn)eclipse_next(&next, "open");
  return next_open(eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int open64(const char* path, int flags,
                                                  ...) {
  static void* next;
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  eclipse_open_fn next_open = (eclipse_open_fn)eclipse_next(&next, "open64");
  return next_open(eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int openat(int dirfd, const char* path,
                                                  int flags, ...) {
  static void* next;
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  eclipse_openat_fn next_openat =
      (eclipse_openat_fn)eclipse_next(&next, "openat");
  return next_openat(dirfd, eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int openat64(int dirfd, const char* path,
                                                    int flags, ...) {
  static void* next;
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  eclipse_openat_fn next_openat =
      (eclipse_openat_fn)eclipse_next(&next, "openat64");
  return next_openat(dirfd, eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int __open_2(const char* path,
                                                    int flags) {
  static void* next;
  eclipse_open_2_fn next_open =
      (eclipse_open_2_fn)eclipse_next(&next, "__open_2");
  return next_open(eclipse_redirect_settings_path(path), flags);
}

__attribute__((visibility("default"))) int __open64_2(const char* path,
                                                      int flags) {
  static void* next;
  eclipse_open_2_fn next_open =
      (eclipse_open_2_fn)eclipse_next(&next, "__open64_2");
  return next_open(eclipse_redirect_settings_path(path), flags);
}

__attribute__((visibility("default"))) int access(const char* path, int mode) {
  static void* next;
  eclipse_access_fn next_access =
      (eclipse_access_fn)eclipse_next(&next, "access");
  return next_access(eclipse_redirect_settings_path(path), mode);
}

__attribute__((visibility("default"))) int stat64(const char* path,
                                                  struct stat64* out) {
  static void* next;
  eclipse_stat64_fn next_stat =
      (eclipse_stat64_fn)eclipse_next(&next, "stat64");
  return next_stat(eclipse_redirect_settings_path(path), out);
}

__attribute__((visibility("default"))) int lstat64(const char* path,
                                                   struct stat64* out) {
  static void* next;
  eclipse_stat64_fn next_stat =
      (eclipse_stat64_fn)eclipse_next(&next, "lstat64");
  return next_stat(eclipse_redirect_settings_path(path), out);
}
