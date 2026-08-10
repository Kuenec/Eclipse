#![allow(clippy::not_unsafe_ptr_arg_deref)]

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

fn null_string<'l>() -> JString<'l> {
    <JString as Reference>::null()
}

fn throw_msg(env: &mut Env, msg: &str) {
    let _ = env.throw_new(SQLITE_EXCEPTION_CLASS, JNIString::from(msg));
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_UTF8: c_int = 1;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;

const ANDROID_OPEN_READONLY: jint = 0x0000_0001;
const ANDROID_CREATE_IF_NECESSARY: jint = 0x1000_0000;

const SQLITE_EXCEPTION_CLASS: &JNIStr = jni_str!("android/database/sqlite/SQLiteException");

struct SendPtr<T>(*mut T);

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

    fn get(&self, handle: jlong) -> Option<*mut T> {
        let (index, generation) = unpack(handle);
        let slot = self.slots.get(index as usize)?;
        if slot.generation != generation {
            return None;
        }
        slot.ptr.as_ref().map(|p| p.0)
    }

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

fn throw_sqlite<T>(env: &mut Env, db: *mut ffi::sqlite3, default: T) -> jni::errors::Result<T> {
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

        let sqlite_flags = if open_flags & ANDROID_CREATE_IF_NECESSARY != 0 {
            SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE
        } else if open_flags & ANDROID_OPEN_READONLY != 0 {
            SQLITE_OPEN_READONLY
        } else {
            SQLITE_OPEN_READWRITE
        };
        let mut db: *mut ffi::sqlite3 = std::ptr::null_mut();

        let rc = unsafe {
            ffi::sqlite3_open_v2(c_path.as_ptr(), &mut db, sqlite_flags, std::ptr::null())
        };
        if rc != SQLITE_OK {
            let r = throw_sqlite(env, db, 0);

            unsafe { ffi::sqlite3_close(db) };
            return r;
        }

        unsafe { ffi::sqlite3_busy_timeout(db, 2500) };

        register_localized_collations(db);
        Ok(connections()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(db))
    })
    .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_has_codec<'l>(mut env: EnvUnowned<'l>, _cls: JClass<'l>) -> jboolean {
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

        unsafe {
            ffi::sqlite3_reset(s);
            ffi::sqlite3_clear_bindings(s);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

fn step_then_reset(
    env: &mut Env,
    db: *mut ffi::sqlite3,
    stmt: *mut ffi::sqlite3_stmt,
) -> Option<c_int> {
    let rc = unsafe { ffi::sqlite3_step(stmt) };
    if rc != SQLITE_ROW && rc != SQLITE_DONE {
        let _ = throw_sqlite(env, db, ());

        unsafe { ffi::sqlite3_reset(stmt) };
        return None;
    }
    Some(rc)
}

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
    env.with_env(|_env| -> jni::errors::Result<jint> { Ok(-1) })
        .resolve::<LogErrorAndDefault>()
}

fn column_text(stmt: *mut ffi::sqlite3_stmt, col: c_int) -> String {
    unsafe {
        let ptr = ffi::sqlite3_column_text(stmt, col);
        if ptr.is_null() {
            return String::new();
        }
        let len = ffi::sqlite3_column_bytes(stmt, col).max(0) as usize;
        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
    }
}

fn column_blob(stmt: *mut ffi::sqlite3_stmt, col: c_int) -> Vec<u8> {
    unsafe {
        let ptr = ffi::sqlite3_column_blob(stmt, col);
        let len = ffi::sqlite3_column_bytes(stmt, col).max(0) as usize;
        if ptr.is_null() || len == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr as *const u8, len).to_vec()
    }
}

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
            let rc = unsafe { ffi::sqlite3_step(s) };
            if rc == SQLITE_DONE {
                break;
            }
            if rc != SQLITE_ROW {
                let r = throw_sqlite(env, db, 0);

                unsafe { ffi::sqlite3_reset(s) };
                return r;
            }
            if total_rows >= start_pos {
                let row = total_rows;

                env.with_local_frame(8, |env| -> jni::errors::Result<()> {
                    env.call_method(&window, jni_str!("allocRow"), jni_sig!("()Z"), &[])?;
                    for col in 0..col_count {
                        let ctype = unsafe { ffi::sqlite3_column_type(s, col) };
                        match ctype {
                            SQLITE_INTEGER => {
                                let v = unsafe { ffi::sqlite3_column_int64(s, col) };
                                env.call_method(
                                    &window,
                                    jni_str!("putLong"),
                                    jni_sig!("(JII)Z"),
                                    &[JValue::Long(v), JValue::Int(row), JValue::Int(col)],
                                )?;
                            }
                            SQLITE_FLOAT => {
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

        unsafe { ffi::sqlite3_reset(s) };

        Ok((i64::from(start_pos) << 32) | i64::from(total_rows) & 0xFFFF_FFFF)
    })
    .resolve::<LogErrorAndDefault>()
}

unsafe extern "C" fn collate_bytes(
    _ctx: *mut std::ffi::c_void,
    len1: c_int,
    p1: *const std::ffi::c_void,
    len2: c_int,
    p2: *const std::ffi::c_void,
) -> c_int {
    let a = unsafe { std::slice::from_raw_parts(p1 as *const u8, len1.max(0) as usize) };
    let b = unsafe { std::slice::from_raw_parts(p2 as *const u8, len2.max(0) as usize) };
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

fn register_localized_collations(db: *mut ffi::sqlite3) {
    for name in [c"LOCALIZED", c"UNICODE"] {
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
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_get_db_lookaside<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> { Ok(0) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_cancel<'l>(mut env: EnvUnowned<'l>, _cls: JClass<'l>, _conn: jlong) {
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

extern "system" fn native_reset_cancel<'l>(
    mut env: EnvUnowned<'l>,
    _cls: JClass<'l>,
    _conn: jlong,
    _cancelable: jboolean,
) {
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

const SQLITE_CONNECTION_CLASS: &JNIStr = jni_str!("android/database/sqlite/SQLiteConnection");

pub fn register_natives(env: &mut Env) -> Result<(), super::FrameworkError> {
    let class = env.find_class(SQLITE_CONNECTION_CLASS)?;

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
        .map(|(n, s, p)| unsafe { NativeMethod::from_raw_parts(n, s, *p) })
        .collect();

    unsafe { env.register_native_methods(&class, &methods) }?;

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
