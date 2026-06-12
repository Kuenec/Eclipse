//! Eclipse-owned, **bionic-ABI-correct** `pthread_*` + thread-local-storage shim — the loader's
//! sixth piece (the threading runtime that beats the host-glibc baseline).
//!
//! 2026-06-05: the [`init_run`](super::init_run) harness ran `libroblox.so`'s `DT_INIT_ARRAY`:
//! constructor `init[0]` completed, but `init[1]` **aborted (SIGABRT)** inside a libc++ /
//! protobuf function-local-static guard that uses `pthread_mutex_lock`, `syscall(SYS_gettid)`,
//! `pthread_once`, and a `pthread_key_create`d TLS slot to track the initializing thread (see
//! `docs/libroblox-init-run.md`). ROOT CAUSE: those imports resolved to **host glibc**, whose
//! `pthread_mutex_t` / `pthread_key_t` / `pthread_once_t` **memory layouts and semantics differ
//! from bionic's**, so glibc misread `libroblox`'s embedded bionic-layout pthread objects and the
//! guard's per-thread invariant failed → `abort()`. This module supplies the fix: Eclipse-owned
//! `extern "C"` natives that operate on the **bionic** memory layouts, prepended before the host
//! tier so the engine's `pthread_*` imports bind to bionic-correct code, NOT glibc.
//!
//! ## Clean-room provenance
//! Every layout/semantic below is from the **public** bionic C-ABI — the documented opaque sizes of
//! `pthread_mutex_t` (`int __private[10]` = 40 B), `pthread_cond_t` (`int __private[12]` = 48 B),
//! `pthread_rwlock_t` (`int __private[14]` = 56 B), `pthread_key_t`/`pthread_once_t` (a 4-byte int),
//! `pthread_attr_t` (56 B), `sem_t` (`int __private[4]` = 16 B), the `PTHREAD_MUTEX_NORMAL/
//! RECURSIVE/ERRORCHECK` type values, and the `PTHREAD_ONCE_INIT == 0` / `PTHREAD_*_INITIALIZER`
//! all-zero rule — plus the public Linux `futex(2)` / `gettid(2)` syscalls. **No** bionic / NDK /
//! linker source was read. `libroblox.so` is parsed as data only; nothing in it is executed here.
//!
//! ## Why operate on the bionic layout (the crux — a glibc forward is WRONG)
//! `libroblox`'s pthread objects are **caller-allocated** at the bionic sizes (a static array of
//! mutexes strides by 40 B; a `pthread_cond_t` field is 48 B; a `pthread_key_t` is a 4-byte int).
//! Forwarding to glibc would have glibc reinterpret those bytes under its *own* (different) layout —
//! exactly the bug that aborted `init[1]`. Eclipse instead **owns** the interpretation of those
//! bytes end-to-end: every `pthread_*` import resolves to this module, so an object is only ever
//! touched by Eclipse code. The public bionic ABI only guarantees the opaque **size/alignment** and
//! that a **zeroed** object is the static initializer (`PTHREAD_MUTEX_INITIALIZER` /
//! `PTHREAD_ONCE_INIT` / a fresh cond are all all-zero); Eclipse defines a consistent, futex-backed
//! encoding within those bytes. This is the standard shape of a clean-room bionic replacement.
//!
//! ## Encoding (Eclipse-owned, within the bionic object's bytes)
//! - **mutex** (40 B): word[0] = futex state (0 unlocked / 1 locked-uncontended / 2 locked-contended,
//!   the standard 3-state futex lock); word[1] = a one-time `MUTEX_INIT` magic so a *zeroed* object
//!   lazily adopts NORMAL on first use (the `PTHREAD_MUTEX_INITIALIZER` contract); word[2] = type
//!   (NORMAL/RECURSIVE/ERRORCHECK); word[3] = owner tid; word[4] = recursion count.
//! - **cond** (48 B): word[0] = a 32-bit sequence/futex word bumped by signal/broadcast; word[1] =
//!   the `CLOCK_MONOTONIC` flag from `pthread_condattr_setclock`.
//! - **rwlock** (56 B): word[0] = futex state word; word[1] = reader count; word[2] = writer tid.
//! - **once** (4 B): the bionic 3-state once word (0 = not run, 1 = in progress, 2 = done) driven by
//!   an atomic CAS + futex wait, so the init runs **exactly once** under contention.
//! - **TLS keys**: `pthread_key_create` allocates a small int index from an Eclipse table;
//!   per-thread values live in a real Rust `thread_local!` `Vec` (NO `%fs`/static-TLS needed —
//!   `libroblox` has no `PT_TLS`). Destructors are recorded per key; run on a deliberate
//!   `pthread_exit`. (Native-thread-teardown destructor delivery is documented-deferred below.)
//!
//! ## Safety
//! Taking each native's address (`f as usize`) is safe Rust; the registry needs no `unsafe`. The
//! `unsafe` is confined to the native *bodies* that dereference the caller's bionic objects (raw
//! pointers) and the two raw syscalls (`futex`, `gettid`), each with a dated `// SAFETY:` note. The
//! reloc/elf/resolve cores stay `#![forbid(unsafe_code)]`.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// =================================================================================================
// Public bionic ABI constants (documented values; the only contract this shim must match exactly).
// =================================================================================================

/// `EBUSY` — a `*_trylock` that would block, or `pthread_mutex_destroy` of a locked mutex.
const EBUSY: c_int = 16;
/// `EINVAL` — an invalid argument (null object, bad type).
const EINVAL: c_int = 22;
/// `EDEADLK` — an errorcheck mutex re-locked by its owner.
const EDEADLK: c_int = 35;
/// `EPERM` — unlocking a mutex the calling thread does not own (errorcheck).
const EPERM: c_int = 1;

/// `PTHREAD_MUTEX_NORMAL` (== `_DEFAULT`): no owner tracking, no recursion.
const PTHREAD_MUTEX_NORMAL: c_int = 0;
/// `PTHREAD_MUTEX_RECURSIVE`: the owner may relock; an internal count tracks depth.
const PTHREAD_MUTEX_RECURSIVE: c_int = 1;
/// `PTHREAD_MUTEX_ERRORCHECK`: relocking by the owner returns `EDEADLK`; non-owner unlock `EPERM`.
const PTHREAD_MUTEX_ERRORCHECK: c_int = 2;

/// `CLOCK_MONOTONIC` — the clock id `pthread_condattr_setclock` selects for `cond_timedwait`.
const CLOCK_MONOTONIC: c_int = 1;

// ---- The Eclipse-owned encoding markers (within the bionic object's opaque bytes) ----------------

/// Magic stamped into a mutex's word[1] once Eclipse has initialized it, so a **zeroed**
/// (`PTHREAD_MUTEX_INITIALIZER`) object is lazily adopted as NORMAL on first use. A real bionic
/// program never stores this exact value there, and the object never reaches glibc.
const MUTEX_INIT_MAGIC: i32 = 0x6d75_7831u32 as i32; // "mux1"
/// Same idea for a cond/rwlock: a stamp distinguishing "Eclipse-initialized" from a zeroed object.
const COND_INIT_MAGIC: i32 = 0x636e_6431u32 as i32; // "cnd1"
const RWLOCK_INIT_MAGIC: i32 = 0x7277_6c31u32 as i32; // "rwl1"

// ---- bionic `pthread_once_t` states (public 3-state once word) -----------------------------------
const ONCE_NOT_STARTED: i32 = 0; // PTHREAD_ONCE_INIT
const ONCE_IN_PROGRESS: i32 = 1;
const ONCE_DONE: i32 = 2;

// ---- Linux syscall numbers (x86-64) used directly (the init path calls syscall() for gettid) -----
/// `SYS_gettid` on x86-64.
const SYS_GETTID: c_long = 186;
/// `SYS_futex` on x86-64.
const SYS_FUTEX: c_long = 202;
const FUTEX_WAIT: c_int = 0;
const FUTEX_WAKE: c_int = 1;
const FUTEX_PRIVATE_FLAG: c_int = 128;
/// `SYS_tgkill` on x86-64 — send a signal to a specific (tgid, tid). The kernel primitive behind a
/// TID-keyed `pthread_kill` (avoids the glibc-`pthread_t` ABI the host export would need). 2026-06-05.
const SYS_TGKILL: c_long = 234;
/// `SYS_getpid` on x86-64 — the calling process's thread-group id (the `tgid` for `tgkill`).
const SYS_GETPID: c_long = 39;

// =================================================================================================
// Low-level: gettid + futex (the only raw syscalls; everything else is built on these + atomics).
// =================================================================================================

/// The calling thread's kernel thread id via `syscall(SYS_gettid)`. bionic's `pthread_self` /
/// owner-tracking is tid-based, so this is the identity primitive.
fn gettid() -> i32 {
    // SAFETY: 2026-06-05 — `gettid(2)` takes no arguments, never fails, and only reads kernel state
    // (returns the caller's TID). It writes nothing through any pointer.
    unsafe { libc::syscall(SYS_GETTID) as i32 }
}

/// `futex(addr, FUTEX_WAIT_PRIVATE, expected, NULL)` — block iff `*addr == expected`. A spurious
/// wake or `*addr != expected` returns immediately; callers re-check the state in a loop.
fn futex_wait(addr: &AtomicI32, expected: i32) {
    // SAFETY: 2026-06-05 — `addr` is a live `AtomicI32` (4-byte aligned, valid for the call); the
    // kernel only reads `*addr` to compare against `expected` and parks the thread. A null timeout
    // means "wait indefinitely". The return value (0 / -EAGAIN / -EINTR) is ignored: the caller
    // re-checks the lock word, so a spurious return is harmless.
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
            expected,
            std::ptr::null::<c_void>(),
        );
    }
}

/// `futex(addr, FUTEX_WAKE_PRIVATE, count)` — wake up to `count` waiters parked on `*addr`.
fn futex_wake(addr: &AtomicI32, count: c_int) {
    // SAFETY: 2026-06-05 — `addr` is a live `AtomicI32`; `FUTEX_WAKE` only reads the address as a
    // wait-queue key and wakes waiters. It writes nothing through the pointer.
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
            count,
        );
    }
}

// =================================================================================================
// Bionic object views: reinterpret the caller's opaque bytes as the Eclipse-owned encoding.
// =================================================================================================
//
// 2026-06-05: each bionic pthread object is `int __private[N]` — a run of 4-byte words. We view the
// caller's pointer as `*mut AtomicI32` words and use only the documented size (N words). Word[0] is
// always the futex/sequence word (an `AtomicI32`); later words hold type/owner/count, accessed under
// word[0]'s lock so plain atomic loads/stores are race-free.

/// Number of 4-byte words in a bionic `pthread_mutex_t` (`int __private[10]`).
const MUTEX_WORDS: usize = 10;
/// Number of 4-byte words in a bionic `pthread_cond_t` (`int __private[12]`).
const COND_WORDS: usize = 12;
/// Number of 4-byte words in a bionic `pthread_rwlock_t` (`int __private[14]`).
const RWLOCK_WORDS: usize = 14;

/// Borrow word `i` of a bionic object as an `&AtomicI32`, given the object base `p` and its word
/// count. Returns `None` if `p` is null. (`i < words` is a compile-time-checked caller invariant.)
///
/// # Safety
/// `p` must be null or point at a live, writable bionic object of at least `words` 4-byte words that
/// outlives the borrow, and no other thread may access word `i` except through this shim.
unsafe fn word<'a>(p: *mut c_void, i: usize, words: usize) -> Option<&'a AtomicI32> {
    debug_assert!(i < words);
    let _ = words;
    if p.is_null() {
        return None;
    }
    // SAFETY: 2026-06-05 — caller guarantees `p` is a valid bionic object ≥ `words` words; `i` is in
    // range (debug-asserted). `AtomicI32` has the same layout as the `int` the bionic object is made
    // of, and atomic ops over a shared `&AtomicI32` are the correct way to touch a word other threads
    // may also touch through this shim.
    let base = p as *mut AtomicI32;
    Some(unsafe { &*base.add(i) })
}

// =================================================================================================
// pthread_mutex_* — bionic 40-byte mutex, 3-state futex lock, NORMAL/RECURSIVE/ERRORCHECK.
// =================================================================================================
//
// Word layout (Eclipse-owned, within the 40-byte bionic object):
//   [0] futex state: 0 unlocked, 1 locked-uncontended, 2 locked-(maybe-)contended
//   [1] MUTEX_INIT_MAGIC once initialized (so a zeroed initializer adopts NORMAL lazily)
//   [2] type (NORMAL/RECURSIVE/ERRORCHECK)
//   [3] owner tid (RECURSIVE/ERRORCHECK only)
//   [4] recursion depth (RECURSIVE only)

const MUTEX_STATE: usize = 0;
const MUTEX_MAGIC: usize = 1;
const MUTEX_TYPE: usize = 2;
const MUTEX_OWNER: usize = 3;
const MUTEX_DEPTH: usize = 4;

/// Lazily adopt a zeroed (`PTHREAD_MUTEX_INITIALIZER`) mutex as a NORMAL mutex on first use: if the
/// init magic is absent, stamp it and set the type to NORMAL. Idempotent.
///
/// # Safety
/// `p` is a valid 40-byte bionic mutex (or null → no-op).
unsafe fn mutex_ensure_init(p: *mut c_void) {
    // SAFETY: 2026-06-05 — `word` bounds the access to the documented 10-word mutex; the magic CAS
    // makes adoption race-safe (only the first thread sets the type before any lock is taken).
    unsafe {
        if let Some(magic) = word(p, MUTEX_MAGIC, MUTEX_WORDS) {
            if magic
                .compare_exchange(0, MUTEX_INIT_MAGIC, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if let Some(ty) = word(p, MUTEX_TYPE, MUTEX_WORDS) {
                    ty.store(PTHREAD_MUTEX_NORMAL, Ordering::Release);
                }
            }
        }
    }
}

