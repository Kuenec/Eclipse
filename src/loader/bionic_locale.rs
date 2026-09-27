use std::ffi::{c_char, c_int, CStr};

enum BionicLocale {
    Ascii,
    Utf8,
}

impl BionicLocale {
    fn named(name: &CStr) -> Option<Self> {
        match name.to_bytes() {
            b"C" | b"POSIX" => Some(Self::Ascii),
            b"" | b"C.UTF-8" | b"en_US.UTF-8" => Some(Self::Utf8),
            _ => None,
        }
    }

    fn host_name(&self) -> &'static CStr {
        match self {
            Self::Ascii => c"C",
            Self::Utf8 => c"C.UTF-8",
        }
    }
}

fn set_errno(value: c_int) {
    unsafe { *libc::__errno_location() = value };
}

unsafe extern "C" fn eclipse_newlocale(
    mask: c_int,
    name: *const c_char,
    base: libc::locale_t,
) -> libc::locale_t {
    if mask & !libc::LC_ALL_MASK != 0 || name.is_null() {
        set_errno(libc::EINVAL);
        return std::ptr::null_mut();
    }
    let Some(locale) = BionicLocale::named(unsafe { CStr::from_ptr(name) }) else {
        set_errno(libc::ENOENT);
        return std::ptr::null_mut();
    };
    unsafe { libc::newlocale(mask, locale.host_name().as_ptr(), base) }
}

pub const LOCALE_NATIVE_COUNT: usize = 1;

pub fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    register("newlocale", eclipse_newlocale as *const () as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn __ctype_get_mb_cur_max() -> usize;
    }

    type NewLocale = unsafe extern "C" fn(c_int, *const c_char, libc::locale_t) -> libc::locale_t;

    const UNLOADABLE_HOST_LOCALE_CHILD: &str = "ECLIPSE_TEST_UNLOADABLE_HOST_LOCALE_CHILD";

    fn bionic_newlocale() -> NewLocale {
        let env = super::super::bionic_env::BionicEnv::with_host_baseline(false, true);
        let addr = env
            .scope()
            .resolve("newlocale")
            .expect("newlocale resolves")
            .addr;
        unsafe { std::mem::transmute::<usize, NewLocale>(addr as usize) }
    }

    fn mb_cur_max_of(newlocale: NewLocale, name: &CStr) -> usize {
        let locale = unsafe { newlocale(libc::LC_ALL_MASK, name.as_ptr(), std::ptr::null_mut()) };
        assert!(!locale.is_null(), "bionic accepts {name:?}");
        let previous = unsafe { libc::uselocale(locale) };
        let mb_cur_max = unsafe { __ctype_get_mb_cur_max() };
        unsafe {
            libc::uselocale(previous);
            libc::freelocale(locale);
        }
        mb_cur_max
    }

    #[test]
    fn bionic_locale_names_load_even_when_the_host_locale_cannot() {
        if std::env::var_os(UNLOADABLE_HOST_LOCALE_CHILD).is_none() {
            let output = std::process::Command::new(
                std::env::current_exe().expect("the test harness executable must have a path"),
            )
            .args([
                "--exact",
                "loader::bionic_locale::tests::\
                 bionic_locale_names_load_even_when_the_host_locale_cannot",
                "--test-threads=1",
            ])
            .env(UNLOADABLE_HOST_LOCALE_CHILD, "1")
            .env("LC_ALL", "xx_YY.UTF-8")
            .output()
            .expect("the unloadable-locale child must start");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "status={:?}, stdout={stdout}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let newlocale = bionic_newlocale();
        for name in [c"", c"C.UTF-8", c"en_US.UTF-8"] {
            assert!(mb_cur_max_of(newlocale, name) > 1, "{name:?} is UTF-8");
        }
        for name in [c"C", c"POSIX"] {
            assert_eq!(mb_cur_max_of(newlocale, name), 1, "{name:?} is ASCII");
        }
    }

    #[test]
    fn locale_names_bionic_does_not_ship_are_rejected_like_bionic() {
        let newlocale = bionic_newlocale();
        let rejected = unsafe {
            newlocale(
                libc::LC_ALL_MASK,
                c"en_US.utf8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        assert!(rejected.is_null());
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT)
        );

        let bad_mask = unsafe {
            newlocale(
                libc::LC_ALL_MASK | (1 << 20),
                c"C".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        assert!(bad_mask.is_null());
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL)
        );
    }
}
