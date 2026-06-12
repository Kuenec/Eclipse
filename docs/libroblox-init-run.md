# libroblox.so DT_INIT_ARRAY execution run — 2026-06-05

The **first time `libroblox.so`'s own code ran under Eclipse's loader.** This documents the
isolated init-execution harness (`src/loader/init_run.rs`, hidden subcommand
`eclipse __run-libroblox-init`) and the **real result** of running it on the dev host: how many of
the 3,427 `DT_INIT_ARRAY` constructors executed, the exact death point, and the diagnosed next
obstacle. Nothing here is faked — a crash is the expected, valuable diagnostic that pinpoints the
next engine-load work item.

> Reproduce (dev-host; skips cleanly with a clear error if the APK is absent):
> ```sh
> cargo build --release
> timeout 120 ./target/release/eclipse __run-libroblox-init > /tmp/eclipse-libroblox-init.log 2>&1
> echo "EXIT=$?"   # 134 = 128 + 6 (SIGABRT), reported by the harness's own signal handler
> ```

---

## 1. The harness (`src/loader/init_run.rs`)

A **diagnostic discovery step**, run from a hidden `eclipse __run-libroblox-init` subcommand on the
process **main thread** (a crash aborts this process, never a `#[test]` suite). It:

1. Reads `lib/x86_64/libroblox.so` from the APK (env `ECLIPSE_ROBLOX_APK` or the default dev-host
   path) and stages it to a temp file.
2. `Linker::load` in **root-only / env-provided-deps** mode (host fallback off) — maps the 3 PT_LOAD,
   applies all base relocations (`R_X86_64_RELATIVE`), honors `PT_GNU_RELRO`.
3. Builds the **full Eclipse scope** `[LoadedObjectProvider(libroblox)] + BionicEnv::with_host_baseline(true, true)`
   (the Eclipse-native tier prepended before the host baseline) and applies the symbol relocations
   (`relocate_object_symbols_partial`) — **work-list 0, all 584 imports resolved**.
4. Confirms the text segment is `PROT_EXEC` (segment `p_flags` **and** a `/proc/self/maps` cross-check).
5. Reads `DT_INIT_ARRAY` (count + each absolute fn pointer = the post-`RELATIVE` slot value) and
   **calls each constructor in order** as `extern "C" fn(int, char**, char**)` with
   `argc=1 / argv=["libroblox", NULL] / envp=[NULL]` (the bionic init-array convention; a `void(void)`
   ctor ignores the three SysV-register args, so this is ABI-safe either way).

**Diagnostics:** before each constructor it logs `init[i]/3427 @ base+0xOFFSET (abs 0x…)` (flushed). A
minimal `SA_SIGINFO` handler for `SIGSEGV`/`SIGABRT`/`SIGBUS`/`SIGILL`/`SIGFPE` logs (async-signal-safe)
`FATAL signal N in constructor init[i] ctor=0x… fault=0x…` and `_exit`s `128+signo`. The constructor
call is the one `unsafe` jump into mapped foreign code (confined here with a dated `// SAFETY:`; the
decode/map/reloc cores stay `#![forbid(unsafe_code)]`).

---

## 2. THE RUN RESULT (real, dev host, 2026-06-05)

```
mapped: objects=1 RELATIVE_applied=527208 RELR_applied=0 RELRO_applied=1
symbol relocs: applied_nonnull=623 applied_weak_zero=12 unresolved_strong=0 (work-list)
text PROT_EXEC: true (text seg vaddr=0x0 runtime=[…) flags=0x5 /proc-exec=true)
DT_INIT_ARRAY: vaddr=0x6a52240 size=27416 bytes -> 3427 constructors
calling 3427 constructors …
init[0/3427] @ base+0x283aa10   ← COMPLETED
init[1/3427] @ base+0x1bbca75   ← ABORTED
*** FATAL signal 6 in constructor init[1] ctor=0x…1bbca75 fault=0x… ***
EXIT=134   (128 + 6 = SIGABRT)
```

- **The static side is fully exercised at runtime:** 527,208 RELATIVE relocs applied, RELRO hardened,
  all 584 imports resolved (`unresolved_strong=0`), text confirmed `PROT_EXEC` by both the segment
  flags (`0x5` = R+X) and `/proc/self/maps`.
- **Constructor `init[0]` ran to completion** — `libroblox`'s own code executed for the first time
  under Eclipse's loader (a genuine milestone: the map/reloc/resolve pipeline produces runnable code).
- **Constructor `init[1]` aborted** with **SIGABRT** at `base+0x1bbca75`.

**Result: 1 of 3,427 constructors completed; constructor init[1] aborted via `abort()` (SIGABRT).**

---

## 3. Death-point analysis (gdb backtrace + objdump)

`gdb -batch -ex run -ex 'bt'` on the harness pins the abort:

```
#0 pthread_kill   (libc.so.6)
#1 raise          (libc.so.6)
#2 abort          (libc.so.6)
#3 0x…  in libroblox  →  offset 0x287ef15  (call abort@plt at 0x287ef10)
```

`init[1]`'s array entry `0x1bbca75` is a `jmp` thunk to the constructor body at `0x64cb336`, a
**function-local-static initializer** (the protobuf default-instance block, symbols
`__start_pb_defaults`/`__stop_pb_defaults`). It runs the libc++ static-init guard at `0x2863682`,
which is built on bionic-style primitives — `pthread_mutex_lock`, `syscall(SYS_gettid=186)`,
`pthread_getspecific`/`pthread_setspecific` (via `pthread_once` + a `pthread_key_create`d TLS key),
`pthread_mutex_unlock` — to track the **initializing thread** in thread-local storage.

The function containing the abort (`…+0x4878xx`) repeatedly calls `pthread_getspecific` /
`pthread_setspecific` / `pthread_once`, reads a per-thread structure out of that TLS slot, computes a
container capacity from it, and `abort()`s when an internal invariant fails (e.g. the
`test (cap-1), cap → jne abort` power-of-two check at `0x287eeb6`). With the **pthread/TLS slot
holding host-glibc state instead of the bionic-shaped state this code expects**, that per-thread
structure is wrong, the derived value violates the invariant, and the constructor traps to `abort()`.

---

## 4. Diagnosed next obstacle (root cause)

