use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_int, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const EBUSY: c_int = 16;

const EINVAL: c_int = 22;

const EDEADLK: c_int = 35;

const EPERM: c_int = 1;

const ETIMEDOUT: c_int = 110;

const EINTR: c_int = 4;

const EAGAIN: c_int = 11;

const PTHREAD_MUTEX_NORMAL: c_int = 0;

const PTHREAD_MUTEX_RECURSIVE: c_int = 1;

const PTHREAD_MUTEX_ERRORCHECK: c_int = 2;

const CLOCK_MONOTONIC: c_int = 1;

const MUTEX_INIT_MAGIC: i32 = 0x6d75_7831u32 as i32;

const COND_INIT_MAGIC: i32 = 0x636e_6431u32 as i32;
const RWLOCK_INIT_MAGIC: i32 = 0x7277_6c31u32 as i32;

const ONCE_NOT_STARTED: i32 = 0;
const ONCE_IN_PROGRESS: i32 = 1;
const ONCE_DONE: i32 = 2;

const SYS_GETTID: c_long = 186;

const SYS_FUTEX: c_long = 202;
const FUTEX_WAIT: c_int = 0;
const FUTEX_WAKE: c_int = 1;

const FUTEX_WAIT_BITSET: c_int = 9;
const FUTEX_PRIVATE_FLAG: c_int = 128;

const FUTEX_CLOCK_REALTIME: c_int = 256;

const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

const SYS_TGKILL: c_long = 234;

const SYS_GETPID: c_long = 39;

fn gettid() -> i32 {
    unsafe { libc::syscall(SYS_GETTID) as i32 }
}

fn futex_wait(addr: &AtomicI32, expected: i32) {
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

fn futex_wait_until(
    addr: &AtomicI32,
    expected: i32,
    deadline: &libc::timespec,
    clock: c_int,
) -> c_int {
    let clock_flag = if clock == CLOCK_MONOTONIC {
        0
    } else {
        FUTEX_CLOCK_REALTIME
    };

    let result = unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | clock_flag,
            expected,
            deadline as *const libc::timespec,
            std::ptr::null::<c_void>(),
            FUTEX_BITSET_MATCH_ANY,
        )
    };
    if result == 0 {
        0
    } else {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(EINVAL)
    }
}

fn futex_wake(addr: &AtomicI32, count: c_int) {
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
            count,
        );
    }
}

const MUTEX_WORDS: usize = 10;

const COND_WORDS: usize = 12;

const RWLOCK_WORDS: usize = 14;

unsafe fn word<'a>(p: *mut c_void, i: usize, words: usize) -> Option<&'a AtomicI32> {
    debug_assert!(i < words);
    let _ = words;
    if p.is_null() {
        return None;
    }

    let base = p as *mut AtomicI32;
    Some(unsafe { &*base.add(i) })
}

const MUTEX_STATE: usize = 0;
const MUTEX_MAGIC: usize = 1;
const MUTEX_TYPE: usize = 2;
const MUTEX_OWNER: usize = 3;
const MUTEX_DEPTH: usize = 4;

unsafe fn mutex_ensure_init(p: *mut c_void) {
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

unsafe extern "C" fn eclipse_pthread_mutex_init(m: *mut c_void, attr: *const c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }

    let ty = if attr.is_null() {
        PTHREAD_MUTEX_NORMAL
    } else {
        (unsafe { *(attr as *const c_int) }) & 0x3
    };

    unsafe {
        std::ptr::write_bytes(m.cast::<u8>(), 0, MUTEX_WORDS * size_of::<c_int>());
        word(m, MUTEX_TYPE, MUTEX_WORDS)
            .unwrap()
            .store(ty, Ordering::Relaxed);
        word(m, MUTEX_MAGIC, MUTEX_WORDS)
            .unwrap()
            .store(MUTEX_INIT_MAGIC, Ordering::Release);
    }
    0
}

unsafe fn futex_lock_acquire(state: &AtomicI32) {
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }

    loop {
        let prev = state.swap(2, Ordering::Acquire);
        if prev == 0 {
            return;
        }

        futex_wait(state, 2);
    }
}

unsafe fn futex_lock_release(state: &AtomicI32) {
    if state.swap(0, Ordering::Release) == 2 {
        futex_wake(state, 1);
    }
}

unsafe fn mutex_lock_impl(m: *mut c_void, blocking: bool) -> c_int {
    if m.is_null() {
        return EINVAL;
    }

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

        if ty == PTHREAD_MUTEX_RECURSIVE || ty == PTHREAD_MUTEX_ERRORCHECK {
            let owner = word(m, MUTEX_OWNER, MUTEX_WORDS).unwrap();
            if owner.load(Ordering::Acquire) == tid {
                if ty == PTHREAD_MUTEX_RECURSIVE {
                    let depth = word(m, MUTEX_DEPTH, MUTEX_WORDS).unwrap();
                    depth.fetch_add(1, Ordering::Relaxed);
                    return 0;
                }
                return EDEADLK;
            }
        }

        if blocking {
            futex_lock_acquire(state);
        } else if state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return EBUSY;
        }

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

unsafe extern "C" fn eclipse_pthread_mutex_lock(m: *mut c_void) -> c_int {
    unsafe { mutex_lock_impl(m, true) }
}

unsafe extern "C" fn eclipse_pthread_mutex_trylock(m: *mut c_void) -> c_int {
    unsafe { mutex_lock_impl(m, false) }
}

unsafe extern "C" fn eclipse_pthread_mutex_unlock(m: *mut c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }

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
                return EPERM;
            }
            if ty == PTHREAD_MUTEX_RECURSIVE {
                let depth = word(m, MUTEX_DEPTH, MUTEX_WORDS).unwrap();
                let d = depth.load(Ordering::Relaxed);
                if d > 1 {
                    depth.store(d - 1, Ordering::Relaxed);
                    return 0;
                }
                depth.store(0, Ordering::Relaxed);
            }
            owner.store(0, Ordering::Release);
        }
        futex_lock_release(state);
        0
    }
}

unsafe extern "C" fn eclipse_pthread_mutex_destroy(m: *mut c_void) -> c_int {
    if m.is_null() {
        return EINVAL;
    }

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

unsafe extern "C" fn eclipse_pthread_mutexattr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

    unsafe { *(a as *mut c_int) = 0 };
    0
}

unsafe extern "C" fn eclipse_pthread_mutexattr_settype(a: *mut c_void, ty: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    if !(PTHREAD_MUTEX_NORMAL..=PTHREAD_MUTEX_ERRORCHECK).contains(&ty) {
        return EINVAL;
    }

    unsafe { *(a as *mut c_int) = ty };
    0
}

