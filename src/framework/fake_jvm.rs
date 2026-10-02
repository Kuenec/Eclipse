use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::mem::{offset_of, size_of};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use jni::objects::JObject;
use jni::sys::{
    jboolean, jclass, jint, jmethodID, jobject, jstring, jthrowable, jvalue, jweak, JNIEnv,
    JNIInvokeInterface_, JNIInvokeInterface__1_2, JNINativeInterface_, JNINativeInterface__1_6,
    JNINativeMethod, JavaVM, JNI_OK, JNI_VERSION_1_6,
};
use jni::{Env, EnvUnowned, Outcome};

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Local,
    Global,
    Weak,
}

struct RefEntry {
    object: usize,
    kind: RefKind,
}

struct ObjectEntry {
    text: Option<String>,
    collected: bool,
}

#[derive(Default)]
struct Heap {
    next_ref: usize,
    refs: HashMap<usize, RefEntry>,
    objects: Vec<ObjectEntry>,
    methods: Vec<String>,
    hooks: HashMap<(usize, String), Hook>,
    natives: HashMap<(String, String, String), usize>,
}

impl Heap {
    fn new_local(&mut self, text: Option<String>) -> jobject {
        self.objects.push(ObjectEntry {
            text,
            collected: false,
        });
        self.add_ref(self.objects.len() - 1, RefKind::Local)
    }

    fn add_ref(&mut self, object: usize, kind: RefKind) -> jobject {
        self.next_ref += 1;
        let raw = self.next_ref << 4;
        self.refs.insert(raw, RefEntry { object, kind });
        raw as jobject
    }

    fn live_object(&self, raw: jobject) -> Option<usize> {
        let entry = self.refs.get(&(raw as usize))?;
        (!self.objects[entry.object].collected).then_some(entry.object)
    }

    fn copy_ref(&mut self, raw: jobject, kind: RefKind) -> jobject {
        match self.live_object(raw) {
            Some(object) => self.add_ref(object, kind),
            None => std::ptr::null_mut(),
        }
    }

    fn text(&self, raw: jobject) -> Option<String> {
        self.live_object(raw)
            .and_then(|object| self.objects[object].text.clone())
    }

    fn method_name(&self, method: jmethodID) -> String {
        self.methods[(method as usize >> 4) - 1].clone()
    }

    fn intern_method(&mut self, name: *const c_char) -> jmethodID {
        let name = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        self.methods.push(name);
        (self.methods.len() << 4) as jmethodID
    }

    fn global_refs(&self, object: usize) -> usize {
        self.refs
            .values()
            .filter(|entry| entry.object == object && entry.kind == RefKind::Global)
            .count()
    }
}