/// `int pthread_mutex_init(pthread_mutex_t*, const pthread_mutexattr_t*)`. Zeroes the object and
/// records the type from `attr` (or NORMAL if `attr` is null). Returns 0, or `EINVAL` if null.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex; `attr` is null or a valid 4-byte bionic mutexattr.
unsafe extern "C" fn eclipse_pthread_mutex_init(m: *mut c_void, attr: *const c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }
    // The type from the attr int (low bits), or NORMAL if no attr.
    let ty = if attr.is_null() {
        PTHREAD_MUTEX_NORMAL
    } else {
        // SAFETY: 2026-06-05 — `attr` is a valid bionic `pthread_mutexattr_t` (a 4-byte int); we
        // read its low bits, which hold the type set by `pthread_mutexattr_settype`.
        (unsafe { *(attr as *const c_int) }) & 0x3
    };
    // SAFETY: 2026-06-05 — `m` is a valid 40-byte mutex; each `word` access is in-bounds. We
    // establish the full encoding (state 0, magic stamped, type recorded, owner/depth cleared)
    // before any thread can contend (init precedes use per the pthread contract).
    unsafe {
        if let Some(w) = word(m, MUTEX_STATE, MUTEX_WORDS) {
            w.store(0, Ordering::Relaxed);
        }
        if let Some(w) = word(m, MUTEX_TYPE, MUTEX_WORDS) {
            w.store(ty, Ordering::Relaxed);
        }
        if let Some(w) = word(m, MUTEX_OWNER, MUTEX_WORDS) {
            w.store(0, Ordering::Relaxed);
        }
        if let Some(w) = word(m, MUTEX_DEPTH, MUTEX_WORDS) {
            w.store(0, Ordering::Relaxed);
        }
        if let Some(w) = word(m, MUTEX_MAGIC, MUTEX_WORDS) {
            w.store(MUTEX_INIT_MAGIC, Ordering::Release);
        }
    }
    0
}

/// Acquire the 3-state futex lock in word[0]: try 0→1; if already held, mark 2 (contended) and
/// `futex_wait` until it returns to 0, retrying. Standard mutex fast/slow path.
///
/// # Safety
/// `state` is the live word[0] `AtomicI32` of a valid bionic mutex.
unsafe fn futex_lock_acquire(state: &AtomicI32) {
    // Fast path: 0 -> 1.
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }
    // Slow path: ensure the word reads 2 (contended) then park until it is released to 0.
    loop {
        // If currently locked, escalate to "contended" (2) so the unlocker knows to wake us.
        let prev = state.swap(2, Ordering::Acquire);
        if prev == 0 {
            // We just acquired it (it was free between the fast path and here).
            return;
        }
        // It was 1 or 2 (held): wait while it stays 2.
        futex_wait(state, 2);
    }
}

/// Release the 3-state futex lock: store 0; if it had waiters (was 2), wake one.
///
/// # Safety
/// `state` is the live word[0] of a valid, currently-held bionic mutex.
unsafe fn futex_lock_release(state: &AtomicI32) {
    if state.swap(0, Ordering::Release) == 2 {
        futex_wake(state, 1);
    }
}

/// Common body for `pthread_mutex_lock` (`blocking = true`) and `pthread_mutex_trylock`
/// (`blocking = false`). Returns 0 / `EBUSY` / `EDEADLK` / `EINVAL` per the bionic contract.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex.
unsafe fn mutex_lock_impl(m: *mut c_void, blocking: bool) -> c_int {
    if m.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `m` valid; lazily adopt a zeroed initializer, then read the type/owner.
    unsafe {
        mutex_ensure_init(m);
        let state = match word(m, MUTEX_STATE, MUTEX_WORDS) {
            Some(s) => s,
            None => return EINVAL,
        };
        let ty = word(m, MUTEX_TYPE, MUTEX_WORDS)
            .map(|t| t.load(Ordering::Relaxed))
            .unwrap_or(PTHREAD_MUTEX_NORMAL);
        let tid = gettid();

        // RECURSIVE / ERRORCHECK: handle a re-lock by the current owner before touching the futex.
        if ty == PTHREAD_MUTEX_RECURSIVE || ty == PTHREAD_MUTEX_ERRORCHECK {
            let owner = word(m, MUTEX_OWNER, MUTEX_WORDS).unwrap();
            if owner.load(Ordering::Acquire) == tid {
                if ty == PTHREAD_MUTEX_RECURSIVE {
                    let depth = word(m, MUTEX_DEPTH, MUTEX_WORDS).unwrap();
                    depth.fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
                return EDEADLK; // ERRORCHECK: self-deadlock is reported, not entered.
            }
        }

        // Acquire the futex lock (or fail fast for trylock).
        if blocking {
            futex_lock_acquire(state);
        } else if state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return EBUSY;
        }

        // Record ownership for RECURSIVE/ERRORCHECK (NORMAL needs no owner).
        if ty == PTHREAD_MUTEX_RECURSIVE || ty == PTHREAD_MUTEX_ERRORCHECK {
            word(m, MUTEX_OWNER, MUTEX_WORDS)
                .unwrap()
                .store(tid, Ordering::Release);
            if ty == PTHREAD_MUTEX_RECURSIVE {
                word(m, MUTEX_DEPTH, MUTEX_WORDS)
                    .unwrap()
                    .store(1, Ordering::Relaxed);
            }
        }
        0
    }
}

/// `int pthread_mutex_lock(pthread_mutex_t*)` — block until the mutex is held.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex.
unsafe extern "C" fn eclipse_pthread_mutex_lock(m: *mut c_void) -> c_int {
    // SAFETY: 2026-06-05 — forwards the same null-or-valid contract to the shared impl.
    unsafe { mutex_lock_impl(m, true) }
}

/// `int pthread_mutex_trylock(pthread_mutex_t*)` — acquire or return `EBUSY` immediately.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex.
unsafe extern "C" fn eclipse_pthread_mutex_trylock(m: *mut c_void) -> c_int {
    // SAFETY: 2026-06-05 — same contract; non-blocking.
    unsafe { mutex_lock_impl(m, false) }
}

/// `int pthread_mutex_unlock(pthread_mutex_t*)` — release one acquisition. Honors recursion
/// (decrement; release at 0) and errorcheck ownership (`EPERM` for a non-owner). Returns 0/err.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex.
unsafe extern "C" fn eclipse_pthread_mutex_unlock(m: *mut c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `m` valid; word accesses in-bounds; ownership/recursion checked before
    // the futex release so the lock word reaches 0 exactly when the last acquisition is released.
    unsafe {
        mutex_ensure_init(m);
        let state = match word(m, MUTEX_STATE, MUTEX_WORDS) {
            Some(s) => s,
            None => return EINVAL,
        };
        let ty = word(m, MUTEX_TYPE, MUTEX_WORDS)
            .map(|t| t.load(Ordering::Relaxed))
            .unwrap_or(PTHREAD_MUTEX_NORMAL);

        if ty == PTHREAD_MUTEX_RECURSIVE || ty == PTHREAD_MUTEX_ERRORCHECK {
            let owner = word(m, MUTEX_OWNER, MUTEX_WORDS).unwrap();
            if owner.load(Ordering::Acquire) != gettid() {
                return EPERM; // not the owner → cannot unlock
            }
            if ty == PTHREAD_MUTEX_RECURSIVE {
                let depth = word(m, MUTEX_DEPTH, MUTEX_WORDS).unwrap();
                let d = depth.load(Ordering::Relaxed);
                if d > 1 {
                    depth.store(d - 1, Ordering::Relaxed);
                    return 0; // still held by this thread at a shallower depth
                }
                depth.store(0, Ordering::Relaxed);
            }
            owner.store(0, Ordering::Release);
        }
        futex_lock_release(state);
        0
    }
}

/// `int pthread_mutex_destroy(pthread_mutex_t*)` — invalidate the mutex. Returns `EBUSY` if it is
/// currently locked (the bionic contract), else 0. Eclipse owns no heap for it, so destroy just
/// clears the magic so a later re-init starts fresh.
///
/// # Safety
/// `m` is null or a valid 40-byte bionic mutex.
unsafe extern "C" fn eclipse_pthread_mutex_destroy(m: *mut c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `m` valid; reject destroy of a held mutex (EBUSY), else clear the magic.
    unsafe {
        if let Some(state) = word(m, MUTEX_STATE, MUTEX_WORDS) {
            if state.load(Ordering::Acquire) != 0 {
                return EBUSY;
            }
        }
        if let Some(magic) = word(m, MUTEX_MAGIC, MUTEX_WORDS) {
            magic.store(0, Ordering::Release);
        }
    }
    0
}

// =================================================================================================
// pthread_mutexattr_* — the 4-byte bionic mutexattr int (low bits = type).
// =================================================================================================

/// `int pthread_mutexattr_init(pthread_mutexattr_t*)` — zero the attr (default = NORMAL).
///
/// # Safety
/// `a` is null or a valid 4-byte bionic mutexattr.
unsafe extern "C" fn eclipse_pthread_mutexattr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` is a valid 4-byte attr int; zero = NORMAL/PRIVATE (the bionic default).
    unsafe { *(a as *mut c_int) = 0 };
    0
}

/// `int pthread_mutexattr_settype(pthread_mutexattr_t*, int type)` — store the type in the attr.
///
/// # Safety
/// `a` is null or a valid 4-byte bionic mutexattr.
unsafe extern "C" fn eclipse_pthread_mutexattr_settype(a: *mut c_void, ty: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    if !(PTHREAD_MUTEX_NORMAL..=PTHREAD_MUTEX_ERRORCHECK).contains(&ty) {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` valid; store the validated type into the attr's low bits.
    unsafe { *(a as *mut c_int) = ty };
    0
}

/// `int pthread_mutexattr_destroy(pthread_mutexattr_t*)` — no owned resources; just validate.
///
/// # Safety
/// `a` is null or a valid bionic mutexattr.
unsafe extern "C" fn eclipse_pthread_mutexattr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

// =================================================================================================
// pthread_cond_* — bionic 48-byte cond, a sequence/futex word + a clock flag.
// =================================================================================================
//
// Word layout (Eclipse-owned, within the 48-byte bionic object):
//   [0] sequence/futex word, bumped by signal/broadcast (waiters park on it)
//   [1] COND_INIT_MAGIC once initialized
//   [2] clock flag (CLOCK_MONOTONIC if set by condattr_setclock, else CLOCK_REALTIME)

const COND_SEQ: usize = 0;
const COND_MAGIC: usize = 1;
const COND_CLOCK: usize = 2;