**The pthread + thread-local-storage surface is host-glibc, not bionic-ABI-correct.** This is exactly
the documented HONEST BASELINE caveat (`docs/bionic-env-worklist.md`): the 45 `pthread_*` imports +
the `pthread`-keyed TLS resolve to host glibc so the *relocation lands*, but glibc and bionic differ
in `pthread_t`, `pthread_key_t`, `pthread_once_t`, mutex internals, and **TLS slot semantics**. A
constructor that stores/reads per-thread state through `pthread_getspecific`/`pthread_setspecific`
(here, a libc++ static-init recursion guard) sees inconsistent state and aborts on an internal
invariant. The crash is **not** in our loader — the map/reloc/resolve/RELRO/PROT_EXEC pipeline is
correct (init[0] proves it); it is the **bionic-vs-glibc runtime ABI** at the first constructor that
exercises pthread-TLS.

### What is NOT the problem (ruled out by evidence)
- **Not the loader.** init[0] completed; relocs/RELRO/PROT_EXEC all verified.
- **Not unresolved imports.** `unresolved_strong=0` — the abort is a *called* routine's runtime
  behavior, not a null GOT slot.
- **Not the init-array convention.** The `(argc,argv,envp)` form is ABI-safe; init[0] ran fine.
- **Not our liblog/assert natives.** No `tracing` FATAL was emitted before the abort, so it is libc's
  `abort()` from libroblox's own invariant check, not `__assert2`/`__android_log_assert`.

### Recommended next step
Stand up an **Eclipse-owned, bionic-ABI-correct pthread + TLS shim** (replace the host-glibc baseline
for the `pthread_*` family and the `%fs`/TCB-backed `pthread_getspecific`/`pthread_setspecific`/
`pthread_key_create`/`pthread_once` path) and prepend it before the host tier in `BionicEnv`, then
re-run this harness. Smallest first cut: a correct `pthread_key_create`/`get`/`set`/`once` over an
Eclipse-owned per-thread key table (libroblox has **no PT_TLS**, so no static-TLS template is needed —
only the dynamic `pthread_*specific` key store + a real thread pointer). Re-running `eclipse
__run-libroblox-init` will then advance past init[1] to the next frontier.

---

## 5. Regression protection

The pure init-array pointer arithmetic and the async-signal-safe formatters are unit-tested
(`src/loader/init_run.rs` tests — GPU/VM-free, run everywhere):
`init_array_count(27_416) == 3_427`, the `*8` stride, and the bounded dec/hex writers. The runtime
behavior (the crash) is a **diagnostic**, not a test assertion — it must run on the main thread and is
expected to change as the bionic shim lands.

```sh
cargo test loader::init_run        # the pure-logic guards
```

---

## 6. RE-RUN after the bionic pthread+TLS shim landed (2026-06-05) — pthread RULED OUT as the cause

The recommended next step from §4 was built: an **Eclipse-owned, bionic-ABI-correct pthread + TLS
shim** (`src/loader/bionic_pthread.rs`), registered in `EclipseNativeProvider` (prepended before the
host tier), so the engine's `pthread_*` / `sem_*` / `gettid` / `syscall` imports now bind to
bionic-correct code operating on the **bionic memory layouts** (mutex 40 B, cond 48 B, rwlock 56 B,
sem 16 B, key/once 4 B), NOT host glibc. The shim's TLS keys use a real Rust per-thread table (no
`%fs` — libroblox has no `PT_TLS`); `pthread_once` is a 3-state futex once; the locks are
futex-backed. 11 GPU/VM-free unit tests prove the logic (2-thread mutual exclusion, once-exactly-once
under 8-thread contention, per-thread TLS isolation across 2 threads, recursive/errorcheck mutex
semantics, destructor-on-exit, bionic layout sizes).

### The re-run result (real, dev host, 2026-06-05)
```
init[0/3427] @ base+0x283aa10   ← COMPLETED
init[1/3427] @ base+0x1bbca75   ← ABORTED (SIGABRT) — SAME instruction as before
EXIT=134
```
**Still 1 of 3,427; the death point did NOT move.** This is a *valuable, honest* result: it
**disproves** §4's hypothesis that the pthread/TLS ABI mismatch caused the abort.

### Proof the shim is CORRECT (env-gated trace `ECLIPSE_PTHREAD_TRACE=1`, since removed)
The exact pthread sequence libroblox issued at init[1], immediately before the abort:
```
key_create -> key=0 (dtor)          # protobuf/libc++ TLS key
getspecific key=0 -> 0x0            # NULL first-use — CORRECT bionic behavior
key_create -> key=1 (dtor)
setspecific key=1 val=0x…f6c0 -> 0  # engine stores a per-thread block
getspecific key=1 -> 0x…f6c0        # reads back the SAME pointer — CORRECT round-trip
getspecific key=1 -> 0x…f6c0        # again correct
*** SIGABRT ***
```
The TLS values round-trip exactly; the shim behaves per the bionic contract. The abort is **after**
these correct calls.

