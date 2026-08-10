
#include <stdarg.h>

extern long syscall(long number, ...);

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