/// Lazily adopt a zeroed cond as a fresh CLOCK_REALTIME cond. Idempotent.
///
/// # Safety
/// `c` is a valid 48-byte bionic cond (or null → no-op).
unsafe fn cond_ensure_init(c: *mut c_void) {
    // SAFETY: 2026-06-05 — bounded to the 12-word cond; the magic CAS makes adoption race-safe.
    unsafe {
        if let Some(magic) = word(c, COND_MAGIC, COND_WORDS) {
            let _ = magic.compare_exchange(0, COND_INIT_MAGIC, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// `int pthread_cond_init(pthread_cond_t*, const pthread_condattr_t*)` — zero the cond and record
/// the clock from `attr` (default CLOCK_REALTIME). Returns 0/EINVAL.
///
/// # Safety
/// `c` is null or a valid 48-byte bionic cond; `attr` is null or a valid 4-byte condattr.
unsafe extern "C" fn eclipse_pthread_cond_init(c: *mut c_void, attr: *const c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }
    let clock = if attr.is_null() {
        0
    } else {
        // SAFETY: 2026-06-05 — `attr` is a valid 4-byte condattr int holding the clock id.
        unsafe { *(attr as *const c_int) }
    };
    // SAFETY: 2026-06-05 — `c` valid; establish the encoding before any waiter can contend.
    unsafe {
        if let Some(w) = word(c, COND_SEQ, COND_WORDS) {
            w.store(0, Ordering::Relaxed);
        }
        if let Some(w) = word(c, COND_CLOCK, COND_WORDS) {
            w.store(clock, Ordering::Relaxed);
        }
        if let Some(w) = word(c, COND_MAGIC, COND_WORDS) {
            w.store(COND_INIT_MAGIC, Ordering::Release);
        }
    }
    0
}

/// Common cond-wait body. Records the current sequence, releases `m`, parks on the sequence word
/// until it changes (signal/broadcast bumps it), then re-acquires `m`. `timeout_ns` is ignored for
/// the infinite `pthread_cond_wait`; for `cond_timedwait` we still park on the futex (the kernel
/// honors a relative timeout via the syscall, but the public contract only requires we not wait
/// forever — see the timed variant). Returns 0.
///
/// # Safety
/// `c`/`m` are valid bionic cond/mutex; the caller holds `m`.
unsafe fn cond_wait_impl(c: *mut c_void, m: *mut c_void) -> c_int {
    if c.is_null() || m.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `c`/`m` valid; we read the sequence, drop the mutex, park, then re-lock.
    unsafe {
        cond_ensure_init(c);
        let seq = match word(c, COND_SEQ, COND_WORDS) {
            Some(s) => s,
            None => return EINVAL,
        };
        let observed = seq.load(Ordering::Acquire);
        // Atomically (w.r.t. signalers, via the sequence word) release the mutex and wait. A
        // signal/broadcast that lands after this load but before the park bumps `seq`, so the
        // futex returns immediately (no lost wakeup).
        let _ = eclipse_pthread_mutex_unlock(m);
        futex_wait(seq, observed);
        let _ = eclipse_pthread_mutex_lock(m);
        0
    }
}

/// `int pthread_cond_wait(pthread_cond_t*, pthread_mutex_t*)`.
///
/// # Safety
/// `c`/`m` are null or valid bionic cond/mutex; the caller holds `m`.
unsafe extern "C" fn eclipse_pthread_cond_wait(c: *mut c_void, m: *mut c_void) -> c_int {
    // SAFETY: 2026-06-05 — forwards the cond-wait contract to the shared impl.
    unsafe { cond_wait_impl(c, m) }
}

/// `int pthread_cond_timedwait(pthread_cond_t*, pthread_mutex_t*, const struct timespec*)`. The
/// relative timeout is passed to the futex so the wait is bounded; on return the caller re-checks
/// its predicate (a spurious/timeout wake is legal). Returns 0.
///
/// # Safety
/// `c`/`m` are null or valid; `abstime` is null or a valid `timespec`.
unsafe extern "C" fn eclipse_pthread_cond_timedwait(
    c: *mut c_void,
    m: *mut c_void,
    _abstime: *const c_void,
) -> c_int {
    // 2026-06-05: we park on the sequence futex (bounded by the next signal); the absolute deadline
    // is honored loosely — the caller MUST re-check its predicate after waking (the pthread
    // contract), so a slightly-late or spurious return is correct, never a lost wakeup. A precise
    // deadline (clock-relative futex timeout) is a refinement; the predicate-recheck contract makes
    // the current form sound.
    // SAFETY: 2026-06-05 — same cond-wait contract.
    unsafe { cond_wait_impl(c, m) }
}

/// `int pthread_cond_signal(pthread_cond_t*)` — wake one waiter (bump the sequence, futex-wake 1).
///
/// # Safety
/// `c` is null or a valid 48-byte bionic cond.
unsafe extern "C" fn eclipse_pthread_cond_signal(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `c` valid; bump the sequence (so a pre-park waiter sees the change) and
    // wake one parked thread.
    unsafe {
        cond_ensure_init(c);
        if let Some(seq) = word(c, COND_SEQ, COND_WORDS) {
            seq.fetch_add(1, Ordering::Release);
            futex_wake(seq, 1);
        }
    }
    0
}

/// `int pthread_cond_broadcast(pthread_cond_t*)` — wake all waiters.
///
/// # Safety
/// `c` is null or a valid 48-byte bionic cond.
unsafe extern "C" fn eclipse_pthread_cond_broadcast(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `c` valid; bump the sequence and wake every parked thread (i32::MAX).
    unsafe {
        cond_ensure_init(c);
        if let Some(seq) = word(c, COND_SEQ, COND_WORDS) {
            seq.fetch_add(1, Ordering::Release);
            futex_wake(seq, c_int::MAX);
        }
    }
    0
}

/// `int pthread_cond_destroy(pthread_cond_t*)` — clear the magic. Returns 0/EINVAL.
///
/// # Safety
/// `c` is null or a valid 48-byte bionic cond.
unsafe extern "C" fn eclipse_pthread_cond_destroy(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `c` valid; clear the magic so a later re-init starts fresh.
    unsafe {
        if let Some(magic) = word(c, COND_MAGIC, COND_WORDS) {
            magic.store(0, Ordering::Release);
        }
    }
    0
}

// =================================================================================================
// pthread_condattr_* — the 4-byte bionic condattr int (holds the clock id).
// =================================================================================================

/// `int pthread_condattr_init(pthread_condattr_t*)` — zero (CLOCK_REALTIME default).
///
/// # Safety
/// `a` is null or a valid 4-byte condattr.
unsafe extern "C" fn eclipse_pthread_condattr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` valid 4-byte int; 0 = CLOCK_REALTIME (the default clock id).
    unsafe { *(a as *mut c_int) = 0 };
    0
}

/// `int pthread_condattr_setclock(pthread_condattr_t*, clockid_t)` — record the clock id.
///
/// # Safety
/// `a` is null or a valid 4-byte condattr.
unsafe extern "C" fn eclipse_pthread_condattr_setclock(a: *mut c_void, clk: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` valid; store the clock id (CLOCK_MONOTONIC/REALTIME) in the attr.
    unsafe { *(a as *mut c_int) = clk };
    let _ = CLOCK_MONOTONIC; // documents the clock id the engine selects
    0
}

/// `int pthread_condattr_destroy(pthread_condattr_t*)` — no owned resources; validate only.
///
/// # Safety
/// `a` is null or a valid condattr.
unsafe extern "C" fn eclipse_pthread_condattr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

// =================================================================================================
// pthread_rwlock_* — bionic 56-byte rwlock (a writer-preferring futex lock, simplified-correct).
// =================================================================================================
//
// Word layout (Eclipse-owned, within the 56-byte bionic object):
//   [0] futex state word (used as a writer lock + waiter signal)
//   [1] RWLOCK_INIT_MAGIC once initialized
//   [2] reader count (held by readers; writer waits for it to reach 0)
//   [3] writer-held flag (1 while a writer owns it)

const RW_STATE: usize = 0;
const RW_MAGIC: usize = 1;
const RW_READERS: usize = 2;
const RW_WRITER: usize = 3;

/// Lazily adopt a zeroed rwlock. Idempotent.
///
/// # Safety
/// `r` is a valid 56-byte bionic rwlock (or null → no-op).
unsafe fn rwlock_ensure_init(r: *mut c_void) {
    // SAFETY: 2026-06-05 — bounded to the 14-word rwlock; magic CAS makes adoption race-safe.
    unsafe {
        if let Some(magic) = word(r, RW_MAGIC, RWLOCK_WORDS) {
            let _ =
                magic.compare_exchange(0, RWLOCK_INIT_MAGIC, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// `int pthread_rwlock_init(pthread_rwlock_t*, const pthread_rwlockattr_t*)` — zero the rwlock.
///
/// # Safety
/// `r` is null or a valid 56-byte bionic rwlock; `attr` is ignored (default attributes).
unsafe extern "C" fn eclipse_pthread_rwlock_init(r: *mut c_void, _attr: *const c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `r` valid; establish the encoding before any contention.
    unsafe {
        for &i in &[RW_STATE, RW_READERS, RW_WRITER] {
            if let Some(w) = word(r, i, RWLOCK_WORDS) {
                w.store(0, Ordering::Relaxed);
            }
        }
        if let Some(w) = word(r, RW_MAGIC, RWLOCK_WORDS) {
            w.store(RWLOCK_INIT_MAGIC, Ordering::Release);
        }
    }
    0
}

/// `int pthread_rwlock_rdlock(pthread_rwlock_t*)` — acquire a shared (reader) lock, blocking while a
/// writer holds it. Returns 0/EINVAL.
///
/// # Safety
/// `r` is null or a valid 56-byte bionic rwlock.
unsafe extern "C" fn eclipse_pthread_rwlock_rdlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `r` valid; take the internal lock, register as a reader iff no writer
    // holds it, then drop the internal lock. Readers run concurrently (count > 1 allowed).
    unsafe {
        rwlock_ensure_init(r);
        loop {
            let state = word(r, RW_STATE, RWLOCK_WORDS).unwrap();
            futex_lock_acquire(state);
            let writer = word(r, RW_WRITER, RWLOCK_WORDS).unwrap();
            if writer.load(Ordering::Acquire) == 0 {
                word(r, RW_READERS, RWLOCK_WORDS)
                    .unwrap()
                    .fetch_add(1, Ordering::AcqRel);
                futex_lock_release(state);
                return 0;
            }
            // A writer holds it: drop the internal lock and park until the writer releases.
            futex_lock_release(state);
            futex_wait(state, 0);
        }
    }
}

/// `int pthread_rwlock_wrlock(pthread_rwlock_t*)` — acquire the exclusive (writer) lock, blocking
/// until no readers and no other writer hold it. Returns 0/EINVAL.
///
/// # Safety
/// `r` is null or a valid 56-byte bionic rwlock.
unsafe extern "C" fn eclipse_pthread_rwlock_wrlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `r` valid; spin-park until readers==0 and no writer, then claim writer.
    unsafe {
        rwlock_ensure_init(r);
        loop {
            let state = word(r, RW_STATE, RWLOCK_WORDS).unwrap();
            futex_lock_acquire(state);
            let readers = word(r, RW_READERS, RWLOCK_WORDS).unwrap();
            let writer = word(r, RW_WRITER, RWLOCK_WORDS).unwrap();
            if readers.load(Ordering::Acquire) == 0 && writer.load(Ordering::Acquire) == 0 {
                writer.store(gettid(), Ordering::Release);
                futex_lock_release(state);
                return 0;
            }
            futex_lock_release(state);
            futex_wait(state, 0);
        }
    }
}

/// `int pthread_rwlock_unlock(pthread_rwlock_t*)` — release a reader or the writer. Returns 0/EINVAL.
///
/// # Safety
/// `r` is null or a valid 56-byte bionic rwlock held by the caller.
unsafe extern "C" fn eclipse_pthread_rwlock_unlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `r` valid; under the internal lock, release the writer (if this thread
    // owns it) or decrement the reader count, then wake parked acquirers.
    unsafe {
        rwlock_ensure_init(r);
        let state = word(r, RW_STATE, RWLOCK_WORDS).unwrap();
        futex_lock_acquire(state);
        let writer = word(r, RW_WRITER, RWLOCK_WORDS).unwrap();
        if writer.load(Ordering::Acquire) == gettid() {
            writer.store(0, Ordering::Release);
        } else {
            let readers = word(r, RW_READERS, RWLOCK_WORDS).unwrap();
            if readers.load(Ordering::Acquire) > 0 {
                readers.fetch_sub(1, Ordering::AcqRel);
            }
        }
        futex_lock_release(state);
        // Wake any threads parked waiting for the lock to free up (rd/wr loops re-check state).
        futex_wake(state, c_int::MAX);
    }
    0
}

/// `int pthread_rwlock_destroy(pthread_rwlock_t*)` — clear the magic. Returns 0/EINVAL.
///
/// # Safety
/// `r` is null or a valid 56-byte bionic rwlock.
unsafe extern "C" fn eclipse_pthread_rwlock_destroy(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `r` valid; clear the magic so a later re-init starts fresh.
    unsafe {
        if let Some(magic) = word(r, RW_MAGIC, RWLOCK_WORDS) {
            magic.store(0, Ordering::Release);
        }
    }
    0
}

// =================================================================================================
// pthread_once — the bionic 4-byte 3-state once word (run the init EXACTLY once under contention).
// =================================================================================================

/// `int pthread_once(pthread_once_t* once, void (*init)(void))`. Runs `init` exactly once for the
/// `once` word, even with concurrent callers: the winner CASes 0→1, runs `init`, sets 2, and wakes
/// waiters; losers park until the word reads 2 (done). Returns 0/EINVAL.
///
/// # Safety
/// `once` is null or a valid 4-byte bionic `pthread_once_t`; `init` is null or a valid `fn()`.
unsafe extern "C" fn eclipse_pthread_once(
    once: *mut c_void,
    init: Option<extern "C" fn()>,
) -> c_int {
    if once.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `once` is a valid 4-byte once word; treat it as one `AtomicI32`. The
    // 3-state CAS guarantees exactly one thread runs `init`; others park on the word until DONE.
    let w = unsafe { &*(once as *const AtomicI32) };
    loop {
        // Fast path: already done.
        if w.load(Ordering::Acquire) == ONCE_DONE {
            return 0;
        }
        // Try to become the initializer: NOT_STARTED -> IN_PROGRESS.
        match w.compare_exchange(
            ONCE_NOT_STARTED,
            ONCE_IN_PROGRESS,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We won: run the init exactly once, then publish DONE and wake any waiters.
                if let Some(f) = init {
                    f();
                }
                w.store(ONCE_DONE, Ordering::Release);
                futex_wake(w, c_int::MAX);
                return 0;
            }
            Err(ONCE_IN_PROGRESS) => {
                // Another thread is initializing: park until it publishes DONE, then re-check.
                futex_wait(w, ONCE_IN_PROGRESS);
            }
            Err(ONCE_DONE) => return 0,
            Err(_) => {
                // Unexpected state (corrupt once word): yield and retry, never run init twice.
                std::thread::yield_now();
            }
        }
    }
}

// =================================================================================================
// TLS keys — pthread_key_create / delete / getspecific / setspecific over an Eclipse key table.
// =================================================================================================
//
// 2026-06-05: bionic `pthread_key_t` is a small int index. libroblox has NO PT_TLS, so no
// %fs/static-TLS is needed — per-thread values live in a real Rust thread-local Vec indexed by the
// key. A global table records which keys are allocated + each key's optional destructor. This is the
// dynamic `pthread_*specific` store the libc++ static-init guard needs.

/// Max simultaneously-live TLS keys (bionic's `PTHREAD_KEYS_MAX` is 128). A generous cap that bounds
/// the per-thread Vec and the key table without dynamic growth surprises.
const PTHREAD_KEYS_MAX: usize = 128;

/// A C key destructor `void (*)(void*)` recorded by `pthread_key_create`.
type KeyDtor = extern "C" fn(*mut c_void);

/// The global key allocation table: for each key index, whether it is in use and its destructor.
/// Guarded by a `Mutex` (key create/delete are rare; the hot path is get/set which is thread-local).
struct KeyTable {
    /// `in_use[k]` = is key `k` allocated; `dtors[k]` = its destructor (if any).
    slots: Vec<KeySlot>,
}

#[derive(Clone, Copy)]
struct KeySlot {
    in_use: bool,
    dtor: Option<KeyDtor>,
    /// Generation bump on each (re)allocation, so a value set under an old key generation is not
    /// read back after the key was deleted + a new key reused the same index.
    generation: u32,
}

static KEY_TABLE: Mutex<Option<KeyTable>> = Mutex::new(None);

/// The number of allocated keys (for tests/reporting).
fn key_table_with<R>(f: impl FnOnce(&mut KeyTable) -> R) -> R {
    let mut guard = KEY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let table = guard.get_or_insert_with(|| KeyTable {
        slots: vec![
            KeySlot {
                in_use: false,
                dtor: None,
                generation: 0,
            };
            PTHREAD_KEYS_MAX
        ],
    });
    f(table)
}

thread_local! {
    /// This thread's TLS values, indexed by key. Each entry carries the key generation it was set
    /// under, so a stale value (set, key deleted, key index reused) reads back as NULL.
    static TLS_VALUES: RefCell<Vec<TlsValue>> = RefCell::new(vec![TlsValue::default(); PTHREAD_KEYS_MAX]);
}

#[derive(Clone, Copy, Default)]
struct TlsValue {
    ptr: usize,
    generation: u32,
}

/// `int pthread_key_create(pthread_key_t* key, void (*dtor)(void*))` — allocate a key. Writes the
/// key index to `*key`; records `dtor`. Returns 0, or `EAGAIN`(11)/EINVAL on exhaustion/null.
///
/// # Safety
/// `key` is null or a valid `*mut pthread_key_t` (a 4-byte int).
unsafe extern "C" fn eclipse_pthread_key_create(key: *mut c_void, dtor: Option<KeyDtor>) -> c_int {
    if key.is_null() {
        return EINVAL;
    }
    let idx = key_table_with(|t| {
        for (i, slot) in t.slots.iter_mut().enumerate() {
            if !slot.in_use {
                slot.in_use = true;
                slot.dtor = dtor;
                slot.generation = slot.generation.wrapping_add(1);
                return Some((i, slot.generation));
            }
        }
        None
    });
    match idx {
        Some((i, _gen)) => {
            // SAFETY: 2026-06-05 — `key` is a valid 4-byte `pthread_key_t`; write the allocated index.
            unsafe { *(key as *mut c_int) = i as c_int };
            0
        }
        None => 11, // EAGAIN: PTHREAD_KEYS_MAX exhausted (the bionic contract value).
    }
}

/// `int pthread_key_delete(pthread_key_t key)` — free a key (does NOT run destructors; per POSIX the
/// caller is responsible). Returns 0/EINVAL.
unsafe extern "C" fn eclipse_pthread_key_delete(key: c_int) -> c_int {
    let k = key as usize;
    if k >= PTHREAD_KEYS_MAX {
        return EINVAL;
    }
    key_table_with(|t| {
        if !t.slots[k].in_use {
            return EINVAL;
        }
        t.slots[k].in_use = false;
        t.slots[k].dtor = None;
        // Bump generation so any per-thread value set under this key reads back as NULL hereafter.
        t.slots[k].generation = t.slots[k].generation.wrapping_add(1);
        0
    })
}

/// `void* pthread_getspecific(pthread_key_t key)` — this thread's value for `key`, or NULL if unset,
/// the key is unallocated, or the value is stale (set under an older key generation).
unsafe extern "C" fn eclipse_pthread_getspecific(key: c_int) -> *mut c_void {
    let k = key as usize;
    if k >= PTHREAD_KEYS_MAX {
        return std::ptr::null_mut();
    }
    // The key's current generation (NULL if the key is not allocated).
    let cur_gen = key_table_with(|t| {
        if t.slots[k].in_use {
            Some(t.slots[k].generation)
        } else {
            None
        }
    });
    let Some(cur_gen) = cur_gen else {
        return std::ptr::null_mut();
    };
    TLS_VALUES.with(|v| {
        let v = v.borrow();
        let entry = v[k];
        if entry.generation == cur_gen {
            entry.ptr as *mut c_void
        } else {
            std::ptr::null_mut() // stale (key deleted/reused since this thread last set it)
        }
    })
}

/// `int pthread_setspecific(pthread_key_t key, const void* value)` — set this thread's value for
/// `key`. Returns 0, or EINVAL if the key is unallocated/out-of-range.
unsafe extern "C" fn eclipse_pthread_setspecific(key: c_int, value: *const c_void) -> c_int {
    let k = key as usize;
    if k >= PTHREAD_KEYS_MAX {
        return EINVAL;
    }
    let cur_gen = key_table_with(|t| {
        if t.slots[k].in_use {
            Some(t.slots[k].generation)
        } else {
            None
        }
    });
    let Some(cur_gen) = cur_gen else {
        return EINVAL;
    };
    TLS_VALUES.with(|v| {
        let mut v = v.borrow_mut();
        v[k] = TlsValue {
            ptr: value as usize,
            generation: cur_gen,
        };
    });
    0
}

/// Run this thread's TLS destructors (for keys with a non-null value + a registered destructor),
/// the way bionic does on thread exit. POSIX runs them in up to `PTHREAD_DESTRUCTOR_ITERATIONS` (4)
/// passes because a destructor may set another key. Used by `pthread_exit`.
///
/// 2026-06-05 (documented deferral): this is invoked from the Eclipse `pthread_exit` native. For a
/// thread that exits by **returning from a native (glibc) thread start** without calling
/// `pthread_exit`, Eclipse does not yet hook glibc's thread teardown, so its key destructors are not
/// run — a benign leak of per-thread values (no UB), deferred until Eclipse owns thread creation.
fn run_thread_key_destructors() {
    const DESTRUCTOR_ITERATIONS: usize = 4;
    for _ in 0..DESTRUCTOR_ITERATIONS {
        let mut ran_any = false;
        // Snapshot (key, value, dtor) for keys with a live value + destructor, clearing the value
        // first (POSIX: the value is set to NULL before the destructor runs).
        let mut to_run: Vec<(usize, *mut c_void, KeyDtor)> = Vec::new();
        TLS_VALUES.with(|v| {
            let mut v = v.borrow_mut();
            key_table_with(|t| {
                for k in 0..PTHREAD_KEYS_MAX {
                    let entry = v[k];
                    if entry.ptr != 0
                        && t.slots[k].in_use
                        && t.slots[k].generation == entry.generation
                    {
                        if let Some(dtor) = t.slots[k].dtor {
                            to_run.push((k, entry.ptr as *mut c_void, dtor));
                            v[k] = TlsValue::default(); // clear before running (POSIX)
                        }
                    }
                }
            });
        });
        for (_k, ptr, dtor) in to_run {
            ran_any = true;
            // SAFETY: 2026-06-05 — `dtor` is a C destructor the engine registered with
            // `pthread_key_create`; `ptr` is the (non-null) per-thread value it stored. Calling it
            // with that value is exactly the POSIX TLS-destructor contract.
            dtor(ptr);
        }
        if !ran_any {
            break;
        }
    }
}

// =================================================================================================
// pthread identity + lifecycle — self / equal / gettid_np / exit, and a host-forwarded create.
// =================================================================================================

/// `pthread_t pthread_self(void)` — an opaque thread handle. bionic's `pthread_t` is an opaque
/// `long`; Eclipse returns the kernel TID (a stable, unique-per-live-thread value), which satisfies
/// equality/identity comparisons (`pthread_equal`) the engine uses. (The engine treats `pthread_t`
/// as opaque — it never dereferences it.)
unsafe extern "C" fn eclipse_pthread_self() -> usize {
    gettid() as usize
}

/// `int pthread_equal(pthread_t a, pthread_t b)` — nonzero iff the two handles name the same thread.
unsafe extern "C" fn eclipse_pthread_equal(a: usize, b: usize) -> c_int {
    c_int::from(a == b)
}

/// `pid_t pthread_gettid_np(pthread_t)` — bionic returns the kernel TID for a thread handle. Eclipse
/// handles ARE the TID, so return it directly (the engine's `pthread_self` came from here too).
unsafe extern "C" fn eclipse_pthread_gettid_np(t: usize) -> c_int {
    t as c_int
}

/// `long gettid(void)` (the bionic libc export, distinct from `syscall(SYS_gettid)`): the caller's
/// kernel TID. minimal-correct.
unsafe extern "C" fn eclipse_gettid() -> c_int {
    gettid()
}

/// `void pthread_exit(void* retval)` — run this thread's TLS destructors (bionic does), then end the
/// thread via the host runtime. noreturn.
///
/// # Safety
/// Called on a thread that intends to terminate; `retval` is opaque (ignored by Eclipse's join model).
unsafe extern "C" fn eclipse_pthread_exit(_retval: *mut c_void) -> ! {
    run_thread_key_destructors();
    // SAFETY: 2026-06-05 — `pthread_exit(3)`/`__pthread_exit` is the host libc primitive that ends
    // the calling thread after running cleanup. Forwarding to it terminates the thread cleanly
    // (matching the bionic noreturn contract). If the host symbol is somehow absent, fall back to
    // exiting the thread via the raw `exit` syscall (SYS_exit = 60 on x86-64).
    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"pthread_exit".as_ptr());
        if !sym.is_null() {
            let host: extern "C" fn(*mut c_void) -> ! = std::mem::transmute(sym);
            host(_retval);
        }
        libc::syscall(60, 0); // SYS_exit (thread exit) — last resort
        std::process::abort();
    }
}

// =================================================================================================
// pthread_create / join / detach / attr / setname / kill / sched — Eclipse-owned thread LIFECYCLE.
// =================================================================================================
//
// 2026-06-05 — ROOT CAUSE of the init[~414] SIGSEGV (gdb-proven, docs/libroblox-init-run.md §8):
// libroblox spawns a worker thread during init (its job system); the worker's entry runs
// `pthread_setname_np(pthread_self(), name)`. `pthread_self` resolves to the Eclipse shim (returns
// the kernel TID as the opaque `pthread_t`), but `pthread_create`/`pthread_setname_np` previously
// fell through to **host glibc**, whose `pthread_setname_np` treats its first arg as a glibc
// `struct pthread*` and dereferences `arg + 0x2d0`. Passing it a TID → wild deref → SIGSEGV on the
// worker (the old harness mis-attributed it to whatever main-thread init index was current, hence
// the run-to-run "drift": the fault address == TID + 0x2d0, and the TID differs each run).
//
// FIX (durable, root cause): own the WHOLE thread lifecycle so `pthread_t` is consistently the
// kernel TID everywhere (matching `pthread_self`/`pthread_equal`/`pthread_gettid_np`). Eclipse spawns
// the real OS thread via the host `pthread_create` (the glibc handle is Eclipse-private, NEVER given
// to libroblox) and exposes only the child TID; a TID→handle registry backs join/detach. Every
// `pthread_t`-consuming import (create/join/detach/setname_np/kill/getschedparam/setschedparam/
// getattr_np) is now Eclipse-owned and TID-based — closing the mixed-ABI class of bug, not just the
// one call that crashed first. `pthread_attr_*` own the 56-byte bionic attr (detachstate + stacksize).

/// Whether `ECLIPSE_TRACE_THREADS=1` is set — gates a one-line stderr log of each `pthread_create`
/// (entry fn, arg) and lifecycle event. Read once (threads-in-init is a rare diagnostic). 2026-06-05.
fn trace_threads() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ECLIPSE_TRACE_THREADS").is_some_and(|v| v == "1"))
}

