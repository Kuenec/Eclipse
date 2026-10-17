#include <elf.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <unistd.h>

enum {
  BIONIC_TLS_SLOT_APP = 2,
  BIONIC_TLS_SLOT_STACK_GUARD = 5,
  BIONIC_TLS_SLOT_ART_THREAD_SELF = 7,
  BIONIC_TLS_RESERVED_SLOTS = BIONIC_TLS_SLOT_ART_THREAD_SELF -
      BIONIC_TLS_SLOT_APP + 1,
  STACK_GUARD_INDEX = BIONIC_TLS_SLOT_STACK_GUARD - BIONIC_TLS_SLOT_APP,
};

__attribute__((section(".tdata.eclipse_bionic_slots"),
               used)) _Thread_local uintptr_t
    eclipse_bionic_tls_slots[BIONIC_TLS_RESERVED_SLOTS];

static uintptr_t process_stack_guard;

int eclipse_bionic_tls_slots_at_thread_pointer(void) {
  return (char*)eclipse_bionic_tls_slots ==
         (char*)__builtin_thread_pointer() +
             BIONIC_TLS_SLOT_APP * sizeof(uintptr_t);
}

int eclipse_bionic_stack_guard_seeded(void) {
  return process_stack_guard != 0 &&
         eclipse_bionic_tls_slots[STACK_GUARD_INDEX] == process_stack_guard;
}

static uintptr_t random_stack_guard(void) {
  const unsigned char* random = (const unsigned char*)getauxval(AT_RANDOM);
  uintptr_t guard = 0;
  if (random != NULL) {
    memcpy(&guard, random, sizeof guard);
  }
  return guard & ~(uintptr_t)0xff;
}

static int segment_protection(Elf64_Word flags) {
  return ((flags & PF_R) ? PROT_READ : 0) | ((flags & PF_W) ? PROT_WRITE : 0) |
         ((flags & PF_X) ? PROT_EXEC : 0);
}

static int segment_holds(const Elf64_Phdr* segment, uintptr_t bias,
                         uintptr_t address) {
  return address >= bias + segment->p_vaddr &&
         address + sizeof(uintptr_t) <=
             bias + segment->p_vaddr + segment->p_memsz;
}

static int write_initial_stack_guard(uintptr_t guard) {
  const Elf64_Phdr* headers = (const Elf64_Phdr*)getauxval(AT_PHDR);
  size_t count = getauxval(AT_PHNUM);
  if (headers == NULL) {
    return 0;
  }
  const Elf64_Phdr* phdr = NULL;
  const Elf64_Phdr* tls = NULL;
  const Elf64_Phdr* relro = NULL;
  for (size_t i = 0; i < count; i++) {
    switch (headers[i].p_type) {
      case PT_PHDR:
        phdr = &headers[i];
        break;
      case PT_TLS:
        tls = &headers[i];
        break;
      case PT_GNU_RELRO:
        relro = &headers[i];
        break;
    }
  }
  if (phdr == NULL || tls == NULL ||
      tls->p_filesz < sizeof eclipse_bionic_tls_slots) {
    return 0;
  }
  uintptr_t bias = (uintptr_t)headers - phdr->p_vaddr;
  uintptr_t slot = bias + tls->p_vaddr + STACK_GUARD_INDEX * sizeof(uintptr_t);
  int protection = -1;
  if (relro != NULL && segment_holds(relro, bias, slot)) {
    protection = PROT_READ;
  } else {
    for (size_t i = 0; i < count; i++) {
      if (headers[i].p_type == PT_LOAD &&
          segment_holds(&headers[i], bias, slot)) {
        protection = segment_protection(headers[i].p_flags);
      }
    }
  }
  if (protection < 0) {
    return 0;
  }
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) {
    return 0;
  }
  void* page = (void*)(slot & ~((uintptr_t)page_size - 1));
  if (protection & PROT_WRITE) {
    *(uintptr_t*)slot = guard;
    return 1;
  }
  if (mprotect(page, (size_t)page_size, protection | PROT_WRITE) != 0) {
    return 0;
  }
  *(uintptr_t*)slot = guard;
  return mprotect(page, (size_t)page_size, protection) == 0;
}

__attribute__((constructor)) static void seed_stack_guard(void) {
  uintptr_t guard = random_stack_guard();
  if (guard == 0 || !eclipse_bionic_tls_slots_at_thread_pointer() ||
      !write_initial_stack_guard(guard)) {
    return;
  }
  eclipse_bionic_tls_slots[STACK_GUARD_INDEX] = guard;
  process_stack_guard = guard;
}