unsafe extern "C" fn eclipse_pthread_mutexattr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

const COND_SEQ: usize = 0;
const COND_MAGIC: usize = 1;
const COND_CLOCK: usize = 2;

unsafe fn cond_ensure_init(c: *mut c_void) {
    unsafe {
        if let Some(magic) = word(c, COND_MAGIC, COND_WORDS) {
            let _ = magic.compare_exchange(0, COND_INIT_MAGIC, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

unsafe extern "C" fn eclipse_pthread_cond_init(c: *mut c_void, attr: *const c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }
    let clock = if attr.is_null() {
        0
    } else {
        unsafe { *(attr as *const c_int) }
    };

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

unsafe fn cond_wait_impl(c: *mut c_void, m: *mut c_void) -> c_int {
    if c.is_null() || m.is_null() {
        return EINVAL;
    }

    unsafe {
        cond_ensure_init(c);
        let seq = match word(c, COND_SEQ, COND_WORDS) {
            Some(s) => s,
            None => return EINVAL,
        };
        let observed = seq.load(Ordering::Acquire);

        let _ = eclipse_pthread_mutex_unlock(m);
        futex_wait(seq, observed);
        let _ = eclipse_pthread_mutex_lock(m);
        0
    }
}

unsafe extern "C" fn eclipse_pthread_cond_wait(c: *mut c_void, m: *mut c_void) -> c_int {
    unsafe { cond_wait_impl(c, m) }
}

unsafe extern "C" fn eclipse_pthread_cond_timedwait(
    c: *mut c_void,
    m: *mut c_void,
    abstime: *const c_void,
) -> c_int {
    if c.is_null() || m.is_null() || abstime.is_null() {
        return EINVAL;
    }

    let deadline = unsafe { *(abstime as *const libc::timespec) };
    if !(0..1_000_000_000).contains(&deadline.tv_nsec) {
        return EINVAL;
    }

    unsafe {
        cond_ensure_init(c);
        let Some(seq) = word(c, COND_SEQ, COND_WORDS) else {
            return EINVAL;
        };
        let Some(clock_word) = word(c, COND_CLOCK, COND_WORDS) else {
            return EINVAL;
        };
        let clock = clock_word.load(Ordering::Acquire);
        if clock != 0 && clock != CLOCK_MONOTONIC {
            return EINVAL;
        }
        let observed = seq.load(Ordering::Acquire);
        let unlock_result = eclipse_pthread_mutex_unlock(m);
        if unlock_result != 0 {
            return unlock_result;
        }

        let wait_result = loop {
            if seq.load(Ordering::Acquire) != observed {
                break 0;
            }
            match futex_wait_until(seq, observed, &deadline, clock) {
                0 | EAGAIN | EINTR => continue,
                ETIMEDOUT => break ETIMEDOUT,
                _ => break EINVAL,
            }
        };

        let lock_result = eclipse_pthread_mutex_lock(m);
        if lock_result == 0 {
            wait_result
        } else {
            lock_result
        }
    }
}

unsafe extern "C" fn eclipse_pthread_cond_signal(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }

    unsafe {
        cond_ensure_init(c);
        if let Some(seq) = word(c, COND_SEQ, COND_WORDS) {
            seq.fetch_add(1, Ordering::Release);
            futex_wake(seq, 1);
        }
    }
    0
}

unsafe extern "C" fn eclipse_pthread_cond_broadcast(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }

    unsafe {
        cond_ensure_init(c);
        if let Some(seq) = word(c, COND_SEQ, COND_WORDS) {
            seq.fetch_add(1, Ordering::Release);
            futex_wake(seq, c_int::MAX);
        }
    }
    0
}

unsafe extern "C" fn eclipse_pthread_cond_destroy(c: *mut c_void) -> c_int {
    if c.is_null() {
        return EINVAL;
    }

    unsafe {
        if let Some(magic) = word(c, COND_MAGIC, COND_WORDS) {
            magic.store(0, Ordering::Release);
        }
    }
    0
}

unsafe extern "C" fn eclipse_pthread_condattr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

    unsafe { *(a as *mut c_int) = 0 };
    0
}

unsafe extern "C" fn eclipse_pthread_condattr_setclock(a: *mut c_void, clk: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

    unsafe { *(a as *mut c_int) = clk };
    let _ = CLOCK_MONOTONIC;
    0
}

unsafe extern "C" fn eclipse_pthread_condattr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

const RW_STATE: usize = 0;
const RW_MAGIC: usize = 1;
const RW_READERS: usize = 2;
const RW_WRITER: usize = 3;

unsafe fn rwlock_ensure_init(r: *mut c_void) {
    unsafe {
        if let Some(magic) = word(r, RW_MAGIC, RWLOCK_WORDS) {
            let _ =
                magic.compare_exchange(0, RWLOCK_INIT_MAGIC, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

unsafe extern "C" fn eclipse_pthread_rwlock_init(r: *mut c_void, _attr: *const c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }

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

unsafe extern "C" fn eclipse_pthread_rwlock_rdlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }

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

            futex_lock_release(state);
            futex_wait(state, 0);
        }
    }
}

unsafe extern "C" fn eclipse_pthread_rwlock_wrlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }

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

unsafe extern "C" fn eclipse_pthread_rwlock_unlock(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }

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

        futex_wake(state, c_int::MAX);
    }
    0
}

unsafe extern "C" fn eclipse_pthread_rwlock_destroy(r: *mut c_void) -> c_int {
    if r.is_null() {
        return EINVAL;
    }

    unsafe {
        if let Some(magic) = word(r, RW_MAGIC, RWLOCK_WORDS) {
            magic.store(0, Ordering::Release);
        }
    }
    0
}

unsafe extern "C" fn eclipse_pthread_once(
    once: *mut c_void,
    init: Option<extern "C" fn()>,
) -> c_int {
    if once.is_null() {
        return EINVAL;
    }

    let w = unsafe { &*(once as *const AtomicI32) };
    loop {
        if w.load(Ordering::Acquire) == ONCE_DONE {
            return 0;
        }

        match w.compare_exchange(
            ONCE_NOT_STARTED,
            ONCE_IN_PROGRESS,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if let Some(f) = init {
                    f();
                }
                w.store(ONCE_DONE, Ordering::Release);
                futex_wake(w, c_int::MAX);
                return 0;
            }
            Err(ONCE_IN_PROGRESS) => {
                futex_wait(w, ONCE_IN_PROGRESS);
            }
            Err(ONCE_DONE) => return 0,
            Err(_) => {
                std::thread::yield_now();
            }
        }
    }
}

const PTHREAD_KEYS_MAX: usize = 128;