fn heap() -> MutexGuard<'static, Heap> {
    static HEAP: OnceLock<Mutex<Heap>> = OnceLock::new();
    HEAP.get_or_init(|| Mutex::new(Heap::default()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    static PENDING_EXCEPTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

unsafe extern "system" fn unimplemented_function() {
    panic!("fake JVM: the code under test called a JNI function the fake does not implement");
}

unsafe extern "system" fn get_version(_env: *mut JNIEnv) -> jint {
    JNI_VERSION_1_6
}

unsafe extern "system" fn get_java_vm(_env: *mut JNIEnv, vm: *mut *mut JavaVM) -> jint {
    unsafe { *vm = vm_ptr() };
    JNI_OK
}

unsafe extern "system" fn find_class(_env: *mut JNIEnv, name: *const c_char) -> jclass {
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    heap().new_local(Some(name))
}

unsafe extern "system" fn new_string_utf(_env: *mut JNIEnv, utf: *const c_char) -> jstring {
    let text = unsafe { CStr::from_ptr(utf) }
        .to_string_lossy()
        .into_owned();
    heap().new_local(Some(text))
}

unsafe extern "system" fn get_string_utf_chars(
    _env: *mut JNIEnv,
    string: jstring,
    is_copy: *mut jboolean,
) -> *const c_char {
    if !is_copy.is_null() {
        unsafe { *is_copy = true };
    }
    let text = heap().text(string).unwrap_or_default();
    CString::new(text).map_or(std::ptr::null(), |chars| chars.into_raw().cast_const())
}

unsafe extern "system" fn release_string_utf_chars(
    _env: *mut JNIEnv,
    _string: jstring,
    chars: *const c_char,
) {
    if !chars.is_null() {
        drop(unsafe { CString::from_raw(chars.cast_mut()) });
    }
}

unsafe extern "system" fn is_assignable_from(
    _env: *mut JNIEnv,
    _class: jclass,
    _superclass: jclass,
) -> jboolean {
    true
}

unsafe extern "system" fn throw_new(_env: *mut JNIEnv, class: jclass, _msg: *const c_char) -> jint {
    let name = heap().text(class).unwrap_or_default();
    PENDING_EXCEPTION.with(|pending| *pending.borrow_mut() = Some(name));
    JNI_OK
}

unsafe extern "system" fn exception_occurred(_env: *mut JNIEnv) -> jthrowable {
    if !PENDING_EXCEPTION.with(|pending| pending.borrow().is_some()) {
        return std::ptr::null_mut();
    }
    heap().new_local(None)
}

unsafe extern "system" fn exception_describe(_env: *mut JNIEnv) {}

unsafe extern "system" fn exception_clear(_env: *mut JNIEnv) {
    PENDING_EXCEPTION.with(|pending| *pending.borrow_mut() = None);
}

unsafe extern "system" fn exception_check(_env: *mut JNIEnv) -> jboolean {
    PENDING_EXCEPTION.with(|pending| pending.borrow().is_some())
}

unsafe extern "system" fn push_local_frame(_env: *mut JNIEnv, _capacity: jint) -> jint {
    JNI_OK
}

unsafe extern "system" fn pop_local_frame(_env: *mut JNIEnv, result: jobject) -> jobject {
    heap().copy_ref(result, RefKind::Local)
}

unsafe extern "system" fn new_global_ref(_env: *mut JNIEnv, obj: jobject) -> jobject {
    heap().copy_ref(obj, RefKind::Global)
}

unsafe extern "system" fn new_local_ref(_env: *mut JNIEnv, obj: jobject) -> jobject {
    heap().copy_ref(obj, RefKind::Local)
}

unsafe extern "system" fn new_weak_global_ref(_env: *mut JNIEnv, obj: jobject) -> jweak {
    heap().copy_ref(obj, RefKind::Weak)
}

unsafe extern "system" fn delete_ref(_env: *mut JNIEnv, obj: jobject) {
    heap().refs.remove(&(obj as usize));
}

unsafe extern "system" fn is_same_object(_env: *mut JNIEnv, a: jobject, b: jobject) -> jboolean {
    let heap = heap();
    heap.live_object(a) == heap.live_object(b)
}

unsafe extern "system" fn get_object_class(_env: *mut JNIEnv, obj: jobject) -> jclass {
    let mut heap = heap();
    if heap.live_object(obj).is_none() {
        return std::ptr::null_mut();
    }
    heap.new_local(Some("java/lang/Object".to_owned()))
}

unsafe extern "system" fn get_method_id(
    _env: *mut JNIEnv,
    _class: jclass,
    name: *const c_char,
    _sig: *const c_char,
) -> jmethodID {
    heap().intern_method(name)
}

fn run_hook(obj: jobject, method: jmethodID) -> bool {
    let hook = {
        let heap = heap();
        let name = heap.method_name(method);
        heap.live_object(obj)
            .and_then(|object| heap.hooks.get(&(object, name)).cloned())
    };
    match hook {
        Some(hook) => {
            hook();
            true
        }
        None => false,
    }
}

unsafe extern "system" fn call_object_method_a(
    _env: *mut JNIEnv,
    obj: jobject,
    method: jmethodID,
    _args: *const jvalue,
) -> jobject {
    run_hook(obj, method);
    std::ptr::null_mut()
}

unsafe extern "system" fn call_boolean_method_a(
    _env: *mut JNIEnv,
    obj: jobject,
    method: jmethodID,
    _args: *const jvalue,
) -> jboolean {
    run_hook(obj, method)
}

unsafe extern "system" fn call_void_method_a(
    _env: *mut JNIEnv,
    obj: jobject,
    method: jmethodID,
    _args: *const jvalue,
) {
    run_hook(obj, method);
}

unsafe extern "system" fn new_object_a(
    _env: *mut JNIEnv,
    _class: jclass,
    _constructor: jmethodID,
    _args: *const jvalue,
) -> jobject {
    heap().new_local(None)
}

unsafe extern "system" fn get_static_method_id(
    _env: *mut JNIEnv,
    _class: jclass,
    name: *const c_char,
    _sig: *const c_char,
) -> jmethodID {
    heap().intern_method(name)
}

unsafe extern "system" fn call_static_object_method_a(
    _env: *mut JNIEnv,
    _class: jclass,
    method: jmethodID,
    args: *const jvalue,
) -> jobject {
    let mut heap = heap();
    match heap.method_name(method).as_str() {
        "currentThread" => heap.new_local(None),
        "forName" => {
            let binary_name = heap.text(unsafe { (*args).l }).unwrap_or_default();
            heap.new_local(Some(binary_name.replace('.', "/")))
        }
        _ => std::ptr::null_mut(),
    }
}

unsafe extern "system" fn register_natives(
    _env: *mut JNIEnv,
    class: jclass,
    methods: *const JNINativeMethod,
    count: jint,
) -> jint {
    let mut heap = heap();
    let class = heap.text(class).unwrap_or_default();
    let count = usize::try_from(count).unwrap_or(0);
    for method in unsafe { std::slice::from_raw_parts(methods, count) } {
        let text = |chars: *const c_char| {
            unsafe { CStr::from_ptr(chars) }
                .to_string_lossy()
                .into_owned()
        };
        heap.natives.insert(
            (class.clone(), text(method.name), text(method.signature)),
            method.fnPtr as usize,
        );
    }
    JNI_OK
}

unsafe extern "system" fn get_env(_vm: *mut JavaVM, env: *mut *mut c_void, _version: jint) -> jint {
    unsafe { *env = env_ptr().cast() };
    JNI_OK
}

fn slot(offset: usize) -> usize {
    offset / size_of::<usize>()
}

fn leak_interface(table: Vec<usize>) -> usize {
    let table: &'static [usize] = Box::leak(table.into_boxed_slice());
    let interface: &'static usize = Box::leak(Box::new(table.as_ptr() as usize));
    interface as *const usize as usize
}

fn env_ptr() -> *mut JNIEnv {
    type Table = JNINativeInterface__1_6;
    static ENV: OnceLock<usize> = OnceLock::new();
    *ENV.get_or_init(|| {
        let mut table = vec![
            unimplemented_function as *const () as usize;
            size_of::<JNINativeInterface_>() / size_of::<usize>()
        ];
        let functions = [
            (offset_of!(Table, GetVersion), get_version as *const ()),
            (offset_of!(Table, FindClass), find_class as *const ()),
            (
                offset_of!(Table, IsAssignableFrom),
                is_assignable_from as *const (),
            ),
            (offset_of!(Table, ThrowNew), throw_new as *const ()),
            (
                offset_of!(Table, ExceptionOccurred),
                exception_occurred as *const (),
            ),
            (
                offset_of!(Table, ExceptionDescribe),
                exception_describe as *const (),
            ),
            (
                offset_of!(Table, ExceptionClear),
                exception_clear as *const (),
            ),
            (
                offset_of!(Table, ExceptionCheck),
                exception_check as *const (),
            ),
            (
                offset_of!(Table, PushLocalFrame),
                push_local_frame as *const (),
            ),
            (
                offset_of!(Table, PopLocalFrame),
                pop_local_frame as *const (),
            ),
            (offset_of!(Table, NewGlobalRef), new_global_ref as *const ()),
            (offset_of!(Table, DeleteGlobalRef), delete_ref as *const ()),
            (offset_of!(Table, NewLocalRef), new_local_ref as *const ()),
            (offset_of!(Table, DeleteLocalRef), delete_ref as *const ()),
            (
                offset_of!(Table, NewWeakGlobalRef),
                new_weak_global_ref as *const (),
            ),
            (
                offset_of!(Table, DeleteWeakGlobalRef),
                delete_ref as *const (),
            ),
            (offset_of!(Table, IsSameObject), is_same_object as *const ()),
            (
                offset_of!(Table, GetObjectClass),
                get_object_class as *const (),
            ),
            (offset_of!(Table, GetMethodID), get_method_id as *const ()),
            (
                offset_of!(Table, CallObjectMethodA),
                call_object_method_a as *const (),
            ),
            (
                offset_of!(Table, CallBooleanMethodA),
                call_boolean_method_a as *const (),
            ),
            (
                offset_of!(Table, CallVoidMethodA),
                call_void_method_a as *const (),
            ),
            (offset_of!(Table, NewObjectA), new_object_a as *const ()),
            (
                offset_of!(Table, GetStaticMethodID),
                get_static_method_id as *const (),
            ),
            (
                offset_of!(Table, CallStaticObjectMethodA),
                call_static_object_method_a as *const (),
            ),
            (offset_of!(Table, NewStringUTF), new_string_utf as *const ()),
            (
                offset_of!(Table, GetStringUTFChars),
                get_string_utf_chars as *const (),
            ),
            (
                offset_of!(Table, ReleaseStringUTFChars),
                release_string_utf_chars as *const (),
            ),
            (offset_of!(Table, GetJavaVM), get_java_vm as *const ()),
            (
                offset_of!(Table, RegisterNatives),
                register_natives as *const (),
            ),
        ];
        for (offset, function) in functions {
            table[slot(offset)] = function as usize;
        }
        leak_interface(table)
    }) as *mut JNIEnv
}

fn vm_ptr() -> *mut JavaVM {
    static VM: OnceLock<usize> = OnceLock::new();
    *VM.get_or_init(|| {
        let mut table = vec![
            unimplemented_function as *const () as usize;
            size_of::<JNIInvokeInterface_>() / size_of::<usize>()
        ];
        table[slot(offset_of!(JNIInvokeInterface__1_2, GetEnv))] = get_env as *const () as usize;
        leak_interface(table)
    }) as *mut JavaVM
}

pub(super) fn native_env<'local>() -> EnvUnowned<'local> {
    unsafe { EnvUnowned::from_raw(env_ptr()) }
}

pub(super) fn with_env<T>(f: impl FnOnce(&mut Env) -> T) -> T {
    let mut unowned = native_env();
    match unowned
        .with_env_no_catch(|env| Ok::<T, jni::errors::Error>(f(env)))
        .into_outcome()
    {
        Outcome::Ok(value) => value,
        Outcome::Err(e) => panic!("fake JVM: attaching the test thread failed: {e}"),
        Outcome::Panic(payload) => std::panic::resume_unwind(payload),
    }
}

pub(super) fn new_object<'local>(env: &Env<'local>) -> JObject<'local> {
    let raw = heap().new_local(None);
    unsafe { JObject::from_raw(env, raw) }
}

