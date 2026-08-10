#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE

#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static const char eclipse_android_settings_path[] =
    "/data/local/tmp/ClientAppSettings.json";
static const char eclipse_host_settings_env[] =
    "ECLIPSE_CLIENT_APP_SETTINGS_PATH";

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

__attribute__((visibility("default"))) int open(const char* path, int flags,
                                                ...) {
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  return (int)syscall(SYS_openat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int open64(const char* path, int flags,
                                                  ...) {
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  return (int)syscall(SYS_openat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), flags, mode);
}

__attribute__((visibility("default"))) int openat(int dirfd, const char* path,
                                                  int flags, ...) {
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  return (int)syscall(SYS_openat, dirfd, eclipse_redirect_settings_path(path),
                      flags, mode);
}

__attribute__((visibility("default"))) int openat64(int dirfd, const char* path,
                                                    int flags, ...) {
  va_list args;
  va_start(args, flags);
  mode_t mode = eclipse_open_mode(flags, &args);
  va_end(args);
  return (int)syscall(SYS_openat, dirfd, eclipse_redirect_settings_path(path),
                      flags, mode);
}

__attribute__((visibility("default"))) int __open_2(const char* path,
                                                    int flags) {
  return (int)syscall(SYS_openat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), flags, 0);
}

__attribute__((visibility("default"))) int __open64_2(const char* path,
                                                      int flags) {
  return (int)syscall(SYS_openat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), flags, 0);
}

__attribute__((visibility("default"))) int access(const char* path, int mode) {
  return (int)syscall(SYS_faccessat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), mode);
}

__attribute__((visibility("default"))) int stat64(const char* path,
                                                  struct stat64* out) {
  return (int)syscall(SYS_newfstatat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), out, 0);
}

__attribute__((visibility("default"))) int lstat64(const char* path,
                                                   struct stat64* out) {
  return (int)syscall(SYS_newfstatat, AT_FDCWD,
                      eclipse_redirect_settings_path(path), out,
                      AT_SYMLINK_NOFOLLOW);
}