type KeyDtor = extern "C" fn(*mut c_void);

struct KeyTable {
    slots: Vec<KeySlot>,
}

#[derive(Clone, Copy)]
struct KeySlot {
    in_use: bool,
    dtor: Option<KeyDtor>,

    generation: u32,
}

static KEY_TABLE: Mutex<Option<KeyTable>> = Mutex::new(None);

static KEY_GENERATIONS: [AtomicU32; PTHREAD_KEYS_MAX] =
    [const { AtomicU32::new(0) }; PTHREAD_KEYS_MAX];

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




    static TLS_VALUES: [Cell<TlsValue>; PTHREAD_KEYS_MAX] =
        const { [const { Cell::new(TlsValue::EMPTY) }; PTHREAD_KEYS_MAX] };
}

#[derive(Clone, Copy, Default)]
struct TlsValue {
    ptr: usize,
    generation: u32,
}

impl TlsValue {
    const EMPTY: Self = Self {
        ptr: 0,
        generation: 0,
    };
}

#[inline(always)]
fn current_key_generation(k: usize) -> Option<u32> {
    let generation = KEY_GENERATIONS[k].load(Ordering::Acquire);
    (generation != 0).then_some(generation)
}

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

                if slot.generation == 0 {
                    slot.generation = 1;
                }

                KEY_GENERATIONS[i].store(slot.generation, Ordering::Release);
                return Some((i, slot.generation));
            }
        }
        None
    });
    match idx {
        Some((i, _gen)) => {
            unsafe { *(key as *mut c_int) = i as c_int };
            0
        }
        None => 11,
    }
}

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
        KEY_GENERATIONS[k].store(0, Ordering::Release);

        t.slots[k].generation = t.slots[k].generation.wrapping_add(1);
        0
    })
}

unsafe extern "C" fn eclipse_pthread_getspecific(key: c_int) -> *mut c_void {
    let k = key as usize;
    if k >= PTHREAD_KEYS_MAX {
        return std::ptr::null_mut();
    }
    let Some(cur_gen) = current_key_generation(k) else {
        return std::ptr::null_mut();
    };

    TLS_VALUES
        .try_with(|v| {
            let entry = v[k].get();
            if entry.generation == cur_gen {
                entry.ptr as *mut c_void
            } else {
                std::ptr::null_mut()
            }
        })
        .unwrap_or(std::ptr::null_mut())
}

unsafe extern "C" fn eclipse_pthread_setspecific(key: c_int, value: *const c_void) -> c_int {
    let k = key as usize;
    if k >= PTHREAD_KEYS_MAX {
        return EINVAL;
    }
    let Some(cur_gen) = current_key_generation(k) else {
        return EINVAL;
    };

    match TLS_VALUES.try_with(|v| {
        v[k].set(TlsValue {
            ptr: value as usize,
            generation: cur_gen,
        });
    }) {
        Ok(()) => 0,
        Err(_) => EINVAL,
    }
}

fn run_thread_key_destructors() {
    const DESTRUCTOR_ITERATIONS: usize = 4;
    for _ in 0..DESTRUCTOR_ITERATIONS {
        let mut ran_any = false;

        let mut to_run: Vec<(usize, *mut c_void, KeyDtor)> = Vec::new();
        let alive = TLS_VALUES.try_with(|v| {
            key_table_with(|t| {
                for (k, value) in v.iter().enumerate() {
                    let entry = value.get();
                    if entry.ptr != 0
                        && t.slots[k].in_use
                        && t.slots[k].generation == entry.generation
                    {
                        if let Some(dtor) = t.slots[k].dtor {
                            to_run.push((k, entry.ptr as *mut c_void, dtor));
                            value.set(TlsValue::EMPTY);
                        }
                    }
                }
            });
        });
        if alive.is_err() {
            return;
        }
        for (_k, ptr, dtor) in to_run {
            ran_any = true;

            dtor(ptr);
        }
        if !ran_any {
            break;
        }
    }
}

type CxaThreadDtor = unsafe extern "C" fn(*mut c_void);

struct CxaThreadDtorEntry {
    func: CxaThreadDtor,
    obj: *mut c_void,
}

struct CxaThreadDtorList {
    entries: RefCell<Vec<CxaThreadDtorEntry>>,
}

impl Drop for CxaThreadDtorList {
    fn drop(&mut self) {
        loop {
            let entry = self.entries.borrow_mut().pop();
            match entry {
                Some(e) => unsafe { (e.func)(e.obj) },
                None => break,
            }
        }
    }
}

thread_local! {


    static CXA_THREAD_DTORS: CxaThreadDtorList = const {
        CxaThreadDtorList {
            entries: RefCell::new(Vec::new()),
        }
    };
}

fn run_cxa_thread_dtors() {
    loop {
        let entry = CXA_THREAD_DTORS.try_with(|l| l.entries.borrow_mut().pop());
        match entry {
            Ok(Some(e)) => unsafe { (e.func)(e.obj) },
            Ok(None) | Err(_) => break,
        }
    }
}

unsafe extern "C" fn eclipse_cxa_thread_atexit_impl(
    func: Option<CxaThreadDtor>,
    obj: *mut c_void,
    dso_handle: *mut c_void,
) -> c_int {
    let Some(func) = func else {
        return 1;
    };
    let registered = CXA_THREAD_DTORS.try_with(|l| {
        l.entries
            .borrow_mut()
            .push(CxaThreadDtorEntry { func, obj });
    });
    if registered.is_ok() {
        return 0;
    }

    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"__cxa_thread_atexit_impl".as_ptr());
        if !sym.is_null() {
            let host: unsafe extern "C" fn(CxaThreadDtor, *mut c_void, *mut c_void) -> c_int =
                std::mem::transmute(sym);
            return host(func, obj, dso_handle);
        }
    }

    0
}

unsafe extern "C" fn eclipse_pthread_atfork(
    prepare: Option<unsafe extern "C" fn()>,
    parent: Option<unsafe extern "C" fn()>,
    child: Option<unsafe extern "C" fn()>,
) -> c_int {
    unsafe { libc::pthread_atfork(prepare, parent, child) }
}

unsafe extern "C" fn eclipse_pthread_self() -> usize {
    gettid() as usize
}

unsafe extern "C" fn eclipse_pthread_equal(a: usize, b: usize) -> c_int {
    c_int::from(a == b)
}

unsafe extern "C" fn eclipse_pthread_gettid_np(t: usize) -> c_int {
    t as c_int
}

unsafe extern "C" fn eclipse_gettid() -> c_int {
    gettid()
}