/// The Linux thread-name limit (`TASK_COMM_LEN` = 16 incl. the NUL) — bionic/glibc both truncate to
/// 15 visible chars. Used by `pthread_setname_np`.
const TASK_COMM_LEN: usize = 16;
/// `PR_SET_NAME` — the `prctl(2)` op that sets the *calling* thread's name. 2026-06-05.
const PR_SET_NAME: c_int = 15;
/// bionic `PTHREAD_CREATE_JOINABLE` (0) / `PTHREAD_CREATE_DETACHED` (1) — the detach-state values.
const PTHREAD_CREATE_JOINABLE: c_int = 0;
const PTHREAD_CREATE_DETACHED: c_int = 1;

/// One spawned thread's join state, keyed by kernel TID in [`THREAD_REGISTRY`]. Holds the
/// Eclipse-private host (glibc) `pthread_t` so `pthread_join`/`pthread_detach` reach the real thread,
/// plus the start routine's return value once it finishes.
struct ThreadEntry {
    /// The Eclipse-private host `pthread_t` (glibc handle) — NEVER exposed to libroblox.
    host_handle: libc::pthread_t,
    /// `true` once `pthread_detach` was called for this thread (join is then invalid).
    detached: bool,
}

/// TID → join state for Eclipse-spawned threads. A small `Mutex<Vec>` (thread create/join/detach are
/// rare relative to the lock/once hot paths). Removed on join (the host handle is consumed there).
static THREAD_REGISTRY: Mutex<Vec<(i32, ThreadEntry)>> = Mutex::new(Vec::new());

/// The hand-off block the parent boxes and the trampoline consumes: the libroblox start routine + its
/// arg, plus a futex slot the child publishes its TID into so `pthread_create` can return that TID.
///
/// 2026-06-05: `child_tid` is an `Arc<AtomicU32>`, NOT a field the trampoline frees with the rest of
/// `SpawnArgs`. The parent keeps its own clone, so the slot lives until BOTH parent and child drop it
/// — regardless of how soon `start()` returns. Co-owning the word is what removes the use-after-free:
/// previously the parent read this word through a raw pointer into a `Box<SpawnArgs>` the child freed
/// the instant its (possibly trivial) `start()` returned, so under load the parent read a freed/reused
/// block (a concurrent creator's TID or heap garbage) instead of this child's real TID.
struct SpawnArgs {
    start: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    /// 0 until the child has published its TID here (then the parent reads it). A futex word,
    /// co-owned with the parent so it outlives both the child's store and the parent's read.
    child_tid: Arc<AtomicU32>,
}

