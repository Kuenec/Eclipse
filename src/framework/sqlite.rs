//! `android.database.sqlite.SQLiteConnection`'s natives, backed by the bundled SQLite (libsqlite3-sys).
//!
//! 2026-06-11: ATL's `SQLiteConnection.java` declares the full AOSP `private static native` surface
//! (`nativeOpen`/`prepare`/`bind`/`step`/`column`/…) but backs it in its GTK lib
//! (`libtranslation_layer_main.so`) which Eclipse does not load — so Roblox's `ActivitySplash.onCreate`
//! DB open is an `UnsatisfiedLinkError`. Eclipse binds these natives itself (`RegisterNatives`, like
//! `Context`/`ConnectivityManager`) against the RAW `libsqlite3-sys` FFI: the JNI contract IS a thin
//! C-API surface, so the `sqlite3*`/`sqlite3_stmt*` pointers round-trip as the opaque `jlong` handles
//! the Java side stores. For soundness (AGENTS.md §2.8) the `jlong`s are NOT raw pointers but
//! generational-slab indices (a stale/fabricated handle is a checked `Err`, never a wild deref).
//!
//! ## Scope: the full `SQLiteConnection` native surface (open + statements + executes + cursor window)
//! Covers `nativeOpen` and the PRAGMA / `android_metadata` / `CREATE TABLE` / version-check sequence
//! `SQLiteOpenHelper.getWritableDatabase` issues, AND `nativeExecuteForCursorWindow` — the row-returning
//! SELECT path. NOTE ATL's `android.database.CursorWindow` is a **pure-Java** `ArrayList<Object[]>` (no
//! native buffer), so `nativeExecuteForCursorWindow` fills it via the window's Java methods
//! (`clear`/`setNumColumns`/`allocRow`/`put{Long,Double,String,Blob,Null}`) over JNI — there is no
//! `#[repr(C,packed)]` FieldSlot buffer here.
//!
//! UTF-8 SQLite entry points are used (`sqlite3_open_v2`/`prepare_v2`/`bind_text`/`column_text`) rather
//! than AOSP's UTF-16 ones — functionally identical (SQLite stores text as UTF-8), and a Java `String`
//! converts cleanly to a Rust `String`. On any SQLite error a native throws
//! `android.database.sqlite.SQLiteException` (via JNI) and returns a neutral default.

#![allow(clippy::not_unsafe_ptr_arg_deref)] // the jlong handles are slab indices, deref'd only after a checked lookup

use std::ffi::{c_int, CStr, CString};
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::strings::JNIStr;
use jni::sys::{jboolean, jdouble, jint, jlong};
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use jni::errors::LogErrorAndDefault;
use jni::refs::Reference;
use jni::strings::JNIString;
use libsqlite3_sys as ffi;

/// A null `JString` (the neutral return for a `String`-returning native on a SQL NULL or bad handle).
fn null_string<'l>() -> JString<'l> {
    <JString as Reference>::null()
}

/// Throw `android.database.sqlite.SQLiteException` with a plain message (the JNI `msg` is modified-UTF-8).
fn throw_msg(env: &mut Env, msg: &str) {
    let _ = env.throw_new(SQLITE_EXCEPTION_CLASS, JNIString::from(msg));
}

// --- SQLite result + open-flag constants (standard, stable ABI; defined locally to avoid relying on a
//     specific libsqlite3-sys export name) -----------------------------------------------------------
const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_UTF8: c_int = 1; // text encoding for a collation/function registration
                              // sqlite3_column_type result codes.
const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;

// `android.database.sqlite.SQLiteDatabase` open-flag bits `nativeOpen`'s `openFlags` carries, mapped to
// SQLite flags exactly as AOSP's `android_database_SQLiteConnection.cpp` does.
const ANDROID_OPEN_READONLY: jint = 0x0000_0001; // SQLiteDatabase.OPEN_READONLY
const ANDROID_CREATE_IF_NECESSARY: jint = 0x1000_0000; // SQLiteDatabase.CREATE_IF_NECESSARY

/// The Java exception class thrown on a SQLite error (slashed name for `throw_new`).
const SQLITE_EXCEPTION_CLASS: &JNIStr = jni_str!("android/database/sqlite/SQLiteException");

// === Generational-slab registry of the raw sqlite3*/sqlite3_stmt* the jlong handles index ============