unsafe extern "C-unwind" fn eclipse_pthread_exit(_retval: *mut c_void) -> ! {
    run_cxa_thread_dtors();
    run_thread_key_destructors();

    unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"pthread_exit".as_ptr());
        if !sym.is_null() {
            let host: unsafe extern "C-unwind" fn(*mut c_void) -> ! = std::mem::transmute(sym);
            host(_retval);
        }
        libc::syscall(60, 0);
        std::process::abort();
    }
}

fn trace_threads() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ECLIPSE_TRACE_THREADS").is_some_and(|v| v == "1"))
}

const TASK_COMM_LEN: usize = 16;

const PR_SET_NAME: c_int = 15;

const PTHREAD_CREATE_JOINABLE: c_int = 0;
const PTHREAD_CREATE_DETACHED: c_int = 1;

struct ThreadEntry {
    host_handle: libc::pthread_t,

    detached: bool,
}

static THREAD_REGISTRY: Mutex<Vec<(i32, ThreadEntry)>> = Mutex::new(Vec::new());

struct SpawnArgs {
    start: extern "C-unwind" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,

    child_tid: Arc<AtomicU32>,
}

extern "C-unwind" fn thread_trampoline(raw: *mut c_void) -> *mut c_void {
    let boxed = unsafe { Box::from_raw(raw as *mut SpawnArgs) };
    let tid = gettid();

    boxed.child_tid.store(tid as u32, Ordering::Release);
    futex_wake_u32(&boxed.child_tid, 1);
    if trace_threads() {
        trace_line("pthread_create child running tid=", tid as i64);
    }
    let start = boxed.start;
    let arg = boxed.arg;

    drop(boxed);

    let ret = start(arg);
    run_cxa_thread_dtors();
    run_thread_key_destructors();
    ret
}

fn futex_wake_u32(addr: &AtomicU32, count: c_int) {
    unsafe {
        libc::syscall(
            SYS_FUTEX,
            addr.as_ptr(),
            FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
            count,
        );
    }
}

fn futex_wait_u32(addr: &AtomicU32, expected: u32) {
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

fn trace_line(label: &str, n: i64) {
    eprintln!("[threads] {label}{n}");
}

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

    let (detached, stacksize) = if attr.is_null() {
        (false, 0usize)
    } else {
        unsafe {
            let d = *(attr as *const c_int).add(ATTR_DETACH);
            let s = *((attr as *const c_int).add(ATTR_STACKSIZE) as *const usize);
            (d == PTHREAD_CREATE_DETACHED, s)
        }
    };

    let mut host_attr: libc::pthread_attr_t = unsafe { std::mem::zeroed() };

    if unsafe { libc::pthread_attr_init(&mut host_attr) } != 0 {
        return EINVAL;
    }
    if stacksize >= libc::PTHREAD_STACK_MIN {
        unsafe { libc::pthread_attr_setstacksize(&mut host_attr, stacksize) };
    }

    let child_tid = Arc::new(AtomicU32::new(0));

    let start: extern "C-unwind" fn(*mut c_void) -> *mut c_void =
        unsafe { std::mem::transmute(start) };
    let spawn = Box::new(SpawnArgs {
        start,
        arg,
        child_tid: Arc::clone(&child_tid),
    });

    let spawn_ptr = Box::into_raw(spawn);
    let mut host_handle: libc::pthread_t = 0;

    let rc = unsafe {
        libc::pthread_create(
            &mut host_handle,
            &host_attr,
            std::mem::transmute::<
                extern "C-unwind" fn(*mut c_void) -> *mut c_void,
                extern "C" fn(*mut c_void) -> *mut c_void,
            >(thread_trampoline),
            spawn_ptr as *mut c_void,
        )
    };

    unsafe { libc::pthread_attr_destroy(&mut host_attr) };
    if rc != 0 {
        unsafe { drop(Box::from_raw(spawn_ptr)) };
        return rc;
    }

    let mut tid = child_tid.load(Ordering::Acquire);
    while tid == 0 {
        futex_wait_u32(&child_tid, 0);
        tid = child_tid.load(Ordering::Acquire);
    }
    let tid = tid as i32;

    if detached {
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
        unsafe { *(thread as *mut usize) = tid as usize };
    }
    0
}

unsafe extern "C" fn eclipse_pthread_join(thread: usize, retval: *mut *mut c_void) -> c_int {
    let tid = thread as i32;
    let host_handle = {
        let mut reg = THREAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        match reg.iter().position(|(t, _)| *t == tid) {
            Some(i) => {
                if reg[i].1.detached {
                    return EINVAL;
                }
                reg.swap_remove(i).1.host_handle
            }
            None => return 3,
        }
    };
    if trace_threads() {
        trace_line("pthread_join tid=", tid as i64);
    }

    unsafe { libc::pthread_join(host_handle, retval) }
}

unsafe extern "C" fn eclipse_pthread_detach(thread: usize) -> c_int {
    let tid = thread as i32;
    let host_handle = {
        let mut reg = THREAD_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        match reg.iter_mut().find(|(t, _)| *t == tid) {
            Some((_, entry)) => {
                if entry.detached {
                    return 0;
                }
                entry.detached = true;
                entry.host_handle
            }
            None => return 3,
        }
    };
    if trace_threads() {
        trace_line("pthread_detach tid=", tid as i64);
    }

    unsafe { libc::pthread_detach(host_handle) }
}

unsafe extern "C" fn eclipse_pthread_setname_np(thread: usize, name: *const c_char) -> c_int {
    if name.is_null() {
        return EINVAL;
    }

    let cname = unsafe { std::ffi::CStr::from_ptr(name) };
    let bytes = cname.to_bytes();
    let n = bytes.len().min(TASK_COMM_LEN - 1);
    let mut buf = [0u8; TASK_COMM_LEN];
    buf[..n].copy_from_slice(&bytes[..n]);

    let tid = thread as i32;
    if tid == gettid() {
        let rc = unsafe { libc::prctl(PR_SET_NAME, buf.as_ptr() as c_ulong, 0, 0, 0) };
        return if rc == 0 { 0 } else { eclipse_errno_value() };
    }

    let path = format!("/proc/self/task/{tid}/comm");
    match std::fs::write(&path, &buf[..n]) {
        Ok(()) => 0,
        Err(_) => 3,
    }
}

unsafe extern "C" fn eclipse_pthread_kill(thread: usize, sig: c_int) -> c_int {
    let tid = thread as i64;

    let tgid = unsafe { libc::syscall(SYS_GETPID) };

    let rc = unsafe { libc::syscall(SYS_TGKILL, tgid, tid, sig as i64) };
    if rc == 0 {
        0
    } else {
        eclipse_errno_value()
    }
}