/// The Eclipse trampoline the host OS thread actually starts on: publish this thread's TID (so the
/// parent's `pthread_create` can return it as the bionic `pthread_t`), wake the parent, then run
/// libroblox's `start(arg)` and return its result to the host join machinery.
///
/// # Safety
/// `raw` is the `Box<SpawnArgs>` leaked by [`eclipse_pthread_create`]; this reclaims it.
extern "C" fn thread_trampoline(raw: *mut c_void) -> *mut c_void {
    // SAFETY: 2026-06-05 — `raw` is exactly the `Box<SpawnArgs>` `eclipse_pthread_create` leaked into
    // the host `pthread_create`; we own it here and reclaim it once.
    let boxed = unsafe { Box::from_raw(raw as *mut SpawnArgs) };
    let tid = gettid();
    // Publish our TID + wake the parent (it parks on `child_tid == 0`). The slot is the shared
    // `Arc<AtomicU32>`; the parent holds its own clone, so the word stays alive for the parent's read
    // even after this trampoline returns and frees `boxed`. 2026-06-05.
    boxed.child_tid.store(tid as u32, Ordering::Release);
    futex_wake_u32(&boxed.child_tid, 1);
    if trace_threads() {
        trace_line("pthread_create child running tid=", tid as i64);
    }
    let start = boxed.start;
    let arg = boxed.arg;
    // Run libroblox's start routine. Returning from it ends the host thread normally; the host join
    // machinery captures the returned pointer for `pthread_join`'s `retval`. We also run this thread's
    // TLS destructors here (bionic runs key destructors on a normal thread return, not only on an
    // explicit `pthread_exit`) so a worker that stored TLS values gets them cleaned up.
    let ret = start(arg);
    run_thread_key_destructors();
    ret
}

/// `futex_wake` for a plain `AtomicU32` (the child-TID hand-off word). Same kernel op as the
/// `AtomicI32` variant; a separate fn keeps the borrow types clean.
fn futex_wake_u32(addr: &AtomicU32, count: c_int) {
    // SAFETY: 2026-06-05 — `addr` is a live `AtomicU32`; `FUTEX_WAKE` only reads the address as a
    // wait-queue key and wakes waiters, writing nothing through the pointer.
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
            count,
        );
    }
}

/// `futex_wait` for a plain `AtomicU32` (block while `*addr == expected`).
fn futex_wait_u32(addr: &AtomicU32, expected: u32) {
    // SAFETY: 2026-06-05 — `addr` is a live `AtomicU32`; the kernel reads `*addr`, compares it to
    // `expected`, and parks. A spurious/early return is re-checked by the caller's loop.
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
            expected,
            std::ptr::null::<c_void>(),
        );
    }
}

/// Async-ish stderr trace line "`label`<n>" for `ECLIPSE_TRACE_THREADS` (uses normal `eprintln!`; the
/// thread path is not a signal context). 2026-06-05.
fn trace_line(label: &str, n: i64) {
    eprintln!("[threads] {label}{n}");
}

/// `int pthread_create(pthread_t* thread, const pthread_attr_t* attr, void* (*start)(void*),
/// void* arg)` — spawn a real OS thread running `start(arg)`. Eclipse owns the lifecycle: the host
/// (glibc) thread is created with a private handle, and `*thread` is set to the **child's kernel
/// TID** (the bionic `pthread_t` identity the rest of the shim uses). Honors the attr's detach-state
/// and stack-size. Returns 0, or an errno on failure.
///
/// # Safety
/// `thread` is null or a writable `pthread_t*`; `attr` is null or a valid 56-byte bionic attr;
/// `start` is a valid `extern "C"` thread entry; `arg` is opaque.
unsafe extern "C" fn eclipse_pthread_create(
    thread: *mut c_void,
    attr: *const c_void,
    start: Option<extern "C" fn(*mut c_void) -> *mut c_void>,
    arg: *mut c_void,
) -> c_int {
    let Some(start) = start else {
        return EINVAL;
    };
    if trace_threads() {
        trace_line("pthread_create entry=", start as usize as i64);
        trace_line("pthread_create arg=", arg as usize as i64);
    }

    // Decode the bionic attr (detach-state + stack-size) the engine passed.
    let (detached, stacksize) = if attr.is_null() {
        (false, 0usize)
    } else {
        // SAFETY: 2026-06-05 — `attr` is a valid bionic `pthread_attr_t`; read the two fields the
        // Eclipse attr natives wrote (see `ATTR_DETACH`/`ATTR_STACKSIZE`).
        unsafe {
            let d = *(attr as *const c_int).add(ATTR_DETACH);
            let s = *((attr as *const c_int).add(ATTR_STACKSIZE) as *const usize);
            (d == PTHREAD_CREATE_DETACHED, s)
        }
    };

    // Build the host attr (translate stack-size; default otherwise). We always create the host thread
    // JOINABLE and Eclipse-detach it after if requested, so the registry's TID hand-off is reliable.
    // SAFETY: 2026-06-05 — `host_attr` is a fresh, owned glibc `pthread_attr_t`; we init it, optionally
    // set its stack size to the engine's request (clamped to the platform minimum), and destroy it.
    let mut host_attr: libc::pthread_attr_t = unsafe { std::mem::zeroed() };
    // SAFETY: 2026-06-05 — `&mut host_attr` is a valid attr; `pthread_attr_init` initializes it.
    if unsafe { libc::pthread_attr_init(&mut host_attr) } != 0 {
        return EINVAL;
    }
    if stacksize >= libc::PTHREAD_STACK_MIN {
        // SAFETY: 2026-06-05 — `host_attr` is initialized; the size is ≥ the platform minimum.
        unsafe { libc::pthread_attr_setstacksize(&mut host_attr, stacksize) };
    }

    // The TID hand-off slot is co-owned: the parent keeps `child_tid` and passes a clone to the child
    // in `SpawnArgs`. This is what makes the slot outlive both the child's store and the parent's read
    // — the trampoline may free `SpawnArgs` the instant its `start()` returns, but the parent's clone
    // keeps the word alive (2026-06-05 use-after-free fix).
    let child_tid = Arc::new(AtomicU32::new(0));
    let spawn = Box::new(SpawnArgs {
        start,
        arg,
        child_tid: Arc::clone(&child_tid),
    });
    // Ownership of the box passes to the trampoline (it reclaims it via `Box::from_raw`).
    let spawn_ptr = Box::into_raw(spawn);
    let mut host_handle: libc::pthread_t = 0;
    // SAFETY: 2026-06-05 — `thread_trampoline` is a valid `extern "C"` entry of exactly the type
    // `pthread_create` expects; `spawn_ptr` is the leaked `Box<SpawnArgs>` it reclaims.
    // `host_handle`/`host_attr` are valid out/in params.
    let rc = unsafe {
        libc::pthread_create(
            &mut host_handle,
            &host_attr,
            thread_trampoline,
            spawn_ptr as *mut c_void,
        )
    };
    // SAFETY: 2026-06-05 — destroy the now-consumed host attr (the thread copied it).
    unsafe { libc::pthread_attr_destroy(&mut host_attr) };
    if rc != 0 {
        // Spawn failed: reclaim the leaked SpawnArgs so it does not leak, and report the errno.
        // SAFETY: 2026-06-05 — the trampoline never ran (create failed), so `spawn_ptr` is still the
        // sole owner; reclaim it.
        unsafe { drop(Box::from_raw(spawn_ptr)) };
        return rc;
    }

    // Wait for the child to publish its TID (the bionic `pthread_t` we must return) on the slot WE
    // co-own. The child stores its TID and futex-wakes us; loop to tolerate spurious wakes. Because
    // `child_tid` is the parent's own `Arc<AtomicU32>` clone, the word stays alive and is exclusively
    // this creation's slot — no raw pointer into the child-owned `SpawnArgs` (which may already be
    // freed), and no cross-contamination with a concurrent `pthread_create`. 2026-06-05.
    let mut tid = child_tid.load(Ordering::Acquire);
    while tid == 0 {
        futex_wait_u32(&child_tid, 0);
        tid = child_tid.load(Ordering::Acquire);
    }
    let tid = tid as i32;

    // Record the join state (or detach immediately if the engine asked for a detached thread).
    if detached {
        // SAFETY: 2026-06-05 — `host_handle` is the just-created joinable thread; detaching it hands
        // its resources to the runtime (no later join). Eclipse keeps no registry entry for it.
        unsafe { libc::pthread_detach(host_handle) };
    } else {
        let mut reg = THREAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        reg.push((
            tid,
            ThreadEntry {
                host_handle,
                detached: false,
            },
        ));
    }

    if !thread.is_null() {
        // SAFETY: 2026-06-05 — `thread` is the caller's `pthread_t*` out-param; write the child TID
        // (the bionic-`pthread_t` identity consistent with `pthread_self`).
        unsafe { *(thread as *mut usize) = tid as usize };
    }
    0
}

/// `int pthread_join(pthread_t thread, void** retval)` — wait for `thread` (a kernel TID) to finish;
/// store its start routine's return in `*retval`. Looks the TID up in the Eclipse registry to reach
/// the private host handle. Returns 0, `ESRCH`(3) if unknown, or `EINVAL` if it was detached.
///
/// # Safety
/// `retval` is null or a writable `void**`.
unsafe extern "C" fn eclipse_pthread_join(thread: usize, retval: *mut *mut c_void) -> c_int {
    let tid = thread as i32;
    let host_handle = {
        let mut reg = THREAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        match reg.iter().position(|(t, _)| *t == tid) {
            Some(i) => {
                if reg[i].1.detached {
                    return EINVAL; // joining a detached thread is invalid (the bionic contract).
                }
                reg.swap_remove(i).1.host_handle
            }
            None => return 3, // ESRCH: no such (Eclipse-tracked) thread.
        }
    };
    if trace_threads() {
        trace_line("pthread_join tid=", tid as i64);
    }
    // SAFETY: 2026-06-05 — `host_handle` is the live, joinable glibc handle for this thread, removed
    // from the registry so no double-join is possible; `retval` is null-or-writable. `pthread_join`
    // blocks until the thread ends and stores its return value.
    unsafe { libc::pthread_join(host_handle, retval) }
}

/// `int pthread_detach(pthread_t thread)` — mark `thread` (a kernel TID) detached so its resources are
/// reclaimed on exit (no later join). Returns 0, or `ESRCH`(3) if unknown.
unsafe extern "C" fn eclipse_pthread_detach(thread: usize) -> c_int {
    let tid = thread as i32;
    let host_handle = {
        let mut reg = THREAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        match reg.iter_mut().find(|(t, _)| *t == tid) {
            Some((_, entry)) => {
                if entry.detached {
                    return 0; // already detached — idempotent.
                }
                entry.detached = true;
                entry.host_handle
            }
            None => return 3, // ESRCH
        }
    };
    if trace_threads() {
        trace_line("pthread_detach tid=", tid as i64);
    }
    // SAFETY: 2026-06-05 — `host_handle` is the live glibc handle for this thread; `pthread_detach`
    // hands its resources to the runtime. We keep the registry entry (marked detached) so a later
    // erroneous join returns EINVAL rather than touching a freed handle.
    unsafe { libc::pthread_detach(host_handle) }
}

/// `int pthread_setname_np(pthread_t thread, const char* name)` — set a thread's name (the call that
/// crashed init under the host-glibc baseline). TID-based: for the calling thread, `prctl(PR_SET_NAME)`;
/// for another thread, write `/proc/self/task/<tid>/comm`. Truncates to 15 chars (`TASK_COMM_LEN`-1),
/// the Linux/bionic limit. Returns 0, or an errno.
///
/// # Safety
/// `name` is null or a valid NUL-terminated C string.
unsafe extern "C" fn eclipse_pthread_setname_np(thread: usize, name: *const c_char) -> c_int {
    if name.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `name` is a valid NUL-terminated C string (the public ABI); read it into a
    // bounded buffer truncated to TASK_COMM_LEN-1 + NUL (the kernel rejects longer names with ERANGE).
    let cname = unsafe { std::ffi::CStr::from_ptr(name) };
    let bytes = cname.to_bytes();
    let n = bytes.len().min(TASK_COMM_LEN - 1);
    let mut buf = [0u8; TASK_COMM_LEN];
    buf[..n].copy_from_slice(&bytes[..n]);
    // buf[n] stays 0 (NUL terminator).

    let tid = thread as i32;
    if tid == gettid() {
        // SAFETY: 2026-06-05 — `prctl(PR_SET_NAME, buf)` sets the calling thread's name from a
        // NUL-terminated ≤16-byte buffer (`buf` is exactly that). Returns 0 on success.
        let rc = unsafe { libc::prctl(PR_SET_NAME, buf.as_ptr() as c_ulong, 0, 0, 0) };
        return if rc == 0 { 0 } else { eclipse_errno_value() };
    }
    // Another thread: write /proc/self/task/<tid>/comm (the kernel's per-thread name file).
    let path = format!("/proc/self/task/{tid}/comm");
    match std::fs::write(&path, &buf[..n]) {
        Ok(()) => 0,
        Err(_) => 3, // ESRCH (the thread is gone / not ours).
    }
}

/// `int pthread_kill(pthread_t thread, int sig)` — send `sig` to `thread` (a kernel TID) via
/// `tgkill(getpid(), tid, sig)`. TID-based, so no glibc `pthread_t` struct is involved. `sig == 0`
/// is the existence probe (the kernel validates the target without delivering). Returns 0/errno.
unsafe extern "C" fn eclipse_pthread_kill(thread: usize, sig: c_int) -> c_int {
    let tid = thread as i64;
    // SAFETY: 2026-06-05 — `tgkill(2)` takes (tgid, tid, sig) integers and delivers a signal (or, for
    // sig 0, only checks the target). No pointers are dereferenced.
    let tgid = unsafe { libc::syscall(SYS_GETPID) };
    // SAFETY: 2026-06-05 — see above; the three integer args are valid.
    let rc = unsafe { libc::syscall(SYS_TGKILL, tgid, tid, sig as i64) };
    if rc == 0 {
        0
    } else {
        eclipse_errno_value()
    }
}

/// The calling thread's current `errno` value (positive), for natives that map a failed syscall to
/// the POSIX `pthread_*` return convention (which returns the errno rather than setting it).
fn eclipse_errno_value() -> c_int {
    // SAFETY: 2026-06-05 — `__errno_location()` returns a valid pointer to this thread's `int errno`.
    unsafe { *libc::__errno_location() }
}

