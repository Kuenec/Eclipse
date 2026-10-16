use std::ffi::CString;

use super::native_provider::{
    last_dl_error, EclipseNativeProvider, HOST_EGL_SONAME, HOST_GLESV2_SONAME,
};
use super::resolve::{dlsym_bionic_import, HostDlsymProvider, ResolvedSym, Scope, SymbolProvider};

#[cfg(test)]
pub(crate) mod import_census;

pub struct BionicEnv {
    scope: Scope,
}

impl BionicEnv {
    pub fn with_host_baseline(try_host_gl: bool, eclipse_natives: bool) -> Self {
        let mut scope = Scope::new();

        if eclipse_natives {
            scope.push(Box::new(EclipseNativeProvider::with_bionic_natives()));
        }

        if try_host_gl {
            for opened in open_host_gl_libraries() {
                match opened {
                    Ok(p) => {
                        scope.push(Box::new(p));
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "eclipse::loader",
                            %error,
                            "host GL library unavailable"
                        );
                    }
                }
            }
        }

        scope.push(Box::new(HostDlsymProvider));

        Self { scope }
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn into_scope(self) -> Scope {
        self.scope
    }

    pub fn into_providers(self) -> Vec<Box<dyn SymbolProvider>> {
        self.scope.into_providers()
    }
}

fn open_host_gl_libraries() -> [Result<DlopenLibProvider, String>; 2] {
    [HOST_EGL_SONAME, HOST_GLESV2_SONAME].map(DlopenLibProvider::open)
}

pub struct DlopenLibProvider {
    handle: HostLibHandle,
}

struct HostLibHandle(*mut libc::c_void);

unsafe impl Send for HostLibHandle {}

unsafe impl Sync for HostLibHandle {}

impl DlopenLibProvider {
    pub fn open(soname: &str) -> Result<Self, String> {
        let cname = CString::new(soname).map_err(|e| format!("dlopen({soname}): {e}"))?;

        let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!("dlopen({soname}) failed: {}", last_dl_error()));
        }
        Ok(Self {
            handle: HostLibHandle(handle),
        })
    }
}

impl SymbolProvider for DlopenLibProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        unsafe { dlsym_bionic_import(self.handle.0, name) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bionic_env_host_baseline_has_libc_tier() {
        let env = BionicEnv::with_host_baseline(false, false);
        let got = env.scope().resolve("memcpy");
        assert!(got.is_some_and(|r| r.addr != 0));
    }

    #[test]
    fn bionic_env_eclipse_natives_win_over_host() {
        let env = BionicEnv::with_host_baseline(false, true);
        assert!(
            env.scope().resolve("__errno").is_some_and(|r| r.addr != 0),
            "the Eclipse tier must resolve __errno (glibc lacks this exact name)"
        );

        assert!(env
            .scope()
            .resolve("__strlen_chk")
            .is_some_and(|r| r.addr != 0));

        assert!(env.scope().resolve("memcpy").is_some_and(|r| r.addr != 0));
    }

    #[test]
    fn dlopen_provider_resolves_from_libc() {
        let p = match DlopenLibProvider::open("libc.so.6") {
            Ok(p) => p,
            Err(e) => {
                eprintln!("dlopen_provider_resolves_from_libc: {e}; skipping");
                return;
            }
        };
        assert!(p.resolve("memcpy").is_some_and(|r| r.addr != 0));
        assert_eq!(p.resolve("__eclipse_no_such_symbol_zzz__"), None);

        assert_eq!(p.resolve("bad\0name"), None);
    }

    #[test]
    fn dlopen_provider_maps_long_double_names_its_dependencies_export_to_binary128() {
        let p = DlopenLibProvider::open("libm.so.6").expect("glibc's libm must open");
        let host = |name: &std::ffi::CStr| unsafe {
            libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) as u64
        };
        assert_eq!(
            p.resolve("strtold").map(|s| s.addr),
            Some(host(c"strtof128")),
            "libc's strtold reached through libm's NEEDED entry must be the binary128 parser"
        );
        for name in ["frexpl", "__isnanl", "logl"] {
            assert_eq!(
                p.resolve(name),
                None,
                "{name} must not bind to glibc's x87 version"
            );
        }
        assert_eq!(p.resolve("cos").map(|s| s.addr), Some(host(c"cos")));
    }

    #[test]
    fn dlopen_provider_absent_lib_reports_the_dlerror_cause() {
        let Err(e) = DlopenLibProvider::open("libeclipse_definitely_no_such_lib_4f2a.so") else {
            panic!("an absent library must not open");
        };
        assert!(
            e.contains("libeclipse_definitely_no_such_lib_4f2a.so")
                && e.contains("cannot open shared object file"),
            "the error must name the library and the loader's cause: {e}"
        );
    }

    #[test]
    fn host_gl_libraries_open_by_their_runtime_sonames() {
        let expected = [
            (HOST_EGL_SONAME, "eglGetDisplay"),
            (HOST_GLESV2_SONAME, "glDrawArrays"),
        ];
        for (opened, (soname, symbol)) in open_host_gl_libraries().into_iter().zip(expected) {
            let provider = match opened {
                Ok(provider) => provider,
                Err(e) => {
                    assert!(
                        e.starts_with(&format!("dlopen({soname}) failed"))
                            && e.contains("cannot open shared object file"),
                        "only an absent host library may fail, and it must be the runtime soname: {e}"
                    );
                    continue;
                }
            };
            let addr = provider
                .resolve(symbol)
                .unwrap_or_else(|| panic!("{soname} must export {symbol}"))
                .addr;
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            assert_ne!(
                unsafe { libc::dladdr(addr as *const libc::c_void, &mut info) },
                0,
                "dladdr must find the object that defines {symbol}"
            );
            let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }
                .to_string_lossy()
                .into_owned();
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let image = crate::loader::elf::ElfImage::parse(&bytes)
                .unwrap_or_else(|e| panic!("parse {path}: {e:?}"));
            assert_eq!(
                image.soname().expect("DT_SONAME decode").as_deref(),
                Some(soname),
                "{path} is the runtime library, so its DT_SONAME must be the name Eclipse opens"
            );
        }
    }
}