fn eclipse_errno_value() -> c_int {
    unsafe { *libc::__errno_location() }
}

const ATTR_WORDS: usize = 14;

const ATTR_DETACH: usize = 0;

const ATTR_STACKSIZE: usize = 2;

unsafe extern "C" fn eclipse_pthread_attr_init(a: *mut c_void) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

    unsafe {
        let words = a as *mut c_int;
        for i in 0..ATTR_WORDS {
            *words.add(i) = 0;
        }
    }
    0
}

unsafe extern "C" fn eclipse_pthread_attr_destroy(a: *mut c_void) -> c_int {
    if a.is_null() {
        EINVAL
    } else {
        0
    }
}

unsafe extern "C" fn eclipse_pthread_attr_setdetachstate(a: *mut c_void, state: c_int) -> c_int {
    if a.is_null() {
        return EINVAL;
    }
    if state != PTHREAD_CREATE_JOINABLE && state != PTHREAD_CREATE_DETACHED {
        return EINVAL;
    }

    unsafe { *(a as *mut c_int).add(ATTR_DETACH) = state };
    0
}

unsafe extern "C" fn eclipse_pthread_attr_setstacksize(a: *mut c_void, size: usize) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

    unsafe { *((a as *mut c_int).add(ATTR_STACKSIZE) as *mut usize) = size };
    0
}

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

unsafe extern "C" fn eclipse_pthread_attr_getstack(
    a: *const c_void,
    base: *mut *mut c_void,
    size: *mut usize,
) -> c_int {
    if a.is_null() {
        return EINVAL;
    }

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

unsafe extern "C" fn eclipse_pthread_getattr_np(_thread: usize, attr: *mut c_void) -> c_int {
    if attr.is_null() {
        return EINVAL;
    }

    unsafe { eclipse_pthread_attr_init(attr) }
}

unsafe extern "C" fn eclipse_pthread_getschedparam(
    thread: usize,
    policy: *mut c_int,
    param: *mut c_int,
) -> c_int {
    let tid = thread as c_int;

    let pol = unsafe { libc::sched_getscheduler(tid) };
    if pol < 0 {
        return eclipse_errno_value();
    }
    if !policy.is_null() {
        unsafe { *policy = pol };
    }
    if !param.is_null() {
        let mut sp: libc::sched_param = unsafe { std::mem::zeroed() };

        if unsafe { libc::sched_getparam(tid, &mut sp) } == 0 {
            unsafe { *param = sp.sched_priority };
        }
    }
    0
}

fn host_sched_request(policy: c_int, priority: c_int) -> (c_int, c_int) {
    if policy == libc::SCHED_FIFO || policy == libc::SCHED_RR {
        (libc::SCHED_OTHER, 0)
    } else {
        (policy, priority)
    }
}

unsafe extern "C" fn eclipse_pthread_setschedparam(
    thread: usize,
    policy: c_int,
    param: *const c_int,
) -> c_int {
    let tid = thread as c_int;
    let requested_priority = if param.is_null() {
        0
    } else {
        unsafe { *param }
    };

    let rc = unsafe { eclipse_sched_setscheduler(tid, policy, &requested_priority) };
    if rc == 0 {
        0
    } else {
        eclipse_errno_value()
    }
}

unsafe extern "C" fn eclipse_sched_setscheduler(
    tid: c_int,
    policy: c_int,
    param: *const c_int,
) -> c_int {
    let requested_priority = if param.is_null() {
        0
    } else {
        unsafe { *param }
    };
    let (host_policy, host_priority) = host_sched_request(policy, requested_priority);
    let sp = libc::sched_param {
        sched_priority: host_priority,
    };

    unsafe { libc::sched_setscheduler(tid, host_policy, &sp) }
}

extern "C" {
    fn eclipse_bionic_syscall(number: c_long, ...) -> c_long;
}

pub const PTHREAD_NATIVE_COUNT: usize = 54;

pub fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    macro_rules! reg {
        ($name:literal, $f:expr) => {
            register($name, $f as *const () as u64);
        };
    }

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

    reg!("pthread_rwlock_init", eclipse_pthread_rwlock_init);
    reg!("pthread_rwlock_rdlock", eclipse_pthread_rwlock_rdlock);
    reg!("pthread_rwlock_wrlock", eclipse_pthread_rwlock_wrlock);
    reg!("pthread_rwlock_unlock", eclipse_pthread_rwlock_unlock);
    reg!("pthread_rwlock_destroy", eclipse_pthread_rwlock_destroy);

    reg!("pthread_once", eclipse_pthread_once);

    reg!("pthread_key_create", eclipse_pthread_key_create);
    reg!("pthread_key_delete", eclipse_pthread_key_delete);
    reg!("pthread_getspecific", eclipse_pthread_getspecific);
    reg!("pthread_setspecific", eclipse_pthread_setspecific);

    reg!("pthread_self", eclipse_pthread_self);
    reg!("pthread_equal", eclipse_pthread_equal);
    reg!("pthread_gettid_np", eclipse_pthread_gettid_np);
    reg!("pthread_exit", eclipse_pthread_exit);
    reg!("gettid", eclipse_gettid);

    reg!("pthread_create", eclipse_pthread_create);
    reg!("pthread_join", eclipse_pthread_join);
    reg!("pthread_detach", eclipse_pthread_detach);
    reg!("pthread_setname_np", eclipse_pthread_setname_np);
    reg!("pthread_kill", eclipse_pthread_kill);
    reg!("pthread_getattr_np", eclipse_pthread_getattr_np);
    reg!("pthread_getschedparam", eclipse_pthread_getschedparam);
    reg!("pthread_setschedparam", eclipse_pthread_setschedparam);
    reg!("sched_setscheduler", eclipse_sched_setscheduler);
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

    reg!("__cxa_thread_atexit_impl", eclipse_cxa_thread_atexit_impl);
    reg!("pthread_atfork", eclipse_pthread_atfork);

    reg!("sem_init", eclipse_sem_init);
    reg!("sem_wait", eclipse_sem_wait);
    reg!("sem_post", eclipse_sem_post);
    reg!("sem_destroy", eclipse_sem_destroy);

    register("syscall", eclipse_bionic_syscall as *const () as u64);
}

const SEM_WORDS: usize = 4;
const SEM_COUNT: usize = 0;

unsafe extern "C" fn eclipse_sem_init(s: *mut c_void, _pshared: c_int, value: c_int) -> c_int {
    if s.is_null() {
        return EINVAL;
    }

    unsafe {
        if let Some(c) = word(s, SEM_COUNT, SEM_WORDS) {
            c.store(value, Ordering::Release);
        }
    }
    0
}