fn object_of(obj: &JObject) -> usize {
    heap()
        .refs
        .get(&(obj.as_raw() as usize))
        .map(|entry| entry.object)
        .expect("fake JVM: reference was not created by the fake")
}

pub(super) fn on_call(obj: &JObject, method: &str, hook: impl Fn() + Send + Sync + 'static) {
    let object = object_of(obj);
    heap()
        .hooks
        .insert((object, method.to_owned()), Arc::new(hook));
}

pub(super) fn strong_refs(obj: &JObject) -> usize {
    let object = object_of(obj);
    heap().global_refs(object)
}

pub(super) fn collect(obj: &JObject) -> bool {
    let object = object_of(obj);
    let mut heap = heap();
    if heap.global_refs(object) > 0 {
        return false;
    }
    heap.objects[object].collected = true;
    true
}

pub(super) fn throw(class: &str) {
    PENDING_EXCEPTION.with(|pending| *pending.borrow_mut() = Some(class.to_owned()));
}

pub(super) fn take_exception() -> Option<String> {
    PENDING_EXCEPTION.with(|pending| pending.borrow_mut().take())
}

pub(super) fn registered_native(class: &str, name: &str, sig: &str) -> Option<usize> {
    heap()
        .natives
        .get(&(class.to_owned(), name.to_owned(), sig.to_owned()))
        .copied()
}