/// A raw SQLite pointer made `Send` so it can live in a process-global `Mutex`'d slab.
///
/// SAFETY: the bundled SQLite is built `SQLITE_THREADSAFE=1` (serialized mode — it serializes access to
/// a connection/statement internally), and every access here goes through the registry `Mutex`, so the
/// pointer is only ever touched by one thread at a time. The pointer is owned by SQLite; the slab merely
/// indexes it and never frees it (only `nativeClose`/`nativeFinalizeStatement` do, via the C API).
struct SendPtr<T>(*mut T);
// SAFETY: see SendPtr's doc — serialized SQLite + Mutex-guarded single-threaded access.
unsafe impl<T> Send for SendPtr<T> {}

struct Slot<T> {
    generation: u32,
    ptr: Option<SendPtr<T>>,
}

struct Slab<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

impl<T> Slab<T> {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    /// Store `ptr`, returning its packed handle (generation high, index low; generation ≥ 1 so a valid
    /// handle is never the reserved `0`).
    fn insert(&mut self, ptr: *mut T) -> jlong {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.ptr = Some(SendPtr(ptr));
            return pack(index, slot.generation);
        }
        let index = self.slots.len() as u32;
        self.slots.push(Slot {
            generation: 1,
            ptr: Some(SendPtr(ptr)),
        });
        pack(index, 1)
    }

    /// The raw pointer for `handle`, or `None` for a stale/fabricated/out-of-range handle.
    fn get(&self, handle: jlong) -> Option<*mut T> {
        let (index, generation) = unpack(handle);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.ptr.as_ref().map(|p| p.0)
    }

    /// Free `handle`'s slot (bumping its generation) and return the raw pointer it held, or `None` if
    /// the handle was already free/stale/fabricated.
    fn remove(&mut self, handle: jlong) -> Option<*mut T> {
        let (index, generation) = unpack(handle);
        let slot = self.slots.get_mut(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        let ptr = slot.ptr.take()?;
        slot.generation = slot.generation.saturating_add(1);
        self.free.push(index);
        Some(ptr.0)
    }
}

fn pack(index: u32, generation: u32) -> jlong {
    ((generation as u64) << 32 | index as u64) as jlong
}

fn unpack(handle: jlong) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

static CONNECTIONS: OnceLock<Mutex<Slab<ffi::sqlite3>>> = OnceLock::new();
static STATEMENTS: OnceLock<Mutex<Slab<ffi::sqlite3_stmt>>> = OnceLock::new();

fn connections() -> &'static Mutex<Slab<ffi::sqlite3>> {
    CONNECTIONS.get_or_init(|| Mutex::new(Slab::new()))
}
fn statements() -> &'static Mutex<Slab<ffi::sqlite3_stmt>> {
    STATEMENTS.get_or_init(|| Mutex::new(Slab::new()))
}

fn conn_get(handle: jlong) -> Option<*mut ffi::sqlite3> {
    connections()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(handle)
}
fn stmt_get(handle: jlong) -> Option<*mut ffi::sqlite3_stmt> {
    statements()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(handle)
}

// === error helper =================================================================================

/// Throw `SQLiteException` carrying `db`'s last error message, and return `default`. Used on any
/// unexpected SQLite result code so the Java side sees the exception (matching AOSP's `throw_sqlite3_exception`).
fn throw_sqlite<T>(env: &mut Env, db: *mut ffi::sqlite3, default: T) -> jni::errors::Result<T> {
    // SAFETY: `db` is a live sqlite3* (a slab-validated handle's pointer); `sqlite3_errmsg` returns a
    // NUL-terminated UTF-8 string owned by SQLite, valid until the next call on `db`.
    let msg = unsafe {
        let raw = ffi::sqlite3_errmsg(db);
        if raw.is_null() {
            "SQLite error".to_string()
        } else {
            CStr::from_ptr(raw).to_string_lossy().into_owned()
        }
    };
    throw_msg(env, &msg);
    Ok(default)
}

/// Look up a statement handle or throw `SQLiteException` (a stale/closed statement is a Java-side logic
/// error, surfaced as an exception not UB). Returns `None` after throwing.
fn require_stmt<T>(
    env: &mut Env,
    handle: jlong,
    default: T,
) -> Result<*mut ffi::sqlite3_stmt, jni::errors::Result<T>> {
    match stmt_get(handle) {
        Some(p) => Ok(p),
        None => {
            throw_msg(env, "invalid or closed statement handle");
            Err(Ok(default))
        }
    }
}