unsafe extern "C" fn eclipse_sem_wait(s: *mut c_void) -> c_int {
    if s.is_null() {
        return EINVAL;
    }

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

unsafe extern "C" fn eclipse_sem_post(s: *mut c_void) -> c_int {
    if s.is_null() {
        return EINVAL;
    }

    unsafe {
        if let Some(count) = word(s, SEM_COUNT, SEM_WORDS) {
            count.fetch_add(1, Ordering::Release);
            futex_wake(count, 1);
        }
    }
    0
}

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

    fn zeroed_words(n: usize) -> Box<[i32]> {
        vec![0i32; n].into_boxed_slice()
    }

    #[test]
    fn bionic_object_word_counts_match_abi() {
        assert_eq!(MUTEX_WORDS * 4, 40);
        assert_eq!(COND_WORDS * 4, 48);
        assert_eq!(RWLOCK_WORDS * 4, 56);
        assert_eq!(SEM_WORDS * 4, 16);

        assert_eq!(std::mem::size_of::<c_int>(), 4);

        let mut n = 0;
        register_natives(|_, _| n += 1);
        assert_eq!(n, PTHREAD_NATIVE_COUNT);
    }

    #[test]
    fn android_realtime_requests_do_not_become_unbounded_host_realtime() {
        assert_eq!(
            host_sched_request(libc::SCHED_FIFO, 99),
            (libc::SCHED_OTHER, 0)
        );
        assert_eq!(
            host_sched_request(libc::SCHED_RR, 42),
            (libc::SCHED_OTHER, 0)
        );
        assert_eq!(
            host_sched_request(libc::SCHED_OTHER, 0),
            (libc::SCHED_OTHER, 0),
            "ordinary Android scheduling requests pass through unchanged"
        );
    }

    #[test]
    fn cond_timedwait_honors_configured_deadline_and_relocks_mutex() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        for clock in [0, CLOCK_MONOTONIC] {
            let cond: &'static mut [i32] = Box::leak(zeroed_words(COND_WORDS));
            let mut mutex = zeroed_words(MUTEX_WORDS);
            let cp = cond.as_mut_ptr() as *mut c_void;
            let mp = mutex.as_mut_ptr() as *mut c_void;
            let mut attr = -1;
            let ap = std::ptr::addr_of_mut!(attr) as *mut c_void;

            let (cancel_tx, cancel_rx) = mpsc::channel();
            let cond_addr = cp as usize;
            let watchdog = std::thread::spawn(move || {
                if cancel_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    unsafe { eclipse_pthread_cond_signal(cond_addr as *mut c_void) };
                }
            });

            let mut deadline = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };

            let result = unsafe {
                assert_eq!(eclipse_pthread_condattr_init(ap), 0);
                assert_eq!(eclipse_pthread_condattr_setclock(ap, clock), 0);
                assert_eq!(eclipse_pthread_cond_init(cp, ap), 0);
                assert_eq!(eclipse_pthread_mutex_lock(mp), 0);
                assert_eq!(libc::clock_gettime(clock, &mut deadline), 0);
                deadline.tv_nsec += 30_000_000;
                if deadline.tv_nsec >= 1_000_000_000 {
                    deadline.tv_sec += 1;
                    deadline.tv_nsec -= 1_000_000_000;
                }
                let started = Instant::now();
                let result = eclipse_pthread_cond_timedwait(
                    cp,
                    mp,
                    std::ptr::addr_of!(deadline) as *const c_void,
                );
                assert!(
                    started.elapsed() >= Duration::from_millis(20),
                    "clock {clock}: the absolute deadline must bound a real wait"
                );
                result
            };

            let _ = cancel_tx.send(());
            watchdog.join().unwrap();
            assert_eq!(
                result, ETIMEDOUT,
                "clock {clock}: an unsignaled timed wait must return ETIMEDOUT"
            );

            unsafe {
                assert_eq!(eclipse_pthread_mutex_trylock(mp), EBUSY);
                assert_eq!(eclipse_pthread_mutex_unlock(mp), 0);
                assert_eq!(eclipse_pthread_cond_destroy(cp), 0);
                assert_eq!(eclipse_pthread_mutex_destroy(mp), 0);
                assert_eq!(eclipse_pthread_condattr_destroy(ap), 0);
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    cp as *mut i32,
                    COND_WORDS,
                )));
            }
        }
    }

    #[test]
    fn mutex_lock_unlock_trylock_cycle_normal() {
        let mut m = zeroed_words(MUTEX_WORDS);
        let mp = m.as_mut_ptr() as *mut c_void;

        unsafe {
            assert_eq!(eclipse_pthread_mutex_lock(mp), 0, "lock a free mutex");

            assert_eq!(
                eclipse_pthread_mutex_trylock(mp),
                EBUSY,
                "trylock a held mutex → EBUSY"
            );
            assert_eq!(eclipse_pthread_mutex_unlock(mp), 0, "unlock");

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

        unsafe {
            assert_eq!(eclipse_pthread_mutexattr_init(ap), 0);
            assert_eq!(
                eclipse_pthread_mutexattr_settype(ap, PTHREAD_MUTEX_RECURSIVE),
                0
            );
            assert_eq!(eclipse_pthread_mutex_init(mp, ap as *const c_void), 0);

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

                    unsafe { eclipse_pthread_mutex_lock(mp) };

                    let now = counter.fetch_add(1, O::AcqRel) + 1;
                    max_seen.fetch_max(now, O::AcqRel);
                    counter.fetch_sub(1, O::AcqRel);
                    let mp = mp_addr as *mut c_void;

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

        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                mp_addr as *mut i32,
                MUTEX_WORDS,
            )));
        }
    }

    #[test]
    fn once_runs_init_exactly_once_under_contention() {
        use std::sync::atomic::{AtomicI32 as A32, Ordering as O};

        static RUN_COUNT: A32 = A32::new(0);
        RUN_COUNT.store(0, O::SeqCst);
        extern "C" fn init() {
            RUN_COUNT.fetch_add(1, O::SeqCst);
        }

        let once: &'static mut [i32] = Box::leak(zeroed_words(1));
        let once_addr = once.as_mut_ptr() as usize;

        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(std::thread::spawn(move || {
                let op = once_addr as *mut c_void;

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

        let op = once_addr as *mut c_void;

        unsafe { eclipse_pthread_once(op, Some(init)) };
        assert_eq!(RUN_COUNT.load(O::SeqCst), 1, "once stays done");

        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                once_addr as *mut i32,
                1,
            )));
        }
    }

    #[test]
    fn key_create_get_set_roundtrip_and_isolation_across_threads() {
        let mut key: c_int = -1;
        let kp = std::ptr::addr_of_mut!(key) as *mut c_void;

        unsafe {
            assert_eq!(eclipse_pthread_key_create(kp, None), 0, "allocate a key");
        }
        assert!(key >= 0, "a valid key index");

        unsafe {
            assert!(
                eclipse_pthread_getspecific(key).is_null(),
                "unset key reads NULL"
            );

            assert_eq!(eclipse_pthread_setspecific(key, 0x1234 as *const c_void), 0);
            assert_eq!(eclipse_pthread_getspecific(key), 0x1234 as *mut c_void);
        }

        let key_copy = key;
        let other = std::thread::spawn(move || unsafe {
            let before = eclipse_pthread_getspecific(key_copy);
            assert!(before.is_null(), "other thread sees NULL (isolation)");
            eclipse_pthread_setspecific(key_copy, 0x9999 as *const c_void);
            eclipse_pthread_getspecific(key_copy) as usize
        })
        .join()
        .unwrap();
        assert_eq!(other, 0x9999, "other thread sets+reads its own value");

        unsafe {
            assert_eq!(
                eclipse_pthread_getspecific(key),
                0x1234 as *mut c_void,
                "this thread's value is isolated from the other thread"
            );
        }

        unsafe {
            assert_eq!(eclipse_pthread_key_delete(key), 0);
            assert!(
                eclipse_pthread_getspecific(key).is_null(),
                "deleted key reads NULL even if a value was set under it"
            );

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

        unsafe {
            assert_eq!(eclipse_pthread_key_create(kp, Some(dtor)), 0);
        }
        let key_copy = key;

        std::thread::spawn(move || {
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

        unsafe {
            eclipse_pthread_key_delete(key);
        }
    }

    #[test]
    fn cxa_dtors_run_before_key_dtors_and_lifo_on_return_from_start() {
        use std::sync::atomic::{AtomicUsize as AU, Ordering as O};
        static SEQ: AU = AU::new(0);
        static KEY_T: AU = AU::new(0);
        static CXA_A_T: AU = AU::new(0);
        static CXA_B_T: AU = AU::new(0);
        static CXA_C_T: AU = AU::new(0);

        extern "C" fn key_dtor(_v: *mut c_void) {
            KEY_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);
        }
        unsafe extern "C" fn cxa_c(_o: *mut c_void) {
            CXA_C_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);
        }
        unsafe extern "C" fn cxa_a(_o: *mut c_void) {
            CXA_A_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);

            unsafe {
                assert_eq!(
                    eclipse_cxa_thread_atexit_impl(
                        Some(cxa_c),
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0,
                    "mid-drain re-registration succeeds"
                );
            }
        }
        unsafe extern "C" fn cxa_b(_o: *mut c_void) {
            CXA_B_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);
        }
        extern "C" fn start(arg: *mut c_void) -> *mut c_void {
            let key = arg as usize as c_int;

            unsafe {
                assert_eq!(eclipse_pthread_setspecific(key, 0x51 as *const c_void), 0);
                assert_eq!(
                    eclipse_cxa_thread_atexit_impl(
                        Some(cxa_a),
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0
                );
                assert_eq!(
                    eclipse_cxa_thread_atexit_impl(
                        Some(cxa_b),
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0
                );

                assert_eq!(
                    eclipse_cxa_thread_atexit_impl(
                        None,
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    1
                );
            }
            std::ptr::null_mut()
        }

        let mut key: c_int = -1;
        let kp = std::ptr::addr_of_mut!(key) as *mut c_void;

        unsafe { assert_eq!(eclipse_pthread_key_create(kp, Some(key_dtor)), 0) };
        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;

        let rc = unsafe {
            eclipse_pthread_create(
                tp,
                std::ptr::null(),
                Some(start),
                key as usize as *mut c_void,
            )
        };
        assert_eq!(rc, 0, "create succeeds");

        unsafe { assert_eq!(eclipse_pthread_join(tid, std::ptr::null_mut()), 0) };

        let (k, a, b, c) = (
            KEY_T.load(O::SeqCst),
            CXA_A_T.load(O::SeqCst),
            CXA_B_T.load(O::SeqCst),
            CXA_C_T.load(O::SeqCst),
        );
        assert!(
            k != 0 && a != 0 && b != 0 && c != 0,
            "all four destructors ran on the return-from-start path (KEY={k} A={a} B={b} C={c})"
        );

        assert!(
            b < a && a < c,
            "cxa LIFO + loop-drain order is B, A, C (got B={b} A={a} C={c})"
        );

        assert!(
            c < k,
            "cxa finalizers run BEFORE key destructors on return-from-start \
             (bionic __cxa_thread_finalize → pthread_key_clean_all; got last-cxa={c} KEY={k})"
        );

        unsafe { eclipse_pthread_key_delete(key) };
    }

    #[test]
    fn cxa_dtors_run_before_key_dtors_on_pthread_exit_path() {
        use std::sync::atomic::{AtomicUsize as AU, Ordering as O};
        static SEQ: AU = AU::new(0);
        static KEY_T: AU = AU::new(0);
        static CXA_T: AU = AU::new(0);

        extern "C" fn key_dtor(_v: *mut c_void) {
            KEY_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);
        }
        unsafe extern "C" fn cxa_d(_o: *mut c_void) {
            CXA_T.store(SEQ.fetch_add(1, O::SeqCst) + 1, O::SeqCst);
        }

        extern "C-unwind" fn start(arg: *mut c_void) -> *mut c_void {
            let key = arg as usize as c_int;

            unsafe {
                assert_eq!(eclipse_pthread_setspecific(key, 0x52 as *const c_void), 0);
                assert_eq!(
                    eclipse_cxa_thread_atexit_impl(
                        Some(cxa_d),
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0
                );
                eclipse_pthread_exit(0x77 as *mut c_void)
            }
        }

        let mut key: c_int = -1;
        let kp = std::ptr::addr_of_mut!(key) as *mut c_void;

        unsafe { assert_eq!(eclipse_pthread_key_create(kp, Some(key_dtor)), 0) };
        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;

        let rc = unsafe {
            eclipse_pthread_create(
                tp,
                std::ptr::null(),
                Some(std::mem::transmute::<
                    extern "C-unwind" fn(*mut c_void) -> *mut c_void,
                    extern "C" fn(*mut c_void) -> *mut c_void,
                >(start)),
                key as usize as *mut c_void,
            )
        };
        assert_eq!(rc, 0, "create succeeds");
        let mut retval: *mut c_void = std::ptr::null_mut();

        unsafe { assert_eq!(eclipse_pthread_join(tid, &mut retval), 0) };
        assert_eq!(
            retval as usize, 0x77,
            "pthread_exit's retval round-trips through join"
        );

        let (k, d) = (KEY_T.load(O::SeqCst), CXA_T.load(O::SeqCst));
        assert!(
            k != 0 && d != 0,
            "both destructor classes ran on the explicit pthread_exit path (KEY={k} CXA={d})"
        );
        assert!(
            d < k,
            "cxa finalizers run BEFORE key destructors on pthread_exit \
             (bionic __cxa_thread_finalize → pthread_key_clean_all; got CXA={d} KEY={k})"
        );

        unsafe { eclipse_pthread_key_delete(key) };
    }

    #[test]
    fn pthread_atfork_registers_handlers_including_null() {
        let rc = unsafe { eclipse_pthread_atfork(None, None, None) };
        assert_eq!(rc, 0, "pthread_atfork(NULL, NULL, NULL) registers cleanly");
    }

    #[test]
    fn sem_post_then_wait_does_not_block() {
        let mut s = zeroed_words(SEM_WORDS);
        let sp = s.as_mut_ptr() as *mut c_void;

        unsafe {
            assert_eq!(eclipse_sem_init(sp, 0, 0), 0);
            assert_eq!(eclipse_sem_post(sp), 0, "post → count 1");
            assert_eq!(eclipse_sem_post(sp), 0, "post → count 2");

            assert_eq!(eclipse_sem_wait(sp), 0);
            assert_eq!(eclipse_sem_wait(sp), 0);
            assert_eq!(eclipse_sem_destroy(sp), 0);
        }
    }

    #[test]
    fn self_equal_and_gettid_are_consistent() {
        unsafe {
            let me = eclipse_pthread_self();
            assert!(eclipse_pthread_equal(me, me) != 0, "a thread equals itself");
            assert_eq!(
                eclipse_pthread_gettid_np(me),
                eclipse_gettid(),
                "gettid_np(self) == gettid()"
            );

            assert_eq!(eclipse_pthread_equal(me, me ^ 1), 0);
        }
    }

    #[test]
    fn null_objects_return_einval_not_crash() {
        let n = std::ptr::null_mut::<c_void>();

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

    #[test]
    fn create_runs_entry_on_real_thread_and_join_returns_its_result() {
        use std::sync::atomic::{AtomicI32 as A32, Ordering as O};

        static RAN_TID: A32 = A32::new(0);
        RAN_TID.store(0, O::SeqCst);
        extern "C" fn start(arg: *mut c_void) -> *mut c_void {
            RAN_TID.store(gettid(), O::SeqCst);

            (arg as usize + 1) as *mut c_void
        }

        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;

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

        let again = unsafe { eclipse_pthread_join(tid, std::ptr::null_mut()) };
        assert_eq!(again, 3, "re-join of a consumed thread → ESRCH");
    }

    #[test]
    fn create_returns_each_childs_own_tid_under_heavy_parallel_load() {
        extern "C" fn start(_arg: *mut c_void) -> *mut c_void {
            gettid() as usize as *mut c_void
        }

        const N: usize = 64;
        const ROUNDS: usize = 16;
        for _ in 0..ROUNDS {
            let mut creators = Vec::with_capacity(N);
            for _ in 0..N {
                creators.push(std::thread::spawn(|| {
                    let mut tid: usize = 0;
                    let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;

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

                    let jrc = unsafe { eclipse_pthread_join(tid, &mut retval) };
                    assert_eq!(jrc, 0, "join succeeds under load");

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

        let mut attr = [0i32; ATTR_WORDS];
        let ap = attr.as_mut_ptr() as *mut c_void;

        unsafe {
            assert_eq!(eclipse_pthread_attr_init(ap), 0);
            assert_eq!(
                eclipse_pthread_attr_setdetachstate(ap, PTHREAD_CREATE_DETACHED),
                0
            );
        }
        let mut tid: usize = 0;
        let tp = std::ptr::addr_of_mut!(tid) as *mut c_void;

        let rc = unsafe {
            eclipse_pthread_create(tp, ap as *const c_void, Some(start), std::ptr::null_mut())
        };
        assert_eq!(rc, 0, "detached create succeeds");

        let jrc = unsafe { eclipse_pthread_join(tid, std::ptr::null_mut()) };
        assert_eq!(jrc, 3, "a detached thread is not joinable → ESRCH");

        for _ in 0..1000 {
            if RAN.load(O::SeqCst) {
                break;
            }
            std::thread::yield_now();
        }
        assert!(RAN.load(O::SeqCst), "the detached thread ran its entry");

        unsafe { assert_eq!(eclipse_pthread_attr_destroy(ap), 0) };
    }

    #[test]
    fn setname_np_on_self_succeeds_and_is_truncated() {
        let me = gettid() as usize;
        let short = c"ecl-test";
        let long = c"this-name-is-way-too-long-for-comm";

        unsafe {
            assert_eq!(eclipse_pthread_setname_np(me, short.as_ptr()), 0);
            assert_eq!(
                eclipse_pthread_setname_np(me, long.as_ptr()),
                0,
                "an over-length name is truncated, not rejected"
            );

            assert_eq!(eclipse_pthread_setname_np(me, std::ptr::null()), EINVAL);
        }
    }

    #[test]
    fn attr_records_detachstate_and_stacksize() {
        let mut attr = [0i32; ATTR_WORDS];
        let ap = attr.as_mut_ptr() as *mut c_void;

        unsafe {
            assert_eq!(eclipse_pthread_attr_init(ap), 0);

            assert_eq!(
                *(ap as *const c_int).add(ATTR_DETACH),
                PTHREAD_CREATE_JOINABLE
            );

            assert_eq!(eclipse_pthread_attr_setstacksize(ap, 0x40000), 0);

            let mut base: *mut c_void = std::ptr::without_provenance_mut(0x1);
            let mut size: usize = 0;
            assert_eq!(eclipse_pthread_attr_getstack(ap, &mut base, &mut size), 0);
            assert!(base.is_null(), "host-owned stack → base reported NULL");
            assert_eq!(size, 0x40000, "getstack reports the recorded stack size");

            assert_eq!(eclipse_pthread_attr_setdetachstate(ap, 99), EINVAL);
            assert_eq!(eclipse_pthread_attr_destroy(ap), 0);
        }
    }

    #[test]
    fn kill_signal_zero_probes_a_live_thread() {
        let me = gettid() as usize;

        unsafe {
            assert_eq!(eclipse_pthread_kill(me, 0), 0, "self is alive");

            assert_eq!(
                eclipse_pthread_kill(0x7fff_fffe, 0),
                3,
                "unknown TID → ESRCH"
            );
        }
    }
}