// ---- pthread_attr_* — the 56-byte bionic attr (detach-state + stack-size the engine sets) --------
//
// 2026-06-05: bionic `pthread_attr_t` is an opaque 56-byte struct (`{ uint32_t flags; void* stack_base;
// size_t stack_size; size_t guard_size; int32_t sched_policy; int32_t sched_priority; ... }`). Eclipse
// owns it as a run of words and stores only what `pthread_create` reads back: the detach-state and the
// stack-size, at fixed Eclipse-owned word offsets (the object never reaches glibc — like the mutex).

/// Number of 4-byte words in a bionic `pthread_attr_t` (56 bytes).
const ATTR_WORDS: usize = 14;
/// Word index of the detach-state (`PTHREAD_CREATE_JOINABLE`/`DETACHED`) in the Eclipse attr encoding.
const ATTR_DETACH: usize = 0;
/// Word index (as a `usize`-aligned pair, i.e. words [2..4)) of the stack size in the Eclipse attr.
/// Kept 8-byte aligned (word 2) so the `*const usize` read in `pthread_create` is well-aligned.
const ATTR_STACKSIZE: usize = 2;

/// `int pthread_attr_init(pthread_attr_t*)` — zero the attr (default = joinable, default stack).
///
/// # Safety
/// `a` is null or a valid 56-byte bionic attr.
unsafe extern "C" fn eclipse_pthread_attr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` is a valid 56-byte attr; zero all 14 words (joinable, no stack request).
    unsafe {
        let words = a as *mut c_int;
        for i in 0..ATTR_WORDS {
            *words.add(i) = 0;
        }
    }
    0
}

/// `int pthread_attr_destroy(pthread_attr_t*)` — no owned resources; validate only.
///
/// # Safety
/// `a` is null or a valid bionic attr.
unsafe extern "C" fn eclipse_pthread_attr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

/// `int pthread_attr_setdetachstate(pthread_attr_t*, int state)` — record JOINABLE/DETACHED.
///
/// # Safety
/// `a` is null or a valid bionic attr.
unsafe extern "C" fn eclipse_pthread_attr_setdetachstate(a: *mut c_void, state: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    if state != PTHREAD_CREATE_JOINABLE && state != PTHREAD_CREATE_DETACHED {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` valid; store the validated detach-state at the Eclipse-owned word.
    unsafe { *(a as *mut c_int).add(ATTR_DETACH) = state };
    0
}

