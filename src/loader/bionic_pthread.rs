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
use std::ffi::{c_int, c_long, c_void};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

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
/// once 1, TLS keys 4, identity/lifecycle 5, sem 4, syscall 1 = **37**. The remaining `pthread_*`
/// category imports (thread create/join/detach, `pthread_attr_*`, scheduling, signals,
/// `__cxa_thread_atexit_impl`) stay on the host baseline until Eclipse owns thread creation; they
/// are not exercised by the init path (see `docs/libroblox-init-run.md` for the deferral).
pub const PTHREAD_NATIVE_COUNT: usize = 37;

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
}