### The REAL death point (gdb + objdump, disable-randomization)
The faulting return address `base+0x287ef15` is the instruction **after** `call abort@plt` at
`base+0x287ef10`, reached by `je 0x287ef10` at `0x287ee5c`: **"the allocator returned NULL"**. The
caller does `call 0x1bbce22` (libroblox's own **tcmalloc-/arena-style per-thread allocator**), then
`test %rax,%rax; je abort`. That allocator:
1. loads its TLS key from `.data` global `0x6aabac0` (statically `0xffffffff` = "uninit") → runs its
   key-init (`0x65089c9`: a sysconf/getauxval-driven heap-config block that reads ~a dozen globals),
   which is where `key_create -> key=0` came from;
2. `pthread_getspecific(key)` → NULL (fresh thread) → uses the static fallback arena `0x6aaa6c0`
   (statically **NULL**);
3. indexes a size-class free-list bucket off that arena, finds it empty, and tail-calls the central
   refill `0x1bbdcfa`, which **returns NULL** → the caller `abort()`s.

This is libroblox's **internal allocator failing its first refill during static init** — not a libc
ABI problem (the abort is identical with host glibc pthread and with the bionic shim, and it lands
*after* correct pthread/TLS calls). The `0x8(%rbx)` power-of-two "capacity" path at `0x287eeb6` that
§4 guessed at is a *different* basic block that this run's breakpoints proved is **never executed**
(the real path is the allocator-returns-NULL `je 0x287ef10`).

### Diagnosed next obstacle (root cause, revised)
**libroblox's own per-thread (tcmalloc/arena) allocator returns NULL on its first allocation during
init[1].** Its central refill (`0x1bbdcfa` → page-heap) fails. Most likely it depends on a
runtime/heap-config value the engine computes from the environment (sysconf page size, CPU count via
`getauxval`/`sched_getaffinity`, a `mallopt`/arena bootstrap, or a global that another constructor
sets) that is not yet satisfied under the bare init harness — OR the central page-heap's `mmap`/span
bootstrap is not succeeding in this context. The shim is **done and correct**; the frontier moved
from "pthread ABI" to "**libroblox internal allocator bootstrap**".

### Recommended next step (revised)
Instrument the engine's central allocator path (`0x1bbdcfa` and the heap-config `0x65089c9`):
trace the libc calls it makes (`mmap`/`munmap`/`mprotect`/`sysconf`/`getauxval`/`sched_getaffinity`)
under the harness and confirm which returns an unexpected value, then provide the bionic-correct
behavior for that one call (e.g. a bionic-shaped `getauxval`/`sysconf` answer, or ensuring `mmap` is
not being denied). The pthread shim stays — it is required and correct; it simply was not the
init[1] blocker.

### Regression protection (this step)
The shim logic is unit-tested (`cargo test loader::bionic_pthread` — 11 GPU/VM-free tests:
`mutex_two_threads_are_mutually_exclusive`, `once_runs_init_exactly_once_under_contention`,
`key_create_get_set_roundtrip_and_isolation_across_threads`, `bionic_object_word_counts_match_abi`,
…). The full-resolution invariant (`real_libroblox_eclipse_natives_fully_resolve_all_imports`) still
holds: work-list 88 → 0 (the 37 pthread natives were always host-resolvable, so they do not change
the *unresolved* work-list; they only displace glibc with the bionic-correct impls). The harness
re-run is the integration evidence and remains a diagnostic, not a `#[test]` assertion.

---

## 7. ROOT CAUSE FOUND + FIXED: the bionic-vs-glibc `sysconf` constant mismatch (2026-06-05)

§6's revised obstacle — "libroblox's own per-thread allocator returns NULL on its first refill during
init[1]" — is now **trace-proven to the exact bad value and fixed at the root cause.**

### The trace that pinned it (`ECLIPSE_TRACE_SYSQ=1`)
A new Eclipse-owned, bionic-ABI-correct system-query tier (`src/loader/bionic_sysconf.rs`) implements
`sysconf` / `getauxval` / `sched_getcpu` / `getpagesize` / `sysinfo`, **prepended before the host
glibc baseline** in `BionicEnv`, each logging `name(raw constant) -> return` under the env gate. The
allocator-bootstrap calls it makes at init[1], captured live:
```
[sysq] sysconf(name=39) -> 4096      # bionic _SC_PAGESIZE — now answered correctly
[sysq] sysinfo(...)      -> 0        # real RAM
[sysq] sysconf(name=39) -> 4096
[sysq] sysconf(name=39) -> 4096
[sysq] sched_getcpu()   -> 9 (raw=9) # valid per-CPU bucket index
[sysq] sched_getcpu()   -> 3 (raw=3)
```

### The bad value (measured on this dev host — `docs`/tests cross-check)
`libroblox.so` is compiled against the **bionic** headers, whose `sysconf(3)` `_SC_*` constant
**values differ from glibc's**. With the engine's `sysconf` import bound to the **host glibc**
baseline, a call the engine believes is `sysconf(_SC_PAGESIZE)` / `sysconf(_SC_NPROCESSORS_ONLN)`
actually passes the **bionic** numbers, which glibc mis-answers:

| query                  | bionic value | host glibc `sysconf(value)` returns |
|------------------------|-------------:|------------------------------------:|
| `_SC_PAGESIZE`         | `39`         | `1000`  (NOT 4096)                  |
| `_SC_NPROCESSORS_CONF` | `96`         | `200809` (a POSIX-version constant) |
| `_SC_NPROCESSORS_ONLN` | `97`         | **`-1`** (unknown to glibc)         |
| `_SC_PHYS_PAGES`       | `98`         | `1`                                 |
| `_SC_CLK_TCK`          | `6`          | **`-1`**                            |

The allocator sized its arena table / page-heap from `sysconf(39)` = page-size = **1000** (and, in the
arena-count path, `sysconf(97)` = **-1**), computed a garbage/zero capacity, so its first central
refill (`0x1bbdcfa`) returned NULL → the `je 0x287ef10`/`call abort@plt` at init[1]. NOT a libc/pthread
ABI bug (those were already ruled out, §6) — a **system-query constant mismatch**, exactly the prime
suspect.

### The fix (`src/loader/bionic_sysconf.rs`, prepended before host in `BionicEnv`)
A clean-room, bionic-ABI-correct `sysconf` that maps the **bionic** `_SC_*` numbers to the correct
runtime answers: bionic `39`/`40` ⇒ the real host page size (4096), bionic `97` ⇒ the online CPU count
(via `sched_getaffinity` bit-count, never 0/-1), bionic `96` ⇒ configured CPUs, bionic `6` ⇒ CLK_TCK,
bionic `98`/`99` ⇒ RAM pages — each delegating to glibc's **own** correct constant where one exists. An
unmapped bionic constant returns `-1` (POSIX indeterminate), never a forwarded-to-glibc wrong answer.
`getauxval`/`sched_getcpu`/`getpagesize`/`sysinfo` forward to the host (their `AT_*` / kernel ABIs are
identical for bionic and glibc) with tracing. `#[forbid(unsafe_code)]` stays on reloc/elf/resolve; the
new `unsafe` is confined to the syscall/FFI bodies (dated `// SAFETY:`).

### THE RE-RUN RESULT (real, dev host, 2026-06-05)
```
init[0/3427]   COMPLETED
init[1/3427]   COMPLETED   ← was SIGABRT; the allocator now bootstraps
…
init[426/3427] SIGSEGV (signal 11) @ base+0x2cf1ec7  fault=0x966da
EXIT=139
```
**Constructors completed: 1 → ~426** (the exact index drifts a few entries run-to-run: 414 / 426 —
the new failure is a near-null pointer deref that depends on prior-ctor ordering, not the allocator).
The `init[1]` allocator-bootstrap SIGABRT is **durably gone**; init now advances **400+ constructors**
deep. New death point = a **different subsystem**: `init[426]` (a protobuf default-instance ctor in
the `…DesignFoundations…` family) does `mov 0x3d685c9(%rip),%rbx  # 6a5a4a0` then `mov (%rbx),%rax` —
it loads a pointer from a static global `0x6a5a4a0` that is still near-NULL and dereferences it (fault
`~0x966da`, a small address). This is a **new frontier**, not the same bug.

### Regression protection (this step)
10 GPU/VM-free unit tests in `src/loader/bionic_sysconf.rs` (`cargo test loader::bionic_sysconf`):
`sysconf(_SC_PAGESIZE)` == the real page size (≥4096, NOT 1000), `sysconf(_SC_NPROCESSORS_ONLN)` > 0
(NOT -1) and ≤ CONF, `sysconf(_SC_CLK_TCK)` > 0, `sysconf(_SC_PHYS_PAGES)` > 0, an unmapped constant ⇒
-1 (never a wrong value), `getauxval(AT_PAGESZ)` == page size, `getpagesize()` == host, `sched_getcpu()`
≥ 0, the CPU-count helper never returns 0/-1, and the registration set is exactly the 5 expected names.
The `native_provider` count test asserts the 5 system-query natives are registered. The harness re-run
is the integration evidence (constructors 1 → ~426), a diagnostic, not a `#[test]` assertion.

### Diagnosed next obstacle (the new frontier)
A constructor reading an **uninitialized static global pointer** (`0x6a5a4a0`) that an earlier
constructor or a runtime-init data dependency should have populated — most likely a default-instance /
global-registry pointer the engine expects to be set up by a prior init step (or by a data relocation
not yet applied in this isolated harness). NEXT = instrument init[~426]'s read of `0x6a5a4a0` (which
prior constructor writes it, and whether it is a GOT/data slot the harness should have relocated) and
satisfy that dependency. The system-query tier stays — it is required and correct; it advanced init
from 1 to 400+ constructors.