// === SQLiteConnection natives (all `static native`; second arg is the JClass) =======================

extern "system" fn native_open<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    path: JString<'l>,
    open_flags: jint,
    _label: JString<'l>,
    _enable_trace: jboolean,
    _enable_profile: jboolean,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let path = path.try_to_string(env)?;
        let Ok(c_path) = CString::new(path) else {
            throw_msg(env, "database path contains a NUL byte");
            return Ok(0);
        };
        // AOSP's openFlags → SQLite flags mapping (android_database_SQLiteConnection.cpp).
        let sqlite_flags = if open_flags & ANDROID_CREATE_IF_NECESSARY != 0 {
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE
        } else if open_flags & ANDROID_OPEN_READONLY != 0 {
            SQLITE_OPEN_READONLY
        } else {
            SQLITE_OPEN_READWRITE
        };
        let mut db: *mut ffi::sqlite3 = std::ptr::null_mut();
        // SAFETY: `c_path` is a valid NUL-terminated path; `&mut db` receives the new connection; the
        // null final arg requests the default VFS — the standard sqlite3_open_v2 contract.
        let rc = unsafe {
            ffi::sqlite3_open_v2(c_path.as_ptr(), &mut db, sqlite_flags, std::ptr::null())
        };
        if rc != SQLITE_OK {
            // On failure SQLite may still allocate a handle carrying the error message; report then close.
            let r = throw_sqlite(env, db, 0);
            // SAFETY: closing a (possibly partially-opened) handle is the documented cleanup.
            unsafe { ffi::sqlite3_close(db) };
            return r;
        }
        // A modest busy timeout so a transient lock surfaces as a retry, not an immediate error
        // (matches AOSP's default busy handler intent).
        // SAFETY: `db` is the freshly-opened live connection.
        unsafe { ffi::sqlite3_busy_timeout(db, 2500) };
        // Register the "LOCALIZED"/"UNICODE" collations up front so a `COLLATE LOCALIZED` in a CREATE
        // TABLE / query (before SQLiteDatabase.setLocale runs) resolves (AOSP registers them at open too).
        register_localized_collations(db);
        Ok(connections()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(db))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_has_codec<'l>(mut env: EnvUnowned<'l>, _cls: JClass<'l>) -> jboolean {
    // No SQLCipher/codec build — plain SQLite.
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(false) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_close<'l>(mut env: EnvUnowned<'l>, _cls: JClass<'l>, conn: jlong) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Some(db) = connections()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(conn)
        {
            // SAFETY: `db` was a live connection just removed from the slab; close_v2 defers if any
            // statements are still outstanding (it never UB's), then frees the connection.
            unsafe { ffi::sqlite3_close(db) };
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_prepare_statement<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    sql: JString<'l>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let Some(db) = conn_get(conn) else {
            throw_msg(env, "invalid or closed connection handle");
            return Ok(0);
        };
        let sql = sql.try_to_string(env)?;
        let bytes = sql.as_bytes();
        let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
        // SAFETY: `db` is a live connection; `bytes` is the SQL text with its byte length; a null tail
        // arg ignores any trailing SQL — the sqlite3_prepare_v2 contract.
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(
                db,
                bytes.as_ptr() as *const std::os::raw::c_char,
                bytes.len() as c_int,
                &mut stmt,
                std::ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            return throw_sqlite(env, db, 0);
        }
        // An empty/whitespace statement yields a null stmt with SQLITE_OK; hand back 0 (the Java side
        // tolerates a 0 statement pointer for a no-op statement).
        if stmt.is_null() {
            return Ok(0);
        }
        Ok(statements()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(stmt))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_finalize_statement<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Some(s) = statements()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(stmt)
        {
            // SAFETY: `s` was a live prepared statement just removed from the slab; finalize frees it.
            unsafe { ffi::sqlite3_finalize(s) };
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_get_parameter_count<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let s = match require_stmt(env, stmt, 0) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement (slab-validated).
        Ok(unsafe { ffi::sqlite3_bind_parameter_count(s) })
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_is_read_only<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        let s = match require_stmt(env, stmt, false) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement.
        Ok(unsafe { ffi::sqlite3_stmt_readonly(s) } != 0)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_get_column_count<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let s = match require_stmt(env, stmt, 0) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement.
        Ok(unsafe { ffi::sqlite3_column_count(s) })
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_get_column_name<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
) -> JString<'l> {
    env.with_env(|env| -> jni::errors::Result<JString<'l>> {
        let s = match require_stmt(env, stmt, null_string()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement; `sqlite3_column_name` returns a NUL-terminated UTF-8 name
        // (or null if `index` is out of range), owned by SQLite until the statement is finalized/reset.
        let raw = unsafe { ffi::sqlite3_column_name(s, index) };
        if raw.is_null() {
            return env.new_string("");
        }
        let name = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        env.new_string(name)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_bind_null<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement; `index` is the 1-based bind index.
        unsafe { ffi::sqlite3_bind_null(s, index) };
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_bind_long<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
    value: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement; bind a 64-bit int at the 1-based `index`.
        unsafe { ffi::sqlite3_bind_int64(s, index, value) };
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_bind_double<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
    value: jdouble,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement.
        unsafe { ffi::sqlite3_bind_double(s, index, value) };
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_bind_string<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
    value: JString<'l>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        let text = value.try_to_string(env)?;
        let bytes = text.as_bytes();
        // SAFETY: `s` is a live statement; bind the UTF-8 text with its byte length. SQLITE_TRANSIENT
        // makes SQLite COPY the bytes immediately, so `text` may be dropped right after this call.
        unsafe {
            ffi::sqlite3_bind_text(
                s,
                index,
                bytes.as_ptr() as *const std::os::raw::c_char,
                bytes.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_bind_blob<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
    index: jint,
    value: JByteArray<'l>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        let bytes: Vec<u8> = env.convert_byte_array(&value)?;
        // SAFETY: `s` is a live statement; bind the blob with its length. SQLITE_TRANSIENT copies it.
        unsafe {
            ffi::sqlite3_bind_blob(
                s,
                index,
                bytes.as_ptr() as *const std::os::raw::c_void,
                bytes.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_reset_statement_and_clear_bindings<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    stmt: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let s = match require_stmt(env, stmt, ()) {
            Ok(s) => s,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement; reset rewinds it, clear_bindings drops bound values.
        unsafe {
            ffi::sqlite3_reset(s);
            ffi::sqlite3_clear_bindings(s);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `sqlite3_step` then `sqlite3_reset`, throwing `SQLiteException` on an error code. Returns the step
/// result code (`SQLITE_ROW`/`SQLITE_DONE`) for the caller to interpret, or `None` if it threw.
fn step_then_reset(
    env: &mut Env,
    db: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
) -> Option<c_int> {
    // SAFETY: `db`/`stmt` are live (slab-validated). step advances; reset rewinds for reuse.
    let rc = unsafe { ffi::sqlite3_step(stmt) };
    if rc != SQLITE_ROW && rc != SQLITE_DONE {
        let _ = throw_sqlite(env, db, ());
        // Still reset so the statement can be reused/finalized cleanly.
        unsafe { ffi::sqlite3_reset(stmt) };
        return None;
    }
    Some(rc)
}

/// Resolve (connection, statement) handles for an execute native, throwing on a bad handle.
fn require_conn_stmt<T>(
    env: &mut Env,
    conn: jlong,
    stmt: jlong,
    default: T,
) -> Result<(*mut ffi::sqlite3, *mut ffi::sqlite3_stmt), jni::errors::Result<T>> {
    let Some(db) = conn_get(conn) else {
        throw_msg(env, "invalid or closed connection handle");
        return Err(Ok(default));
    };
    match stmt_get(stmt) {
        Some(s) => Ok((db, s)),
        None => {
            throw_msg(env, "invalid or closed statement handle");
            Err(Ok(default))
        }
    }
}

extern "system" fn native_execute<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, ()) {
            Ok(v) => v,
            Err(d) => return d,
        };
        step_then_reset(env, db, s);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_execute_for_long<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, 0i64) {
            Ok(v) => v,
            Err(d) => return d,
        };
        let value = match step_then_reset(env, db, s) {
            Some(SQLITE_ROW) => unsafe { ffi::sqlite3_column_int64(s, 0) },
            Some(_) => 0,
            None => return Ok(0),
        };
        Ok(value)
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_execute_for_string<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
) -> JString<'l> {
    env.with_env(|env| -> jni::errors::Result<JString<'l>> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, null_string()) {
            Ok(v) => v,
            Err(d) => return d,
        };
        match step_then_reset(env, db, s) {
            Some(SQLITE_ROW) => {
                // SAFETY: `s` is live and stepped to a row; column_text returns NUL-terminated UTF-8
                // valid until the next step/reset; null = SQL NULL → return a Java null.
                let raw = unsafe { ffi::sqlite3_column_text(s, 0) };
                if raw.is_null() {
                    return Ok(null_string());
                }
                let text = unsafe { CStr::from_ptr(raw as *const std::os::raw::c_char) }
                    .to_string_lossy()
                    .into_owned();
                env.new_string(text)
            }
            _ => Ok(null_string()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_execute_for_changed_row_count<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, -1i32) {
            Ok(v) => v,
            Err(d) => return d,
        };
        match step_then_reset(env, db, s) {
            // SAFETY: `db` is a live connection; sqlite3_changes reports rows changed by the last step.
            Some(_) => Ok(unsafe { ffi::sqlite3_changes(db) }),
            None => Ok(-1),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_execute_for_last_inserted_row_id<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, -1i64) {
            Ok(v) => v,
            Err(d) => return d,
        };
        match step_then_reset(env, db, s) {
            // SAFETY: `db` is live; last_insert_rowid reports the most recent INSERT's rowid.
            Some(_) => Ok(unsafe { ffi::sqlite3_last_insert_rowid(db) }),
            None => Ok(-1),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_execute_for_blob_file_descriptor<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    _stmt: jlong,
) -> jint {
    // Rarely hit (an incremental-BLOB-over-fd read). Eclipse does not back the ashmem fd path; -1 tells
    // the Java side "no fd", which it handles by falling back to a normal blob read.
    env.with_env(|_env| -> jni::errors::Result<jint> { Ok(-1) })
        .resolve::<LogErrorAndDefault>()
}

/// The current row's column `col` as a Rust `String` (UTF-8). Empty for NULL/empty.
fn column_text(stmt: *mut ffi::sqlite3_stmt, col: c_int) -> String {
    // SAFETY: `stmt` is a live statement stepped to a row; column_text/column_bytes return the UTF-8
    // text + its byte length, valid until the next step/reset.
    unsafe {
        let ptr = ffi::sqlite3_column_text(stmt, col);
        if ptr.is_null() {
            return String::new();
        }
        let len = ffi::sqlite3_column_bytes(stmt, col).max(0) as usize;
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
    }
}

/// The current row's column `col` as raw blob bytes.
fn column_blob(stmt: *mut ffi::sqlite3_stmt, col: c_int) -> Vec<u8> {
    // SAFETY: as `column_text` — blob ptr + byte length valid until the next step/reset.
    unsafe {
        let ptr = ffi::sqlite3_column_blob(stmt, col);
        let len = ffi::sqlite3_column_bytes(stmt, col).max(0) as usize;
        if ptr.is_null() || len == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr as *const u8, len).to_vec()
    }
}

/// `nativeExecuteForCursorWindow(connPtr, stmtPtr, CursorWindow window, startPos, requiredPos,
/// countAllRows)` → `(actualStartPos << 32) | totalRows`. Steps the statement and FILLS the **Java**
/// `CursorWindow` (ATL's is a pure-Java `ArrayList<Object[]>`, NOT a native buffer) via its Java methods
/// (`clear`/`setNumColumns`/`setStartPosition`/`allocRow`/`put{Long,Double,String,Blob,Null}`). ATL's
/// window is unbounded, so all rows from `startPos` are filled in one pass; `put*` takes the ABSOLUTE
/// row index (the window subtracts its `startPos` internally). Per-row local frames free the transient
/// `JString`/`JByteArray` refs so a large result set never overflows the local-reference table.
extern "system" fn native_execute_for_cursor_window<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    stmt: jlong,
    window: JObject<'l>,
    start_pos: jint,
    _required_pos: jint,
    _count_all_rows: jboolean,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let (db, s) = match require_conn_stmt(env, conn, stmt, 0i64) {
            Ok(v) => v,
            Err(d) => return d,
        };
        // SAFETY: `s` is a live statement; column_count is valid before/after stepping.
        let col_count = unsafe { ffi::sqlite3_column_count(s) };
        env.call_method(&window, jni_str!("clear"), jni_sig!("()V"), &[])?;
        env.call_method(
            &window,
            jni_str!("setNumColumns"),
            jni_sig!("(I)Z"),
            &[JValue::Int(col_count)],
        )?;
        env.call_method(
            &window,
            jni_str!("setStartPosition"),
            jni_sig!("(I)V"),
            &[JValue::Int(start_pos)],
        )?;

        let mut total_rows: i32 = 0;
        loop {
            // SAFETY: `s` is a live statement; step advances to the next row / DONE / an error code.
            let rc = unsafe { ffi::sqlite3_step(s) };
            if rc == SQLITE_DONE {
                break;
            }
            if rc != SQLITE_ROW {
                let r = throw_sqlite(env, db, 0);
                // SAFETY: reset so the statement is reusable/finalizable after the error.
                unsafe { ffi::sqlite3_reset(s) };
                return r;
            }
            if total_rows >= start_pos {
                let row = total_rows;
                // One local frame per row frees the transient JString/JByteArray column refs.
                env.with_local_frame(8, |env| -> jni::errors::Result<()> {
                    env.call_method(&window, jni_str!("allocRow"), jni_sig!("()Z"), &[])?;
                    for col in 0..col_count {
                        // SAFETY: `s` is stepped to a row; column_type is the dynamic type at (row,col).
                        let ctype = unsafe { ffi::sqlite3_column_type(s, col) };
                        match ctype {
                            SQLITE_INTEGER => {
                                // SAFETY: column is INTEGER → column_int64 is valid.
                                let v = unsafe { ffi::sqlite3_column_int64(s, col) };
                                env.call_method(
                                    &window,
                                    jni_str!("putLong"),
                                    jni_sig!("(JII)Z"),
                                    &[JValue::Long(v), JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                            SQLITE_FLOAT => {
                                // SAFETY: column is FLOAT → column_double is valid.
                                let v = unsafe { ffi::sqlite3_column_double(s, col) };
                                env.call_method(
                                    &window,
                                    jni_str!("putDouble"),
                                    jni_sig!("(DII)Z"),
                                    &[JValue::Double(v), JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                            SQLITE_TEXT => {
                                let jstr = env.new_string(column_text(s, col))?;
                                env.call_method(
                                    &window,
                                    jni_str!("putString"),
                                    jni_sig!("(Ljava/lang/String;II)Z"),
                                    &[JValue::Object(&jstr), JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                            SQLITE_BLOB => {
                                let jarr = env.byte_array_from_slice(&column_blob(s, col))?;
                                env.call_method(
                                    &window,
                                    jni_str!("putBlob"),
                                    jni_sig!("([BII)Z"),
                                    &[JValue::Object(&jarr), JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                            _ => {
                                // SQLITE_NULL (or any unexpected): store a NULL field.
                                env.call_method(
                                    &window,
                                    jni_str!("putNull"),
                                    jni_sig!("(II)Z"),
                                    &[JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                        }
                    }
                    Ok(())
                })?;
            }
            total_rows = total_rows.saturating_add(1);
        }
        // SAFETY: rewind the statement for reuse (AOSP resets after filling the window).
        unsafe { ffi::sqlite3_reset(s) };
        // Pack (actualStartPos << 32) | totalRows. We filled from `start_pos`, so actualStartPos = start_pos.
        Ok((i64::from(start_pos) << 32) | i64::from(total_rows) & 0xFFFF_FFFF)
    })
    .resolve::<LogErrorAndDefault>()
}

/// A byte-lexicographic comparator registered as the "LOCALIZED"/"UNICODE" collations so a
/// `REINDEX LOCALIZED` (SQLiteDatabase.setLocale) and any `COLLATE LOCALIZED` resolve. NOT
/// locale-accurate (AOSP uses ICU); it is a valid TOTAL order, which is all SQLite requires, and
/// Roblox's startup DBs (WorkManager / jobqueue) don't depend on locale-correct string ordering.
///
/// # Safety
/// SQLite calls this with two byte buffers and their lengths; the pointers are valid for the lengths
/// given for the duration of the call (the SQLite collation-callback contract).
unsafe extern "C" fn collate_bytes(
    _ctx: *mut std::ffi::c_void,
    len1: c_int,
    p1: *const std::ffi::c_void,
    len2: c_int,
    p2: *const std::ffi::c_void,
) -> c_int {
    // SAFETY: per the collation-callback contract, `pN` is valid for `lenN` bytes for this call.
    let a = unsafe { std::slice::from_raw_parts(p1 as *const u8, len1.max(0) as usize) };
    let b = unsafe { std::slice::from_raw_parts(p2 as *const u8, len2.max(0) as usize) };
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Register the "LOCALIZED" and "UNICODE" collations on `db` (idempotent — re-registering replaces).
fn register_localized_collations(db: *mut ffi::sqlite3) {
    for name in [c"LOCALIZED", c"UNICODE"] {
        // SAFETY: `db` is a live connection; `name` is a 'static NUL-terminated collation name;
        // `collate_bytes` is a valid comparator; no per-collation user data / destroy callback.
        unsafe {
            ffi::sqlite3_create_collation_v2(
                db,
                name.as_ptr(),
                SQLITE_UTF8,
                std::ptr::null_mut(),
                Some(collate_bytes),
                None,
            );
        }
    }
}

extern "system" fn native_register_localized_collators<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    conn: jlong,
    _locale: JString<'l>,
) {
    // SQLiteDatabase.setLocale calls this, then runs `REINDEX LOCALIZED`. Register the "LOCALIZED"/
    // "UNICODE" collations on the connection so that REINDEX (and any `COLLATE LOCALIZED`) resolve.
    env.with_env(|env| -> jni::errors::Result<()> {
        match conn_get(conn) {
            Some(db) => register_localized_collations(db),
            None => throw_msg(env, "invalid or closed connection handle"),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_register_custom_function<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    _function: jni::objects::JObject<'l>,
) {
    // No-op: Eclipse registers no app-defined SQL functions (none are needed for Roblox's startup DBs).
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_get_db_lookaside<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
) -> jint {
    // Lookaside memory stat — informational only; 0 is a sound neutral value.
    env.with_env(|_env| -> jni::errors::Result<jint> { Ok(0) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_cancel<'l>(mut env: EnvUnowned<'l>, _cls: JClass<'l>, _conn: jlong) {
    // No-op: Eclipse does not implement statement cancellation (CancellationSignal). Operations run to
    // completion; nothing to interrupt.
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_reset_cancel<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    _cancelable: jboolean,
) {
    // No-op companion to native_cancel.
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

// === registration =================================================================================

/// `android.database.sqlite.SQLiteConnection` (slashed name for `find_class`).
const SQLITE_CONNECTION_CLASS: &JNIStr = jni_str!("android/database/sqlite/SQLiteConnection");

/// Bind Eclipse's libsqlite3-backed natives for the `SQLiteConnection` surface (open + statement
/// lifecycle + executes + `nativeExecuteForCursorWindow`). Registered before the lifecycle so they are
/// bound before `ActivitySplash.onCreate`'s DB open.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match its declared JNI signature. They do,
/// by construction (the descriptors are verbatim from ATL's `SQLiteConnection.java`). Each body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary, and
/// the `jlong` handles are generational-slab indices (a stale/fabricated handle throws, never UB).
pub fn register_natives(env: &mut Env) -> Result<(), super::FrameworkError> {
    let class = env.find_class(SQLITE_CONNECTION_CLASS)?;
    // (name, signature, fn pointer). Signatures verbatim from ATL's SQLiteConnection.java.
    let m: [(&JNIStr, &JNIStr, *mut std::ffi::c_void); 24] = [
        (
            jni_str!("nativeOpen"),
            jni_str!("(Ljava/lang/String;ILjava/lang/String;ZZ)J"),
            native_open as *mut _,
        ),
        (
            jni_str!("nativeHasCodec"),
            jni_str!("()Z"),
            native_has_codec as *mut _,
        ),
        (
            jni_str!("nativeClose"),
            jni_str!("(J)V"),
            native_close as *mut _,
        ),
        (
            jni_str!("nativePrepareStatement"),
            jni_str!("(JLjava/lang/String;)J"),
            native_prepare_statement as *mut _,
        ),
        (
            jni_str!("nativeFinalizeStatement"),
            jni_str!("(JJ)V"),
            native_finalize_statement as *mut _,
        ),
        (
            jni_str!("nativeGetParameterCount"),
            jni_str!("(JJ)I"),
            native_get_parameter_count as *mut _,
        ),
        (
            jni_str!("nativeIsReadOnly"),
            jni_str!("(JJ)Z"),
            native_is_read_only as *mut _,
        ),
        (
            jni_str!("nativeGetColumnCount"),
            jni_str!("(JJ)I"),
            native_get_column_count as *mut _,
        ),
        (
            jni_str!("nativeGetColumnName"),
            jni_str!("(JJI)Ljava/lang/String;"),
            native_get_column_name as *mut _,
        ),
        (
            jni_str!("nativeBindNull"),
            jni_str!("(JJI)V"),
            native_bind_null as *mut _,
        ),
        (
            jni_str!("nativeBindLong"),
            jni_str!("(JJIJ)V"),
            native_bind_long as *mut _,
        ),
        (
            jni_str!("nativeBindDouble"),
            jni_str!("(JJID)V"),
            native_bind_double as *mut _,
        ),
        (
            jni_str!("nativeBindString"),
            jni_str!("(JJILjava/lang/String;)V"),
            native_bind_string as *mut _,
        ),
        (
            jni_str!("nativeBindBlob"),
            jni_str!("(JJI[B)V"),
            native_bind_blob as *mut _,
        ),
        (
            jni_str!("nativeResetStatementAndClearBindings"),
            jni_str!("(JJ)V"),
            native_reset_statement_and_clear_bindings as *mut _,
        ),
        (
            jni_str!("nativeExecute"),
            jni_str!("(JJ)V"),
            native_execute as *mut _,
        ),
        (
            jni_str!("nativeExecuteForLong"),
            jni_str!("(JJ)J"),
            native_execute_for_long as *mut _,
        ),
        (
            jni_str!("nativeExecuteForString"),
            jni_str!("(JJ)Ljava/lang/String;"),
            native_execute_for_string as *mut _,
        ),
        (
            jni_str!("nativeExecuteForBlobFileDescriptor"),
            jni_str!("(JJ)I"),
            native_execute_for_blob_file_descriptor as *mut _,
        ),
        (
            jni_str!("nativeExecuteForChangedRowCount"),
            jni_str!("(JJ)I"),
            native_execute_for_changed_row_count as *mut _,
        ),
        (
            jni_str!("nativeExecuteForLastInsertedRowId"),
            jni_str!("(JJ)J"),
            native_execute_for_last_inserted_row_id as *mut _,
        ),
        (
            jni_str!("nativeExecuteForCursorWindow"),
            jni_str!("(JJLandroid/database/CursorWindow;IIZ)J"),
            native_execute_for_cursor_window as *mut _,
        ),
        (
            jni_str!("nativeGetDbLookaside"),
            jni_str!("(J)I"),
            native_get_db_lookaside as *mut _,
        ),
        (
            jni_str!("nativeRegisterLocalizedCollators"),
            jni_str!("(JLjava/lang/String;)V"),
            native_register_localized_collators as *mut _,
        ),
    ];
    let methods: Vec<NativeMethod> = m
        .iter()
        // SAFETY: each fn matches its paired signature (verbatim from SQLiteConnection.java).
        .map(|(n, s, p)| unsafe { NativeMethod::from_raw_parts(n, s, *p) })
        .collect();
    // SAFETY: `class` is the loaded SQLiteConnection; the methods hold valid fn pointers matching the
    // class's `native` declarations.
    unsafe { env.register_native_methods(&class, &methods) }?;

    // The remaining declared natives (nativeRegisterCustomFunction / nativeCancel / nativeResetCancel)
    // are bound best-effort (a sig drift on any must not drop the core set above).
    let extra: [(&JNIStr, &JNIStr, *mut std::ffi::c_void); 3] = [
        (
            jni_str!("nativeRegisterCustomFunction"),
            jni_str!("(JLandroid/database/sqlite/SQLiteCustomFunction;)V"),
            native_register_custom_function as *mut _,
        ),
        (
            jni_str!("nativeCancel"),
            jni_str!("(J)V"),
            native_cancel as *mut _,
        ),
        (
            jni_str!("nativeResetCancel"),
            jni_str!("(JZ)V"),
            native_reset_cancel as *mut _,
        ),
    ];
    for (n, s, p) in extra {
        // SAFETY: fn matches signature; a NoSuchMethodError is cleared best-effort.
        let nm = unsafe { NativeMethod::from_raw_parts(n, s, p) };
        if unsafe { env.register_native_methods(&class, std::slice::from_ref(&nm)) }.is_err()
            && env.exception_check()
        {
            env.exception_clear();
        }
    }

    tracing::info!(
        class = "android/database/sqlite/SQLiteConnection",
        "registered Eclipse's libsqlite3-backed natives (open + statement lifecycle + executes + cursor window)"
    );
    Ok(())
}
