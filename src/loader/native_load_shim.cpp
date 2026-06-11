// Eclipse — clean-room C++ delegation shim for ART's `JavaVMExt::LoadNativeLibrary`. 2026-06-11.
//
// WHY this exists (root cause it enables fixing): ART's `System.loadLibrary(name)` →
// `Runtime.nativeLoad(path, loader, caller)` → `art::JavaVMExt::LoadNativeLibrary(...)` →
// `bionic_dlopen(path)` (the apkenv shim linker). For the app's engine JNI libs (e.g. `libzstd-jni`)
// apkenv ABORTS — it cannot apply their modern relocations / its dependency-graph walk NULL-derefs
// (docs/libroblox-init-run.md §9-§11). Eclipse PRE-LOADS those libs through its own Rust loader, so
// its `Runtime.nativeLoad` interception (src/framework.rs) reports a pre-loaded soname as
// already-loaded and skips apkenv. Every OTHER library (e.g. `libwolfssljni`, a discovery-based lib
// loaded during cert verification) must still load through ART's NORMAL path — handle in
// `libraries_`, `JNI_OnLoad`, `Java_*` discovery — so the interception DELEGATES those to ART's real
// `LoadNativeLibrary`.
//
// WHY a C++ shim (not pure Rust): `LoadNativeLibrary` takes `const std::string&` / `std::string*`.
// Hand-building a libstdc++ `std::string` in Rust would hard-code a fragile internal ABI. This shim
// constructs the `std::string` args with the SAME host libstdc++ ART links, so the ABI is correct by
// construction. It calls the function through a pointer Rust `dlsym`s at runtime (libart is opened
// RTLD_GLOBAL by `runtime::boot`), so the shim has NO build-time dependency on libart — the address
// is passed in as a `void*`.
//
// ABI note: `art::JavaVMExt::LoadNativeLibrary` is a NON-virtual member function, so its code address
// (from `dlsym` of the mangled symbol) is callable as a free function whose first explicit argument
// is the `this` pointer (the Itanium C++ ABI). ART hands out `JavaVM*` that point at the `JavaVMExt`
// object (`JavaVMExt : public JavaVM`), so `this == the process JavaVM*`.
//
// Compiled by `build.rs` via the `cc` crate (the established shim pattern — see `liblog_shim.c` /
// `bionic_syscall_shim.c`). No global state, reentrant, no UB: it only forwards pointers/handles the
// caller owns and copies a bounded error string out.

#include <cstddef>
#include <cstring>
#include <string>

extern "C" {

// Delegate one `nativeLoad` to ART's real loader.
//
//   load_fn       — runtime-`dlsym`'d address of `art::JavaVMExt::LoadNativeLibrary`.
//   vm            — the `JavaVMExt* this` (== the process `JavaVM*`).
//   env           — the calling `JNIEnv*`.
//   path          — NUL-terminated resolved library path.
//   class_loader  — the `jobject` ClassLoader argument (may be null = boot class loader).
//   caller_class  — the `jclass` caller argument (may be null).
//   err_buf/err_cap — on failure, the error message is copied here (NUL-terminated, truncated).
//
// Returns 1 if the library loaded (ART returned true), 0 on failure (with `err_buf` filled).
int eclipse_art_load_native_library(void* load_fn,
                                    void* vm,
                                    void* env,
                                    const char* path,
                                    void* class_loader,
                                    void* caller_class,
                                    char* err_buf,
                                    size_t err_cap) {
    if (load_fn == nullptr) {
        if (err_buf != nullptr && err_cap > 0) {
            std::strncpy(err_buf, "Eclipse: ART LoadNativeLibrary address is null", err_cap - 1);
            err_buf[err_cap - 1] = '\0';
        }
        return 0;
    }

    // `LoadNativeLibrary(this, JNIEnv*, const std::string& path, jobject loader, jclass caller,
    //  std::string* error_msg) -> bool`, called as a free fn with explicit `this` (Itanium ABI).
    typedef bool (*LoadNativeLibraryFn)(void* self,
                                        void* env,
                                        const std::string& path,
                                        void* class_loader,
                                        void* caller_class,
                                        std::string* error_msg);
    LoadNativeLibraryFn fn = reinterpret_cast<LoadNativeLibraryFn>(load_fn);

    std::string p(path != nullptr ? path : "");
    std::string error;
    bool ok = fn(vm, env, p, class_loader, caller_class, &error);

    if (!ok && err_buf != nullptr && err_cap > 0) {
        std::strncpy(err_buf, error.c_str(), err_cap - 1);
        err_buf[err_cap - 1] = '\0';
    }
    return ok ? 1 : 0;
}

}  // extern "C"