---

## 8. ROOT CAUSE FOUND + FIXED: a WORKER THREAD + mixed pthread_t ABI — init now runs 3427/3427 (2026-06-05)

§7's "next frontier" hypothesis (a ctor reading uninitialized global `0x6a5a4a0`) was **wrong**, and
gdb proved it. The real init[~414] crash was on a **spawned worker thread**, not the init-array.

### The trace + gdb evidence (ASLR off, `ECLIPSE_TRACE_THREADS=1`)
- **libroblox spawns ONE thread during init** (its job system): `pthread_create` fires once; the
  child is later named **"RBX Worker A"** via `pthread_setname_np`.
- The SIGSEGV is on **Thread 2 (the worker)**, in **host glibc `pthread_setname_np`** at
  `mov 0x2d0(%r12),%r8d`, with `%r12` = a small value == the worker's **kernel TID** (e.g. `0x97b80`
  = 621440 = the LWP). The faulting address = `TID + 0x2d0`, which **drifts run-to-run because the
  TID differs** — that is the "414 vs 426 drift", not ctor ordering.
- The worker's libroblox entry does exactly: `call pthread_self@plt; mov %rax,%rdi; mov name,%rsi;
  call pthread_setname_np@plt` — i.e. `pthread_setname_np(pthread_self(), name)`.

### Root cause: a MIXED pthread_t ABI (Eclipse identity vs host-glibc struct)
`pthread_self`/`pthread_equal`/`pthread_gettid_np` resolved to the **Eclipse shim** (which returns the
**kernel TID** as the opaque bionic `pthread_t`), but `pthread_create`/`pthread_setname_np` (and the
whole thread-lifecycle family: join/detach/kill/getschedparam/setschedparam/getattr_np/attr_*) fell
through to **host glibc**, whose `pthread_t` is a `struct pthread*`. The worker passed the Eclipse
`pthread_self()` (a TID) to glibc `pthread_setname_np`, which dereferenced it as a struct → fault. The
old §3 "init[1] aborted in a pthread-TLS guard" and §7's "global `0x6a5a4a0`" were the signal handler
mis-attributing a **worker** crash to whatever **main-thread** init index was current at the time
(`CURRENT_INIT_INDEX`), and a coincidental disassembly of a *different* ctor at that index.

### The fix (`src/loader/bionic_pthread.rs`) — own the WHOLE thread lifecycle, TID-based
14 new Eclipse-owned natives so `pthread_t` is **consistently the kernel TID** everywhere
(`PTHREAD_NATIVE_COUNT` 37 → 51): `pthread_create` (spawns a real OS thread via a private glibc
handle — NEVER exposed — running an Eclipse trampoline that publishes its TID to the parent and runs
libroblox's `start(arg)`; honors the bionic attr's detach-state + stack-size), `pthread_join`/
`pthread_detach` (TID→handle registry), `pthread_setname_np` (TID-based: `prctl(PR_SET_NAME)` for self,
`/proc/self/task/<tid>/comm` for others — truncated to `TASK_COMM_LEN-1`), `pthread_kill`
(`tgkill(getpid(),tid,sig)`), `pthread_getattr_np`, `pthread_get/setschedparam` (TID-based `sched_*`),
and `pthread_attr_init/destroy/setdetachstate/setstacksize/setschedparam/getstack`. `pthread_sigmask`
(sigset-only, no `pthread_t`) + `__cxa_thread_atexit_impl` stay on the host baseline (ABI-identical).

### Two follow-on harness-teardown faults (also gdb-proven, fixed in `src/loader/init_run.rs`)
With the worker no longer crashing, the init-array ran to **3427/3427**, exposing two **process-exit**
artifacts (NOT init failures — the harness's job was already done):
1. **Unmap-under-live-worker:** the success path `drop(set)` `munmap`ped libroblox while "RBX Worker A"
   was still executing its text → the worker faulted on freed text (gdb: its PC landed in the
   now-unmapped `[base,base+size)`).
2. **Exit-time finalizer + `__sF` layout:** returning through `main` let glibc `exit()` run libroblox's
   C++ static destructors / `atexit` finalizers, one of which `fflush`es an engine `FILE*` taken as
   `&__sF[i]`; Eclipse's `__sF` was (at the time) a host-stdio **pointer** table, so the slot address
   derefed as a bad glibc `FILE*` → fault on the **main thread** at exit. *(2026-06-12: this exact
   mechanism later killed crashpad's in-handler logging — core 782252 — and was root-cause-fixed:
   `__sF` is now a bionic-shaped 3×152-byte backing with translating stdio natives; see
   `native_provider.rs`. The `_exit(0)` below stays for reason 1.)*
   Fix: once **all** constructors complete, the diagnostic's defined job is done, so it `_exit(0)`s
   **immediately** (async-signal-safe; no unmap, no destructors/finalizers, no teardown of live
   workers; the OS reclaims everything). Init — not shutdown — is this harness's scope.

### THE RE-RUN RESULT (real, dev host, 2026-06-05) — DETERMINISTIC
```
init[0/3427]      COMPLETED
…
init[3426/3427]   COMPLETED
ALL 3427/3427 constructors completed without a crash
EXIT=0
```
**Constructors completed: ~414 → 3427/3427 (ALL), EXIT=0, across 9/9 runs.** The run-to-run drift is
**gone** (it was the worker's TID-derived fault address). libroblox's entire `DT_INIT_ARRAY` now
executes under Eclipse's loader; the engine even spawns + names a background worker ("RBX Worker A")
that keeps running. The init phase is **complete**; the next frontier is **post-init engine bring-up**
(driving the worker/job system + the engine's real entry, the `JNI_OnLoad`/Activity path), not init.

### Regression protection (this step)
5 new GPU/VM-free unit tests in `src/loader/bionic_pthread.rs` (16 total;
`cargo test loader::bionic_pthread`): `create_runs_entry_on_real_thread_and_join_returns_its_result`
(the entry runs on a DIFFERENT OS thread; `pthread_self()` inside == the returned `pthread_t`; join
returns the entry's result; a re-join → ESRCH), `create_detached_is_not_joinable_and_runs`,
`setname_np_on_self_succeeds_and_is_truncated`, `attr_records_detachstate_and_stacksize`,
`kill_signal_zero_probes_a_live_thread`. The `native_provider` registration-count test tracks
`PTHREAD_NATIVE_COUNT` (now 51). The harness re-run (3427/3427, EXIT=0) is the integration evidence — a
diagnostic, not a `#[test]` assertion.