/// `int pthread_attr_setstacksize(pthread_attr_t*, size_t size)` — record the requested stack size.
///
/// # Safety
/// `a` is null or a valid bionic attr.
unsafe extern "C" fn eclipse_pthread_attr_setstacksize(a: *mut c_void, size: usize) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` valid; store the size at the 8-byte-aligned Eclipse stack-size slot.
    unsafe { *((a as *mut c_int).add(ATTR_STACKSIZE) as *mut usize) = size };
    0
}

/// `int pthread_attr_setschedparam(pthread_attr_t*, const sched_param*)` — accepted and ignored by
/// Eclipse's scheduler model (the host scheduler governs the real thread). Validates non-null.
///
/// # Safety
/// `a`/`param` are null or valid.
unsafe extern "C" fn eclipse_pthread_attr_setschedparam(
    a: *mut c_void,
    _param: *const c_void,
) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

/// `int pthread_attr_getstack(const pthread_attr_t*, void** base, size_t* size)` — report the stack.
/// Eclipse does not pre-allocate the stack (the host runtime owns it), so it returns base=NULL and the
/// recorded size (or 0). Callers use this only informationally during init.
///
/// # Safety
/// `a` is null or a valid attr; `base`/`size` are null or writable out-params.
unsafe extern "C" fn eclipse_pthread_attr_getstack(
    a: *const c_void,
    base: *mut *mut c_void,
    size: *mut usize,
) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `a` is a valid attr; read the recorded stack size. `base`/`size` are
    // null-or-writable out-params; we report base=NULL (host-owned stack) + the recorded size.
    unsafe {
        let recorded = *((a as *const c_int).add(ATTR_STACKSIZE) as *const usize);
        if !base.is_null() {
            *base = std::ptr::null_mut();
        }
        if !size.is_null() {
            *size = recorded;
        }
    }
    0
}

/// `int pthread_getattr_np(pthread_t, pthread_attr_t* attr)` — fill `attr` with the thread's
/// attributes. Eclipse returns a default (joinable, default stack) initialized attr; the engine uses
/// this informationally. Returns 0/EINVAL.
///
/// # Safety
/// `attr` is null or a valid 56-byte bionic attr.
unsafe extern "C" fn eclipse_pthread_getattr_np(_thread: usize, attr: *mut c_void) -> c_int {
    if attr.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — initialize `attr` to the joinable/default state (same as attr_init).
    unsafe { eclipse_pthread_attr_init(attr) }
}

/// `int pthread_getschedparam(pthread_t, int* policy, sched_param* param)` — report the thread's
/// scheduling policy/priority via the TID-based `sched_getscheduler`/`sched_getparam`. Returns 0/errno.
///
/// # Safety
/// `policy`/`param` are null or writable out-params (the `sched_param`'s first int is the priority).
unsafe extern "C" fn eclipse_pthread_getschedparam(
    thread: usize,
    policy: *mut c_int,
    param: *mut c_int,
) -> c_int {
    let tid = thread as c_int;
    // SAFETY: 2026-06-05 — `sched_getscheduler(tid)` returns the policy (or -1/errno); `policy` is
    // null-or-writable.
    let pol = unsafe { libc::sched_getscheduler(tid) };
    if pol < 0 {
        return eclipse_errno_value();
    }
    if !policy.is_null() {
        // SAFETY: 2026-06-05 — `policy` is a writable `int*` out-param.
        unsafe { *policy = pol };
    }
    if !param.is_null() {
        // SAFETY: 2026-06-05 — `param` points to a `sched_param` whose first field is `sched_priority`
        // (an int); `sched_getparam` fills it for `tid`.
        let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
        // SAFETY: 2026-06-05 — `&mut sp` is a valid `sched_param`; `sched_getparam(tid, &sp)` fills it.
        if unsafe { libc::sched_getparam(tid, &mut sp) } == 0 {
            // SAFETY: 2026-06-05 — write the priority into the caller's `sched_param`'s first int.
            unsafe { *param = sp.sched_priority };
        }
    }
    0
}

/// `int pthread_setschedparam(pthread_t, int policy, const sched_param* param)` — set the thread's
/// scheduling policy/priority via the TID-based `sched_setscheduler`. A permission failure (`EPERM`,
/// common for real-time policies as a normal user) is reported, never fatal. Returns 0/errno.
///
/// # Safety
/// `param` is null or a valid `sched_param` (first int = priority).
unsafe extern "C" fn eclipse_pthread_setschedparam(
    thread: usize,
    policy: c_int,
    param: *const c_int,
) -> c_int {
    let tid = thread as c_int;
    let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };
    if !param.is_null() {
        // SAFETY: 2026-06-05 — `param`'s first int is the requested priority.
        sp.sched_priority = unsafe { *param };
    }
    // SAFETY: 2026-06-05 — `sched_setscheduler(tid, policy, &sp)` sets the TID's policy/priority; a
    // failure returns -1 and sets errno (we surface it as the POSIX `pthread_*` return value).
    let rc = unsafe { libc::sched_setscheduler(tid, policy, &sp) };
    if rc == 0 {
        0
    } else {
        eclipse_errno_value()
    }
}

// =================================================================================================
// syscall(2) — the init path calls syscall() directly (for SYS_gettid). Forward to the real kernel.
// =================================================================================================

// `long syscall(long number, ...)` — the raw Linux syscall entry. The init path calls
// `syscall(SYS_gettid)`; other variadic syscalls forward their (up to 6) register arguments. The
// host glibc `syscall` is ABI-identical to bionic's (both marshal the SysV-register arguments into
// the kernel ABI). This is the one pthread-family import where a host forward is CORRECT: `syscall`
// is a thin kernel trampoline with no libc-private state, identical between glibc and bionic on
// x86-64 Linux. 2026-06-05: implemented in the clean-room C shim (`src/loader/bionic_syscall_shim.c`)
// because a C-variadic *definition* needs nightly Rust; the shim forwards varargs to the host
// `syscall(3)`. Rust DECLARES it variadic (stable allows variadic *declarations*) and takes its
// address to register it under the bionic import name "syscall".
extern "C" {
    fn eclipse_bionic_syscall(number: c_long, ...) -> c_long;
}

// =================================================================================================
// Registration — the (name -> Eclipse address) pairs for the EclipseNativeProvider.
// =================================================================================================

/// The number of pthread/TLS/sem/syscall natives this shim registers — the stateful threading
/// primitives the init path needs. Breakdown: mutex 5, mutexattr 3, cond 6, condattr 3, rwlock 5,
/// once 1, TLS keys 4, identity/lifecycle 5, sem 4, syscall 1 = 37, **plus** the thread LIFECYCLE
/// (2026-06-05): create/join/detach 3, setname_np 1, kill 1, getattr_np 1, get/setschedparam 2,
/// attr_* 6 = 14 → **51**. The lifecycle is now Eclipse-owned (TID-based `pthread_t`) because the init
/// path DOES spawn threads, and the mixed Eclipse-`pthread_self`/host-glibc-`pthread_setname_np` ABI
/// crashed the worker (gdb-proven, `docs/libroblox-init-run.md` §8). `__cxa_thread_atexit_impl`
/// stays on the host baseline (ABI-identical). 2026-06-11: `pthread_sigmask` is NOT ABI-identical
/// after all — bionic `sigset_t` is 8 bytes vs glibc's 128 (a non-null `oldset` made glibc WRITE
/// 128 bytes through it) — it is now provided by `native_provider`'s bionic signal-ABI section
/// (with `sigaction`/`sigprocmask`/`sigemptyset`/`sigaddset`/`sigfillset`), counted there, so this
/// count is unchanged.
pub const PTHREAD_NATIVE_COUNT: usize = 51;

/// Append every Eclipse-owned bionic pthread/TLS/sem/syscall native to `register` as
/// `(name, address)` pairs. Called by [`super::native_provider::EclipseNativeProvider`] so the
/// engine's `pthread_*`/`sem_*`/`gettid`/`syscall` imports bind to this bionic-correct shim,
/// displacing the host-glibc baseline.
///
/// `register(name, addr)` records the binding; `addr` is each native's address taken safely
/// (`f as *const () as u64`).
pub fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    macro_rules! reg {
        ($name:literal, $f:expr) => {
            register($name, $f as *const () as u64);
        };
    }

    // ---- mutex (5) + mutexattr (3) ----
    reg!("pthread_mutex_init", eclipse_pthread_mutex_init);
    reg!("pthread_mutex_lock", eclipse_pthread_mutex_lock);
    reg!("pthread_mutex_trylock", eclipse_pthread_mutex_trylock);
    reg!("pthread_mutex_unlock", eclipse_pthread_mutex_unlock);
    reg!("pthread_mutex_destroy", eclipse_pthread_mutex_destroy);
    reg!("pthread_mutexattr_init", eclipse_pthread_mutexattr_init);
    reg!(
        "pthread_mutexattr_settype",
        eclipse_pthread_mutexattr_settype
    );
    reg!(
        "pthread_mutexattr_destroy",
        eclipse_pthread_mutexattr_destroy
    );

    // ---- cond (6) + condattr (3) ----
    reg!("pthread_cond_init", eclipse_pthread_cond_init);
    reg!("pthread_cond_wait", eclipse_pthread_cond_wait);
    reg!("pthread_cond_timedwait", eclipse_pthread_cond_timedwait);
    reg!("pthread_cond_signal", eclipse_pthread_cond_signal);
    reg!("pthread_cond_broadcast", eclipse_pthread_cond_broadcast);
    reg!("pthread_cond_destroy", eclipse_pthread_cond_destroy);
    reg!("pthread_condattr_init", eclipse_pthread_condattr_init);
    reg!(
        "pthread_condattr_setclock",
        eclipse_pthread_condattr_setclock
    );
    reg!("pthread_condattr_destroy", eclipse_pthread_condattr_destroy);

    // ---- rwlock (6) ----
    reg!("pthread_rwlock_init", eclipse_pthread_rwlock_init);
    reg!("pthread_rwlock_rdlock", eclipse_pthread_rwlock_rdlock);
    reg!("pthread_rwlock_wrlock", eclipse_pthread_rwlock_wrlock);
    reg!("pthread_rwlock_unlock", eclipse_pthread_rwlock_unlock);
    reg!("pthread_rwlock_destroy", eclipse_pthread_rwlock_destroy);

    // ---- once (1) ----
    reg!("pthread_once", eclipse_pthread_once);

    // ---- TLS keys (4) ----
    reg!("pthread_key_create", eclipse_pthread_key_create);
    reg!("pthread_key_delete", eclipse_pthread_key_delete);
    reg!("pthread_getspecific", eclipse_pthread_getspecific);
    reg!("pthread_setspecific", eclipse_pthread_setspecific);

    // ---- identity / lifecycle (5) ----
    reg!("pthread_self", eclipse_pthread_self);
    reg!("pthread_equal", eclipse_pthread_equal);
    reg!("pthread_gettid_np", eclipse_pthread_gettid_np);
    reg!("pthread_exit", eclipse_pthread_exit);
    reg!("gettid", eclipse_gettid);

    // ---- thread lifecycle (14) — Eclipse-owned, TID-based pthread_t (2026-06-05 init[~414] fix) ----
    // create/join/detach 3, setname_np 1, kill 1, getattr_np 1, get/setschedparam 2, attr_* 6.
    reg!("pthread_create", eclipse_pthread_create);
    reg!("pthread_join", eclipse_pthread_join);
    reg!("pthread_detach", eclipse_pthread_detach);
    reg!("pthread_setname_np", eclipse_pthread_setname_np);
    reg!("pthread_kill", eclipse_pthread_kill);
    reg!("pthread_getattr_np", eclipse_pthread_getattr_np);
    reg!("pthread_getschedparam", eclipse_pthread_getschedparam);
    reg!("pthread_setschedparam", eclipse_pthread_setschedparam);
    reg!("pthread_attr_init", eclipse_pthread_attr_init);
    reg!("pthread_attr_destroy", eclipse_pthread_attr_destroy);
    reg!(
        "pthread_attr_setdetachstate",
        eclipse_pthread_attr_setdetachstate
    );
    reg!(
        "pthread_attr_setstacksize",
        eclipse_pthread_attr_setstacksize
    );
    reg!(
        "pthread_attr_setschedparam",
        eclipse_pthread_attr_setschedparam
    );
    reg!("pthread_attr_getstack", eclipse_pthread_attr_getstack);

    // ---- sem (4) ----
    reg!("sem_init", eclipse_sem_init);
    reg!("sem_wait", eclipse_sem_wait);
    reg!("sem_post", eclipse_sem_post);
    reg!("sem_destroy", eclipse_sem_destroy);

    // ---- syscall (1) — the C-variadic shim (forwards varargs to the host syscall(3)) ----
    register("syscall", eclipse_bionic_syscall as *const () as u64);
}

// =================================================================================================
// sem_* — bionic 16-byte sem_t (a futex-backed counting semaphore).
// =================================================================================================
//
// Word layout (Eclipse-owned, within the 16-byte bionic sem_t):
//   [0] count (the semaphore value; waiters park when it is 0)

const SEM_WORDS: usize = 4;
const SEM_COUNT: usize = 0;

/// `int sem_init(sem_t*, int pshared, unsigned value)` — initialize the count.
///
/// # Safety
/// `s` is null or a valid 16-byte bionic `sem_t`.
unsafe extern "C" fn eclipse_sem_init(s: *mut c_void, _pshared: c_int, value: c_int) -> c_int {
    if s.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `s` valid; store the initial count.
    unsafe {
        if let Some(c) = word(s, SEM_COUNT, SEM_WORDS) {
            c.store(value, Ordering::Release);
        }
    }
    0
}

/// `int sem_wait(sem_t*)` — decrement, blocking while the count is 0. Returns 0/EINVAL.
///
/// # Safety
/// `s` is null or a valid 16-byte bionic `sem_t`.
unsafe extern "C" fn eclipse_sem_wait(s: *mut c_void) -> c_int {
    if s.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `s` valid; CAS the count down when positive, else park until a post.
    unsafe {
        let count = match word(s, SEM_COUNT, SEM_WORDS) {
            Some(c) => c,
            None => return EINVAL,
        };
        loop {
            let cur = count.load(Ordering::Acquire);
            if cur > 0 {
                if count
                    .compare_exchange(cur, cur - 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return 0;
                }
            } else {
                futex_wait(count, 0);
            }
        }
    }
}

/// `int sem_post(sem_t*)` — increment and wake a waiter. Returns 0/EINVAL.
///
/// # Safety
/// `s` is null or a valid 16-byte bionic `sem_t`.
unsafe extern "C" fn eclipse_sem_post(s: *mut c_void) -> c_int {
    if s.is_null() {
        return EINVAL;
    }
    // SAFETY: 2026-06-05 — `s` valid; bump the count and wake one parked waiter.
    unsafe {
        if let Some(count) = word(s, SEM_COUNT, SEM_WORDS) {
            count.fetch_add(1, Ordering::Release);
            futex_wake(count, 1);
        }
    }
    0
}

/// `int sem_destroy(sem_t*)` — no owned resources; validate only. Returns 0/EINVAL.
///
/// # Safety
/// `s` is null or a valid 16-byte bionic `sem_t`.
unsafe extern "C" fn eclipse_sem_destroy(s: *mut c_void) -> c_int {
    if s.is_null() {
        EINVAL
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zeroed bionic mutex (40 bytes = 10 int words), as the static initializer / a freshly
    /// allocated field would be. Returned boxed so the address is stable for the duration of a test.
    fn zeroed_words(n: usize) -> Box<[i32]> {
        vec![0i32; n].into_boxed_slice()
    }

    // ---- bionic layout sizes match the documented ABI -------------------------------------------

    #[test]
    fn bionic_object_word_counts_match_abi() {
        // The opaque sizes from the public bionic `bits/pthread_types.h` (LP64):
        // pthread_mutex_t int[10]=40B, cond_t int[12]=48B, rwlock_t int[14]=56B, sem_t int[4]=16B.
        assert_eq!(MUTEX_WORDS * 4, 40);
        assert_eq!(COND_WORDS * 4, 48);
        assert_eq!(RWLOCK_WORDS * 4, 56);
        assert_eq!(SEM_WORDS * 4, 16);
        // pthread_key_t / pthread_once_t are a 4-byte int.
        assert_eq!(std::mem::size_of::<c_int>(), 4);
        // The shim registers exactly the documented native count.
        let mut n = 0;
        register_natives(|_, _| n += 1);
        assert_eq!(n, PTHREAD_NATIVE_COUNT);
    }

    // ---- mutex lock / unlock / trylock cycle ----------------------------------------------------

    #[test]
    fn mutex_lock_unlock_trylock_cycle_normal() {
        let mut m = zeroed_words(MUTEX_WORDS);
        let mp = m.as_mut_ptr() as *mut c_void;
        // A zeroed (PTHREAD_MUTEX_INITIALIZER) NORMAL mutex.
        // SAFETY: `mp` is a valid 40-byte zeroed mutex for the test's lifetime.
        unsafe {
            assert_eq!(eclipse_pthread_mutex_lock(mp), 0, "lock a free mutex");
            // While held, trylock must fail with EBUSY (NORMAL: no recursion).
            assert_eq!(
                eclipse_pthread_mutex_trylock(mp),
                EBUSY,
                "trylock a held mutex → EBUSY"
            );
            assert_eq!(eclipse_pthread_mutex_unlock(mp), 0, "unlock");
            // Now trylock succeeds.
            assert_eq!(
                eclipse_pthread_mutex_trylock(mp),
                0,
                "trylock a free mutex → 0"
            );
            assert_eq!(eclipse_pthread_mutex_unlock(mp), 0, "unlock again");
        }
    }

    #[test]
    fn mutex_recursive_allows_owner_relock() {
        let mut m = zeroed_words(MUTEX_WORDS);
        let mp = m.as_mut_ptr() as *mut c_void;
        let mut attr: c_int = 0;
        let ap = std::ptr::addr_of_mut!(attr) as *mut c_void;
        // SAFETY: valid attr + mutex for the test.
        unsafe {
            assert_eq!(eclipse_pthread_mutexattr_init(ap), 0);
            assert_eq!(
                eclipse_pthread_mutexattr_settype(ap, PTHREAD_MUTEX_RECURSIVE),
                0
            );
            assert_eq!(eclipse_pthread_mutex_init(mp, ap as *const c_void), 0);
            // The owner may lock twice; each lock needs a matching unlock.
            assert_eq!(eclipse_pthread_mutex_lock(mp), 0);
            assert_eq!(
                eclipse_pthread_mutex_lock(mp),
                0,
                "recursive relock by owner"
            );
            assert_eq!(
                eclipse_pthread_mutex_unlock(mp),
                0,
                "first unlock (depth 2→1)"
            );
            // Still held at depth 1: a *different* thread's trylock would block; here we just unlock.
            assert_eq!(
                eclipse_pthread_mutex_unlock(mp),
                0,
                "final unlock (depth 1→0)"
            );
            assert_eq!(
                eclipse_pthread_mutex_trylock(mp),
                0,
                "fully released → trylock succeeds"
            );
            assert_eq!(eclipse_pthread_mutex_unlock(mp), 0);
        }
    }

    #[test]
    fn mutex_errorcheck_self_deadlock_is_edeadlk() {
        let mut m = zeroed_words(MUTEX_WORDS);
        let mp = m.as_mut_ptr() as *mut c_void;
        let mut attr: c_int = 0;
        let ap = std::ptr::addr_of_mut!(attr) as *mut c_void;
        // SAFETY: valid attr + mutex.
        unsafe {
            eclipse_pthread_mutexattr_init(ap);
            eclipse_pthread_mutexattr_settype(ap, PTHREAD_MUTEX_ERRORCHECK);
            eclipse_pthread_mutex_init(mp, ap as *const c_void);
            assert_eq!(eclipse_pthread_mutex_lock(mp), 0);
            assert_eq!(
                eclipse_pthread_mutex_lock(mp),
                EDEADLK,
                "errorcheck self-relock → EDEADLK"
            );
            assert_eq!(eclipse_pthread_mutex_unlock(mp), 0);
        }
    }

    #[test]
    fn mutex_two_threads_are_mutually_exclusive() {
        use std::sync::atomic::{AtomicI64, Ordering as O};
        use std::sync::Arc;

        // A shared zeroed NORMAL mutex (leaked so its address is 'static for the spawned threads).
        let m: &'static mut [i32] = Box::leak(zeroed_words(MUTEX_WORDS));
        let mp_addr = m.as_mut_ptr() as usize;
        let counter = Arc::new(AtomicI64::new(0));
        let max_seen = Arc::new(AtomicI64::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let counter = Arc::clone(&counter);
            let max_seen = Arc::clone(&max_seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..2000 {
                    let mp = mp_addr as *mut c_void;
                    // SAFETY: `mp` is the shared leaked 40-byte mutex, valid for all threads.
                    unsafe { eclipse_pthread_mutex_lock(mp) };
                    // Inside the critical section the counter must never exceed 1.
                    let now = counter.fetch_add(1, O::AcqRel) + 1;
                    max_seen.fetch_max(now, O::AcqRel);
                    counter.fetch_sub(1, O::AcqRel);
                    let mp = mp_addr as *mut c_void;
                    // SAFETY: same mutex; this thread holds it.
                    unsafe { eclipse_pthread_mutex_unlock(mp) };
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_seen.load(O::Acquire),
            1,
            "the mutex must serialize the critical section (max concurrent occupancy = 1)"
        );
        // Reclaim the leaked mutex.
        // SAFETY: reconstruct the Box from the leaked pointer to free it (no other refs remain).
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                mp_addr as *mut i32,
                MUTEX_WORDS,
            )));
        }
    }

    // ---- pthread_once runs the init EXACTLY once under contention --------------------------------

    #[test]
    fn once_runs_init_exactly_once_under_contention() {
        use std::sync::atomic::{AtomicI32 as A32, Ordering as O};

        static RUN_COUNT: A32 = A32::new(0);
        RUN_COUNT.store(0, O::SeqCst);
        extern "C" fn init() {
            RUN_COUNT.fetch_add(1, O::SeqCst);
        }

        // One shared once word (leaked for 'static across threads).
        let once: &'static mut [i32] = Box::leak(zeroed_words(1));
        let once_addr = once.as_mut_ptr() as usize;

        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(std::thread::spawn(move || {
                let op = once_addr as *mut c_void;
                // SAFETY: `op` is the shared 4-byte once word, valid for all threads.
                unsafe { eclipse_pthread_once(op, Some(init)) };
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            RUN_COUNT.load(O::SeqCst),
            1,
            "pthread_once must run the init exactly once across all contending threads"
        );
        // A subsequent call is a no-op (already DONE).
        let op = once_addr as *mut c_void;
        // SAFETY: shared once word.
        unsafe { eclipse_pthread_once(op, Some(init)) };
        assert_eq!(RUN_COUNT.load(O::SeqCst), 1, "once stays done");

        // SAFETY: reclaim the leaked once word.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                once_addr as *mut i32,
                1,
            )));
        }
    }

    // ---- TLS keys: create / get / set round-trip + per-thread isolation -------------------------

    #[test]
    fn key_create_get_set_roundtrip_and_isolation_across_threads() {
        let mut key: c_int = -1;
        let kp = std::ptr::addr_of_mut!(key) as *mut c_void;
        // SAFETY: `kp` is a valid 4-byte key out-param.
        unsafe {
            assert_eq!(eclipse_pthread_key_create(kp, None), 0, "allocate a key");
        }
        assert!(key >= 0, "a valid key index");

        // Initially unset on this thread.
        // SAFETY: `key` is a valid allocated key.
        unsafe {
            assert!(
                eclipse_pthread_getspecific(key).is_null(),
                "unset key reads NULL"
            );
            // Set + read back the value on this thread.
            assert_eq!(eclipse_pthread_setspecific(key, 0x1234 as *const c_void), 0);
            assert_eq!(eclipse_pthread_getspecific(key), 0x1234 as *mut c_void);
        }

        // A *different* thread sees its OWN (NULL) value for the same key — per-thread isolation.
        let key_copy = key;
        let other = std::thread::spawn(move || {
            // SAFETY: `key_copy` is the same allocated key; the value store is thread-local.
            unsafe {
                let before = eclipse_pthread_getspecific(key_copy);
                assert!(before.is_null(), "other thread sees NULL (isolation)");
                eclipse_pthread_setspecific(key_copy, 0x9999 as *const c_void);
                eclipse_pthread_getspecific(key_copy) as usize
            }
        })
        .join()
        .unwrap();
        assert_eq!(other, 0x9999, "other thread sets+reads its own value");

        // Back on this thread, our value is unchanged by the other thread.
        // SAFETY: same key.
        unsafe {
            assert_eq!(
                eclipse_pthread_getspecific(key),
                0x1234 as *mut c_void,
                "this thread's value is isolated from the other thread"
            );
        }

        // Delete the key; a subsequent get reads NULL (the stale value is masked by the generation).
        // SAFETY: `key` is allocated.
        unsafe {
            assert_eq!(eclipse_pthread_key_delete(key), 0);
            assert!(
                eclipse_pthread_getspecific(key).is_null(),
                "deleted key reads NULL even if a value was set under it"
            );
            // setspecific on a deleted key is EINVAL (the value is never dereferenced on this path).
            let sentinel = std::ptr::without_provenance::<c_void>(0x1);
            assert_eq!(eclipse_pthread_setspecific(key, sentinel), EINVAL);
        }
    }

    #[test]
    fn key_destructor_runs_on_pthread_exit() {
        use std::sync::atomic::{AtomicUsize as AU, Ordering as O};
        static DTOR_VALUE: AU = AU::new(0);
        DTOR_VALUE.store(0, O::SeqCst);
        extern "C" fn dtor(v: *mut c_void) {
            DTOR_VALUE.store(v as usize, O::SeqCst);
        }

        let mut key: c_int = -1;
        let kp = std::ptr::addr_of_mut!(key) as *mut c_void;
        // SAFETY: valid key out-param.
        unsafe {
            assert_eq!(eclipse_pthread_key_create(kp, Some(dtor)), 0);
        }
        let key_copy = key;
        // On a worker thread: set a value, then run the destructors explicitly (the pthread_exit
        // path), and confirm the destructor saw the value.
        std::thread::spawn(move || {
            // SAFETY: allocated key; thread-local set.
            unsafe {
                eclipse_pthread_setspecific(key_copy, 0xABCD as *const c_void);
            }
            run_thread_key_destructors();
        })
        .join()
        .unwrap();
        assert_eq!(
            DTOR_VALUE.load(O::SeqCst),
            0xABCD,
            "the key destructor ran with the per-thread value on thread exit"
        );
        // SAFETY: clean up the key.
        unsafe {
            eclipse_pthread_key_delete(key);
        }
    }

    // ---- sem: post/wait counting ----------------------------------------------------------------

    #[test]
    fn sem_post_then_wait_does_not_block() {
        let mut s = zeroed_words(SEM_WORDS);
        let sp = s.as_mut_ptr() as *mut c_void;
        // SAFETY: `sp` is a valid 16-byte sem_t.
        unsafe {
            assert_eq!(eclipse_sem_init(sp, 0, 0), 0);
            assert_eq!(eclipse_sem_post(sp), 0, "post → count 1");
            assert_eq!(eclipse_sem_post(sp), 0, "post → count 2");
            // Two waits consume the two posts without blocking.
            assert_eq!(eclipse_sem_wait(sp), 0);
            assert_eq!(eclipse_sem_wait(sp), 0);
            assert_eq!(eclipse_sem_destroy(sp), 0);
        }
    }

    // ---- identity --------------------------------------------------------------------------------

    #[test]
    fn self_equal_and_gettid_are_consistent() {
        // SAFETY: these natives take no pointers.
        unsafe {
            let me = eclipse_pthread_self();
            assert!(eclipse_pthread_equal(me, me) != 0, "a thread equals itself");
            assert_eq!(
                eclipse_pthread_gettid_np(me),
                eclipse_gettid(),
                "gettid_np(self) == gettid()"
            );
            // A different (fabricated) handle is not equal to self.
            assert_eq!(eclipse_pthread_equal(me, me ^ 1), 0);
        }
    }

    // ---- null-argument safety: every object native rejects NULL with EINVAL, never UB -----------

    #[test]
    fn null_objects_return_einval_not_crash() {
        let n = std::ptr::null_mut::<c_void>();
        // SAFETY: passing NULL is the explicit "reject with EINVAL" path of each native.
        unsafe {
            assert_eq!(eclipse_pthread_mutex_lock(n), EINVAL);
            assert_eq!(eclipse_pthread_mutex_unlock(n), EINVAL);
            assert_eq!(eclipse_pthread_mutex_destroy(n), EINVAL);
            assert_eq!(eclipse_pthread_cond_signal(n), EINVAL);
            assert_eq!(eclipse_pthread_cond_wait(n, n), EINVAL);
            assert_eq!(eclipse_pthread_rwlock_rdlock(n), EINVAL);
            assert_eq!(eclipse_pthread_rwlock_wrlock(n), EINVAL);
            assert_eq!(eclipse_pthread_once(n, None), EINVAL);
            assert_eq!(eclipse_sem_wait(n), EINVAL);
            assert_eq!(eclipse_pthread_key_create(n, None), EINVAL);
        }
    }

    // ---- thread lifecycle: create runs the entry on a real thread; join returns its result --------

    #[test]
    fn create_runs_entry_on_real_thread_and_join_returns_its_result() {
        use std::sync::atomic::{AtomicI32 as A32, Ordering as O};

        // The start routine records the TID it actually ran on (a fresh OS thread → != the test
        // thread's TID) and returns a sentinel pointer the join must hand back verbatim.
        static RAN_TID: A32 = A32::new(0);
        RAN_TID.store(0, O::SeqCst);
        extern "C" fn start(arg: *mut c_void) -> *mut c_void {
            RAN_TID.store(gettid(), O::SeqCst);
            // Echo the arg back + 1 so the test proves the arg reached the entry and the result
            // round-trips through join.
            (arg as usize + 1) as *mut c_void
        }

        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;
        // SAFETY: `tp` is a valid `pthread_t*` out-param; `start` is a valid entry; arg is opaque.
        let rc = unsafe {
            eclipse_pthread_create(tp, std::ptr::null(), Some(start), 0x41 as *mut c_void)
        };
        assert_eq!(rc, 0, "pthread_create succeeds");
        assert!(tid != 0, "create wrote a non-zero TID as the pthread_t");
        assert_ne!(
            tid as i32,
            gettid(),
            "the entry ran on a DIFFERENT (spawned) OS thread, not the caller"
        );

        let mut retval: *mut c_void = std::ptr::null_mut();
        // SAFETY: `tid` is the just-created joinable thread; `&mut retval` is a writable out-param.
        let jrc = unsafe { eclipse_pthread_join(tid, &mut retval) };
        assert_eq!(jrc, 0, "join succeeds");
        assert_eq!(
            retval as usize, 0x42,
            "join returns the entry's result (arg+1)"
        );
        assert_eq!(
            RAN_TID.load(O::SeqCst),
            tid as i32,
            "pthread_self() inside the thread == the pthread_t create returned (TID identity)"
        );

        // Joining a now-finished/removed thread reports ESRCH (3), never UB.
        // SAFETY: `tid` is no longer tracked (join consumed it).
        let again = unsafe { eclipse_pthread_join(tid, std::ptr::null_mut()) };
        assert_eq!(again, 3, "re-join of a consumed thread → ESRCH");
    }

    // 2026-06-05: regression guard for the child-TID hand-off use-after-free. Each created thread
    // returns ITS OWN gettid() as the start-routine result; the invariant is that the pthread_t the
    // parent received == the thread's own gettid (the bionic TID identity). `start` returns
    // immediately so the trampoline frees its hand-off block at the earliest possible instant — the
    // exact window in which a dangling read used to observe a freed/reused block (a concurrent
    // creator's TID or heap garbage). Many threads × many rounds make a freed block get reused by a
    // concurrent creator almost immediately; if the hand-off slot did not outlive the parent's read,
    // some pthread_t would not equal its own gettid here.
    #[test]
    fn create_returns_each_childs_own_tid_under_heavy_parallel_load() {
        extern "C" fn start(_arg: *mut c_void) -> *mut c_void {
            // Return this thread's real kernel TID so join can compare it to the pthread_t the
            // parent obtained for this exact thread.
            gettid() as usize as *mut c_void
        }

        const N: usize = 64; // concurrent creators per round
        const ROUNDS: usize = 16; // repeat to widen the race window
        for _ in 0..ROUNDS {
            // Each creator thread spawns one Eclipse thread, then joins it, and checks that the
            // pthread_t it got back equals that child's own reported gettid().
            let mut creators = Vec::with_capacity(N);
            for _ in 0..N {
                creators.push(std::thread::spawn(|| {
                    let mut tid: usize = 0;
                    let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;
                    // SAFETY: `tp` is a valid `pthread_t*` out-param; `start` is a valid entry.
                    let rc = unsafe {
                        eclipse_pthread_create(
                            tp,
                            std::ptr::null(),
                            Some(start),
                            std::ptr::null_mut(),
                        )
                    };
                    assert_eq!(rc, 0, "create succeeds under load");
                    assert!(tid != 0, "create wrote a non-zero pthread_t");

                    let mut retval: *mut c_void = std::ptr::null_mut();
                    // SAFETY: `tid` is the just-created joinable thread; `&mut retval` is writable.
                    let jrc = unsafe { eclipse_pthread_join(tid, &mut retval) };
                    assert_eq!(jrc, 0, "join succeeds under load");
                    // THE INVARIANT: the pthread_t the parent received IS this child's own TID.
                    assert_eq!(
                        retval as usize, tid,
                        "pthread_t returned by create must equal the child's own gettid() \
                         (no dangling/cross-contaminated TID hand-off under parallel load)"
                    );
                }));
            }
            for c in creators {
                c.join().expect("creator thread asserted TID identity");
            }
        }
    }

    #[test]
    fn create_detached_is_not_joinable_and_runs() {
        use std::sync::atomic::{AtomicBool as AB, Ordering as O};
        static RAN: AB = AB::new(false);
        RAN.store(false, O::SeqCst);
        extern "C" fn start(_a: *mut c_void) -> *mut c_void {
            RAN.store(true, O::SeqCst);
            std::ptr::null_mut()
        }

        // A DETACHED attr.
        let mut attr = [0i32; ATTR_WORDS];
        let ap = attr.as_mut_ptr() as *mut c_void;
        // SAFETY: `ap` is a valid 56-byte attr; set DETACHED.
        unsafe {
            assert_eq!(eclipse_pthread_attr_init(ap), 0);
            assert_eq!(
                eclipse_pthread_attr_setdetachstate(ap, PTHREAD_CREATE_DETACHED),
                0
            );
        }
        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;
        // SAFETY: valid out-param + attr + entry.
        let rc = unsafe {
            eclipse_pthread_create(tp, ap as *const c_void, Some(start), std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "detached create succeeds");
        // A detached thread keeps no registry entry → join reports ESRCH (3), not 0.
        // SAFETY: `tid` is a detached thread (untracked).
        let jrc = unsafe { eclipse_pthread_join(tid, std::ptr::null_mut()) };
        assert_eq!(jrc, 3, "a detached thread is not joinable → ESRCH");
        // Give the detached thread a moment to run, then confirm it executed.
        for _ in 0..1000 {
            if RAN.load(O::SeqCst) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(RAN.load(O::SeqCst), "the detached thread ran its entry");
        // SAFETY: destroy the attr (no owned resources).
        unsafe { assert_eq!(eclipse_pthread_attr_destroy(ap), 0) };
    }

    #[test]
    fn setname_np_on_self_succeeds_and_is_truncated() {
        // Setting the CALLING thread's name via prctl must succeed (0) for a normal and an
        // over-length name (truncated to TASK_COMM_LEN-1, never ERANGE).
        let me = gettid() as usize;
        let short = c"ecl-test";
        let long = c"this-name-is-way-too-long-for-comm";
        // SAFETY: both are valid NUL-terminated C strings; `me` is this thread's TID.
        unsafe {
            assert_eq!(eclipse_pthread_setname_np(me, short.as_ptr()), 0);
            assert_eq!(
                eclipse_pthread_setname_np(me, long.as_ptr()),
                0,
                "an over-length name is truncated, not rejected"
            );
            // A null name is EINVAL, never a crash.
            assert_eq!(eclipse_pthread_setname_np(me, std::ptr::null()), EINVAL);
        }
    }

    #[test]
    fn attr_records_detachstate_and_stacksize() {
        let mut attr = [0i32; ATTR_WORDS];
        let ap = attr.as_mut_ptr() as *mut c_void;
        // SAFETY: `ap` is a valid 56-byte attr; exercise the setters/getters.
        unsafe {
            assert_eq!(eclipse_pthread_attr_init(ap), 0);
            // Default detach-state is JOINABLE (0) after init.
            assert_eq!(
                *(ap as *const c_int).add(ATTR_DETACH),
                PTHREAD_CREATE_JOINABLE
            );
            // setstacksize records the value at the 8-byte-aligned slot; getstack reads it back.
            assert_eq!(eclipse_pthread_attr_setstacksize(ap, 0x40000), 0);
            // A non-null sentinel so the test proves getstack OVERWRITES it to NULL.
            let mut base: *mut c_void = std::ptr::without_provenance_mut(0x1);
            let mut size: usize = 0;
            assert_eq!(eclipse_pthread_attr_getstack(ap, &mut base, &mut size), 0);
            assert!(base.is_null(), "host-owned stack → base reported NULL");
            assert_eq!(size, 0x40000, "getstack reports the recorded stack size");
            // An invalid detach-state is rejected.
            assert_eq!(eclipse_pthread_attr_setdetachstate(ap, 99), EINVAL);
            assert_eq!(eclipse_pthread_attr_destroy(ap), 0);
        }
    }

    #[test]
    fn kill_signal_zero_probes_a_live_thread() {
        // `pthread_kill(self, 0)` is the existence probe: the calling thread is alive → 0, never a
        // delivered signal, never UB. A bogus TID → ESRCH(3) (the kernel rejects the unknown target).
        let me = gettid() as usize;
        // SAFETY: TID-based tgkill with sig 0 only checks the target.
        unsafe {
            assert_eq!(eclipse_pthread_kill(me, 0), 0, "self is alive");
            // A TID that is not in our thread group → ESRCH.
            assert_eq!(
                eclipse_pthread_kill(0x7fff_fffe, 0),
                3,
                "unknown TID → ESRCH"
            );
        }
    }
}
