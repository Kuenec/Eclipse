
#include <cstddef>
#include <cstring>
#include <string>

extern "C" {

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

}