---

## 9. THE RUST LOADER, INTEGRATED INTO THE LIVE `eclipse run` — libroblox loads + inits + JNI_OnLoad in the running ART runtime (2026-06-05)

The isolated harness (§1–§8) proved the loader; this step **wires it into the live `eclipse run` path**
so the real engine loads, initializes, and registers its JNI **against Eclipse's actual booted ART VM** —
then runs the real Roblox APK end-to-end. Nothing here is faked; the REAL run result + the new post-load
frontier are reported exactly.

> Reproduce (dev host):
> ```sh
> cargo build --release
> timeout 180 ./target/release/eclipse run ~/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk \
>   > /tmp/eclipse-roblox-run.log 2>&1; echo "EXIT=$?"
> ```

### What was wired (`src/loader/engine.rs` + `src/main.rs`)
A new `src/loader/engine.rs` factors the proven load pipeline into a **persistent** form (no `_exit`, no
`munmap` — the image stays mapped for the process lifetime so the engine's background workers keep running):

- `load_libroblox(apk_path, log) -> LoadedEngine` — reads `lib/x86_64/libroblox.so` from the APK, maps the
  3 PT_LOAD, applies the 527,208 `R_X86_64_RELATIVE`, honors `PT_GNU_RELRO`, builds the FULL Eclipse scope
  (`[LoadedObjectProvider(libroblox)] + BionicEnv::with_host_baseline(true, true)`, the Eclipse-native tier
  prepended) and applies the symbol relocations (**work-list 0 — all 584 imports resolve**), confirms text
  `PROT_EXEC`, and locates `DT_INIT_ARRAY`. Returns the live `LoadedEngine` (owns the `LoadedImageSet`).
- `LoadedEngine::run_init_array(log)` — calls all **3,427** `DT_INIT_ARRAY` constructors in order (shares the
  init-array arithmetic with `init_run.rs`). Unlike the diagnostic harness it installs **no** crash handler
  and does **not** `_exit` (the run is proven deterministic, §8); the engine spawns its background workers
  here, so `LoadedEngine` MUST stay alive afterward.
- `call_jni_onload(engine, java_vm, log) -> jint` — looks up the engine's exported `JNI_OnLoad` (GLOBAL FUNC
  at vaddr `0x1f3d5b1`) via the same `LoadedObjectProvider` the scope uses (`base + st_value`), then calls
  `JNI_OnLoad(JavaVM*, void*)` with Eclipse's REAL ART `JavaVM` (`runtime::Vm::as_raw()`), returning the JNI
  version it reports.

