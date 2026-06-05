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