`src/main.rs::run_apk` calls `load_engine_via_rust_loader(&mut apk, apk_path, &vm)` on the **main thread**,
with the VM alive + JNI-attached, **after** the bionic library-path whitelist and **before** driving the
framework lifecycle (where Roblox's Java would call `System.loadLibrary("roblox")`). It is gated on the APK
actually containing `lib/x86_64/libroblox.so` (a cheap file-name scan via `Apk::native_abis`), so the
pure-Java demo APKs **skip** the loader and keep the existing framework-only path — **no regression**
(`demo_app` still reaches `ActivityResumed` + opens the window). The two `unsafe` foreign jumps (the
constructors + the `JNI_OnLoad` call) are confined with dated `// SAFETY:`; `reloc.rs`/`elf.rs`/`resolve.rs`
stay `#![forbid(unsafe_code)]`. ZERO new crates.

### THE REAL ROBLOX RUN RESULT (dev host, 2026-06-05, deterministic across 2/2 runs)
```
# Loading the native engine via Eclipse's Rust loader (NOT the apkenv linker)…
engine-load: libroblox.so = 111823960 bytes
engine-load: mapped objects=1 RELATIVE_applied=527208 RELRO_applied=1
engine-load: symbol relocs applied_nonnull=623 weak_zero=12 unresolved_strong=0
engine-load: text PROT_EXEC ✓; DT_INIT_ARRAY vaddr=0x6a52240 -> 3427 constructors
engine-load: 3427/3427 constructors completed ✓
engine-load: calling JNI_OnLoad @ base+0x1f3d5b1 (abs 0x…423e05b1) with the ART JavaVM…
  liblog WARN tag="JNIMain": LoggingProtocol::getProcessTimestamp() threw an exception …
  liblog WARN tag="JNIMain": DeviceStaticParams is null.
engine-load: JNI_OnLoad returned 0x10006 (JNI_VERSION_1_6)
# Driving the framework lifecycle (JNI; steps 1–7) …
  roblox.config: setBaseUrl() null => www.roblox.com    ← Roblox's OWN Java onCreate running
  rbx.baseurl: Incoming base url: www.roblox.com
  creating androidx.startup.InitializationProvider
  W bionic_linker: `…/libzstd-jni-1.5.7-6.so` is not a prelinked library
  W bionic_linker: `libm.so` is not a prelinked library
  E bionic_linker: unknown reloc type 18 @ 0x… (5)         ← R_X86_64_TPOFF64, the apkenv wall
  E bionic_linker: failed to link libm.so
  Fatal signal 11 (SIGSEGV) … fault addr 0x18  Thread: "AppStartupTaskM"
EXIT=139
```

**Milestones reached (all REAL, in the live runtime):**
1. **The engine-load interception fired** — libroblox routed to Eclipse's Rust loader, NOT the apkenv linker.
2. **libroblox loaded + relocated + RELRO-hardened** in the live process: 527,208 RELATIVE + 623 symbol relocs,
   `unresolved_strong=0`.
3. **All 3,427 `DT_INIT_ARRAY` constructors ran** in the live runtime (the engine even emits its own liblog
   warnings through Eclipse's liblog natives).
4. **`JNI_OnLoad` ran against the REAL ART `JavaVM` and returned `JNI_VERSION_1_6`** — the engine's native
   methods are now registered against Eclipse's ART. (During it the engine's `JNIMain` code executed.)
5. **The framework lifecycle then drove Roblox's own `Application.onCreate`** — real Roblox Java ran
   (`roblox.config setBaseUrl → www.roblox.com`, `rbx.baseurl`).

### The new post-load frontier (root cause, evidence-based)
The crash is **NOT** in Eclipse's loader or in libroblox — it is the **apkenv shim linker hitting the exact
original modern-reloc wall (`unknown reloc type 18` = `R_X86_64_TPOFF64`) on a DIFFERENT library**. During
`Application.onCreate`, `androidx.startup.InitializationProvider` triggers `System.loadLibrary("zstd-jni")`,
which goes through ART's `Runtime.nativeLoad` → the **apkenv** linker (Eclipse only intercepts `libroblox`).
`libzstd-jni` has `NEEDED libm.so`; the apkenv linker parses the provisioned host `libm.so.6`, hits its
`R_X86_64_TPOFF64`, and **fails to link libm.so** → `libzstd-jni`'s load returns broken → a NULL deref on the
`AppStartupTaskM` thread (`rax=0`, fault addr `0x18`) → SIGSEGV.

This is the documented engine-load wall, now reached by the engine's **sibling JNI libs** rather than by
libroblox itself. The fix is the same routing extended to those libs: **route `System.loadLibrary` for the
app's other native libs (libzstd-jni and its transitive `libm`/bionic deps) through Eclipse's Rust loader
too** — either by intercepting ART's `Runtime.nativeLoad` JNI native, or by pre-loading the app's full
`lib/x86_64/*.so` set through `link.rs` (with a proper dependency graph + a bionic `libm` provider) before the
lifecycle, the same way libroblox is pre-loaded now.

### Recommended next step
Extend the interception from "just libroblox" to "the app's native-lib set": pre-load `libzstd-jni` (and any
other app `.so` ART's `nativeLoad` would hand to the apkenv linker) through `link.rs`, supplying a
bionic-correct `libm`/`libc` provider (Eclipse already owns the relocation cores + the bionic-native tiers),
so the modern relocs are applied by Eclipse's `reloc.rs` instead of aborting in apkenv. Then re-run to advance
past the `androidx.startup` task into the rest of `onCreate`.

### Regression protection (this step)
4 GPU/VM-free unit tests in `src/loader/engine.rs` (`cargo test loader::engine`): the `JNI_OnLoad` symbol name
is exactly `"JNI_OnLoad"` (a typo would silently make the lookup return `None`), the engine entry path is
`lib/x86_64/libroblox.so`, `describe_jni_version` labels the common JNI constants (and reports a negative
return as an error sentinel, not a version), and `JNI_VERSION_1_6 == 0x00010006` (the ART default; a jni-sys
bump that changed it fails here, not silently at runtime). The full-resolution + map invariants are the
existing gated `link.rs` real-libroblox tests (work-list 0, 527,208 RELATIVE). The live `eclipse run` is the
integration evidence; `demo_app` reaching `ActivityResumed` (engine loader skipped) is the no-regression check.

## 10. App-JNI-lib PRE-LOAD generalized — `libzstd-jni` relocates cleanly via the Rust loader; the boot does NOT yet advance (2026-06-05)

### What was generalized
`engine::load_libroblox` was factored into a reusable, lib-agnostic pipeline:

- `engine::map_resolve_app_lib(apk_path, filename, search_dir, log)` — the shared map + base-relocate +
  full-resolve core. Reads `lib/x86_64/<filename>` via `src/apk`, root-only loads it through `link.rs`
  (`with_host_fallback(false)`, `with_tolerate_missing_deps(true)`) with the FULL `BionicEnv::with_host_baseline`
  scope applied to the symbol relocations. A `DT_INIT_ARRAY` is now **optional** (most app JNI libs ship none —
  lazy-native). `search_dir`, when set, is the extracted-libs dir added to the linker's `DT_NEEDED` search path so a
  sibling **app-lib** dependency (e.g. `libeigen_lapack.so` `NEEDED libeigen_blas.so`) loads from disk through this
  same loader; bionic `DT_NEEDED` (`libc/m/dl/...`) still resolve via the scope (never from the host).
- `engine::load_libroblox` — now a thin wrapper over the core (no search dir) that additionally asserts the engine
  has a `DT_INIT_ARRAY` (its 3,427 ctors are not optional).
- `engine::load_app_native_lib(apk_path, filename, java_vm, search_dir, log) -> Option<PreloadedLib>` — the generic
  full pipeline: dedup by soname → map/resolve via the core → run `DT_INIT_ARRAY` **iff present** → call `JNI_OnLoad`
  **iff exported** → record the soname in a process-global dedup registry. Returns `None` when the lib was already
  loaded (deduped), else the kept-alive `PreloadedLib`.
- Registry: `static LOADED_SONAMES: Mutex<Option<HashSet<String>>>` — **dedup only** (stores sonames, which are
  `Send`); the actual mappings are kept alive for the process lifetime by the caller binding each `PreloadedLib`.
- `src/apk`: `Apk::native_lib_filenames(abi)` lists the flat `.so` file names under `lib/<abi>/` (sorted).
- `src/main.rs::run_apk`: `load_engine_via_rust_loader` → `preload_app_native_libs` — pre-loads libroblox FIRST
  (mandatory) then every other `lib/x86_64/*.so` TOLERANT of per-lib failure (warn + continue), before the lifecycle.
  Pure-Java APKs (no `lib/x86_64/libroblox.so`) skip it (`demo_app` still reaches `ActivityResumed`).

### THE REAL ROBLOX RUN RESULT (dev host, 2026-06-05, `/tmp/eclipse-roblox-run3.log`, EXIT=139)
Six x86_64 JNI libs pre-loaded cleanly via Eclipse's Rust loader (work-list 0 each):
`libroblox.so` (3427 ctors + `JNI_OnLoad → 1.6`), `libdatastore_shared_counter`, `libeigen_blas`,
`libeigen_lapack` (loaded its sibling `libeigen_blas` from the search dir), `libyuv_shared`, and crucially
**`libzstd-jni-1.5.7-6.so`** (23 RELATIVE + 432 symbol relocs, `unresolved_strong=0`, lazy-native — no ctors,
no `JNI_OnLoad`). Four others (`libbacktrace-native`, `libimage_processing_util_jni`, `librenderscript-toolkit`,
`libsurface_util_jni`, `libtrampoline`) have a few unresolved-strong NDK imports (`libjnigraphics`/etc.) and were
TOLERATED (warned + skipped) — none is on the immediate `onCreate` path.

`onCreate` runs as before (`roblox.config setBaseUrl → www.roblox.com`, `setChannel()`,
`creating androidx.startup.InitializationProvider`), then SIGSEGVs at the **byte-for-byte identical** prior point.

### Root cause (evidence) — the pre-load is inert without the `nativeLoad` consult
The crash is unchanged from the pre-change run (`run2`): during `androidx.startup`, `InitializationProvider` calls
`System.loadLibrary("zstd-jni-1.5.7-6")`; ART's `Runtime.nativeLoad` STILL hands the load to the **apkenv** linker
(log: `` `…/libzstd-jni-1.5.7-6.so` is not a prelinked library ``), which follows `NEEDED libm.so`, hits
`bionic_translation linker.c:2128 unknown reloc type 18` (`R_X86_64_TPOFF64`), `failed to link libm.so`, and the
broken load NULL-derefs (fault `0x18`, `rax=0`) on the `AppStartupTaskM` thread → SIGSEGV. Registers match `run2`
almost exactly.

So **ART's `System.loadLibrary` does not consult Eclipse's pre-load/loaded-lib registry.** Pre-loading a lib into
our address space makes its symbols live for *us*, but `loadLibrary` independently re-loads it through apkenv. The
prior belief that "the framework consults the registry, which is why libroblox skipped apkenv" is **not borne out**:
libroblox skipped apkenv only because Roblox never issues `System.loadLibrary("roblox")` before this crash (its
natives are already registered by our `JNI_OnLoad`). `androidx.startup` *does* issue `loadLibrary` for zstd-jni, and
that call bypasses the registry.

### Recommended next step (safeguard-gated — main loop only)
The pre-load half is done and proven (zstd-jni relocates with work-list 0). The missing half is the **registry
CONSULT**: make ART's `Runtime.nativeLoad`/`System.loadLibrary` short-circuit to Eclipse's loaded-lib registry so a
pre-loaded soname is reported already-loaded instead of being re-loaded via apkenv. That is the `nativeLoad`
interception, which lies **inside the cyber-safeguard boundary** — it must be done in the main loop, not by a
workflow subagent, and only after explicit re-authorization. The pre-load PATTERN added here is safe; the
`nativeLoad`/`loadLibrary` interception is the forbidden region for automated agents.

### Regression protection (this step)
3 GPU/VM-free unit tests: `engine::soname_registry_dedups_by_soname` (the registry inserts a soname once and dedups
the second insert — the property the pre-load loop relies on), `engine::preloaded_lib_fields_express_the_optional_paths`
(the lazy-native shape `(0 ctors, no JNI_OnLoad)` vs the engine shape `(n ctors, Some(version))` — the optional
init/`JNI_OnLoad` paths), and `apk::native_lib_filenames_lists_flat_so_files_for_the_abi_sorted` (flat `.so`
enumeration for the ABI, sorted, excluding nested/wrong-ABI/non-`.so`, and empty for a pure-Java APK — the gate that
skips the pre-load loop). The live `eclipse run` is the integration evidence; `demo_app` reaching `ActivityResumed`
with the loader skipped is the no-regression check.

## 11. ROOT CAUSE FOUND + FIXED: the PROVISIONED `libm.so` was glibc `libm.so.6` (modern relocs apkenv can't apply) — an apkenv-LOADABLE libm shim is now provisioned; the TPOFF64 wall is durably gone (2026-06-05)

§9/§10's wall — `androidx.startup` → `System.loadLibrary("zstd-jni")` → apkenv linker → `NEEDED libm.so` →
`unknown reloc type 18` (`R_X86_64_TPOFF64`) → `failed to link libm.so` → SIGSEGV — is now **fixed at the root cause,
which was a PROVISIONING bug**, not a missing capability.

### The root cause (measured with `readelf`, 2026-06-05)
Eclipse's `runtime::provision_bionic_sonames` symlinked the app's bare `libm.so` to the **host glibc `libm.so.6`**.
The apkenv / `bionic_translation` shim linker resolves an app lib's `NEEDED libm.so` by mmap-parsing whatever file is
named `libm.so` on its search path — so it tried to relocate glibc's `libm.so.6`, which carries relocs the older
apkenv linker cannot apply:

```
$ readelf -rW $(cc -print-file-name=libm.so.6) | awk '/R_X86_64/{print $3}' | sort | uniq -c
     32 R_X86_64_GLOB_DAT
      1 R_X86_64_TPOFF64        # <- apkenv "unknown reloc type 18"
$ readelf -SW libm.so.6 | grep relr
  [ 9] .relr.dyn  RELR ...      # <- packed relocs apkenv also cannot apply
$ readelf -d libm.so.6 | grep NEEDED
  NEEDED  libc.so.6
  NEEDED  ld-linux-x86-64.so.2  # <- a glibc-internal dep
```

Note: zstd-jni / eigen_blas / eigen_lapack each carry `NEEDED libm.so` but import **ZERO** math from it (their UND
symbols are all `@LIBC`). Only `libroblox` imports real math (49 symbols, measured via `readelf --dyn-syms` on the 4
`NEEDED libm.so` libs) — and libroblox is loaded by Eclipse's OWN Rust loader (its math binds via `BionicEnv`), NOT by
apkenv. So for the apkenv-loaded libs, `libm.so` must merely be **loadable**; for durable correctness the shim still
provides the full, correct math surface (a wrong `sin` would corrupt the engine).

### The fix: an apkenv-loadable, correct-math libm shim (benign provisioning, no linker-source change)
A new `crates/libm-shim` sub-crate — a `#![no_std]` cdylib (`panic="abort"`, own `#[panic_handler]`) re-exporting the
**pure-Rust `libm` crate** (rust-lang/compiler-builtins; Context7-confirmed API) under the C libm symbol names. 56
symbols exported (the 49 the libs need + the f32 two-arg variants); pointer-out functions (`frexp`/`modf`/`sincos`/
`remquof`) wrap the crate's tuple returns into the C out-pointer ABI; `lround`/`llround`/`nan` (not in the crate) sit
on `libm::round`. `build.rs::build_libm_shim` builds it via `$CARGO build` into `OUT_DIR/libm-shim-target` (no
recursion into the outer target dir → no lock contention; portable, no hardcoded paths) and bakes the `.so` path via
`cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO`; `runtime::provision_eclipse_libm` **copies** it to `<app-lib>/libm.so`. The
host-symlink table `BIONIC_BARE_SONAMES` is now empty (no host glibc lib is wrongly handed to apkenv as a `NEEDED`).

The produced `.so` is apkenv-safe — only the reloc types zstd-jni itself uses (which apkenv provably handles):

```
$ readelf -rW libeclipse_libm_shim.so | awk '/R_X86_64/{print $3}' | sort | uniq -c
      4 R_X86_64_GLOB_DAT
      6 R_X86_64_RELATIVE
# 0 R_X86_64_TPOFF64, no .relr.dyn section, no NEEDED, no PT_TLS
```

### THE REAL ROBLOX RUN RESULT (dev host, 2026-06-05, `/tmp/eclipse-roblox-run4.log`, EXIT=139)
```
# Provisioning bionic sonames (libm.so → Eclipse apkenv-loadable shim) …  ✓
…
creating androidx.startup.InitializationProvider
W bionic_linker: `…/libzstd-jni-1.5.7-6.so` is not a prelinked library   ← apkenv proceeds (was: ERROR)
W bionic_linker: `libm.so` is not a prelinked library                    ← apkenv LOADS our shim libm
Fatal signal 11 (SIGSEGV) … Thread "AppStartupTaskM"
```
**`unknown reloc type 18` + `failed to link libm.so`: 0× (was 2× in run2/run3).** The TPOFF64 wall is durably gone;
apkenv now parses BOTH zstd-jni AND `libm.so` without aborting.

### The new frontier (gdb-proven) — INSIDE the apkenv linker (cyber-safeguard region)
```
#0 apkenv_find_library    (libdl_bio.so.0)   ← SIGSEGV (rax=0, fault 0x18); rdi="\001", rsi=0 (NOT a named lookup)
#1 apkenv_find_library    (libdl_bio.so.0)   ← recursing the dependency graph
#2 bionic_dlopen          (libdl_bio.so.0)
#3 art::JavaVMExt::LoadNativeLibrary  (libart.so)
#4 JVM_NativeLoad         (libopenjdkjvm.so)
```
A NULL deref **inside the apkenv linker's own dependency-graph walk** while ART's `System.loadLibrary("zstd-jni")`
re-loads zstd-jni through apkenv (the Eclipse-pre-loaded copy stays inert without the registry-consult — §10). The
`rdi`/`rsi` at the crash prove it is NOT a missing/glibc-provisioned `NEEDED` lib (no benign provisioning gap remains);
it is an apkenv-internal fault. **The durable fix is the Rust-loader `System.loadLibrary`/`Runtime.nativeLoad`
registry-consult so a pre-loaded soname short-circuits the apkenv `apkenv_find_library` walk entirely — that is INSIDE
the cyber-safeguard boundary (the apkenv/bionic_translation linker + `nativeLoad` region) and is MAIN-LOOP ONLY,
FORBIDDEN for subagents.**

### Regression protection (this step)
2 GPU/VM-free unit tests in `src/runtime.rs` (`cargo test runtime::tests::eclipse_libm`):
`eclipse_libm_shim_is_apkenv_loadable_and_provisions_libm_so` (decodes the built shim via Eclipse's own `elf.rs`,
asserts NO `R_X86_64_TPOFF64` + NO RELR, then provisions a `<dir>/libm.so` copy idempotently) and
`eclipse_libm_shim_math_values_are_correct` (dlopens the shim, checks `sin`/`cos`/`pow`/`log`/`exp`/`fmod`/`atan2`/
`sinf`/`powf`/`frexp` vs known values). `host_symlinked_sonames_…` now asserts `libm.so` is NEVER host-symlinked;
`build.rs` adds a `readelf` build-time TPOFF64 guard; `crates/libm-shim` has its own clippy/fmt-clean gate. `demo_app`
still reaches `ActivityResumed` (provisioning runs cleanly on a pure-Java APK; engine loader skipped) — no regression.
The live `eclipse run` is the integration evidence (libm wall gone, run4).
