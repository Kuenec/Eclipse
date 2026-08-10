use std::ffi::{c_int, c_long, c_ulong, c_void};
use std::sync::atomic::{AtomicU8, Ordering};

const SC_ARG_MAX: c_int = 0x0000;

const SC_CLK_TCK: c_int = 0x0002;

const SC_NGROUPS_MAX: c_int = 0x0003;

const SC_OPEN_MAX: c_int = 0x0004;

const SC_PAGESIZE: c_int = 0x0027;

const SC_PAGE_SIZE: c_int = 0x0028;

const SC_NPROCESSORS_CONF: c_int = 0x0060;

const SC_NPROCESSORS_ONLN: c_int = 0x0061;

const SC_PHYS_PAGES: c_int = 0x0062;

const SC_AVPHYS_PAGES: c_int = 0x0063;

static TRACE_STATE: AtomicU8 = AtomicU8::new(0);

fn trace_enabled() -> bool {
    match TRACE_STATE.load(Ordering::Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = std::env::var_os("ECLIPSE_TRACE_SYSQ").is_some_and(|v| v == "1");
            TRACE_STATE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

fn trace(args: std::fmt::Arguments<'_>) {
    if trace_enabled() {
        eprintln!("[sysq] {args}");
    }
}

fn online_cpu_count() -> c_long {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    let rc = unsafe {
        libc::sched_getaffinity(
            0,
            std::mem::size_of::<libc::cpu_set_t>(),
            std::ptr::addr_of_mut!(set),
        )
    };
    if rc == 0 {
        let n = unsafe { libc::CPU_COUNT(&set) };
        if n > 0 {
            return c_long::from(n);
        }
    }

    let onln = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if onln > 0 {
        return onln;
    }
    1
}

fn conf_cpu_count() -> c_long {
    let conf = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    conf.max(online_cpu_count())
}

extern "C" fn eclipse_sysconf(name: c_int) -> c_long {
    let r: c_long = match name {
        SC_PAGESIZE | SC_PAGE_SIZE => super::map::host_page_size() as c_long,
        SC_NPROCESSORS_ONLN => online_cpu_count(),
        SC_NPROCESSORS_CONF => conf_cpu_count(),
        SC_CLK_TCK => {
            let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if t > 0 {
                t
            } else {
                100
            }
        }
        SC_PHYS_PAGES => unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) },
        SC_AVPHYS_PAGES => unsafe { libc::sysconf(libc::_SC_AVPHYS_PAGES) },
        SC_OPEN_MAX => unsafe { libc::sysconf(libc::_SC_OPEN_MAX) },
        SC_ARG_MAX => unsafe { libc::sysconf(libc::_SC_ARG_MAX) },
        SC_NGROUPS_MAX => unsafe { libc::sysconf(libc::_SC_NGROUPS_MAX) },

        _ => -1,
    };
    trace(format_args!("sysconf(name={name}) -> {r}"));
    r
}

extern "C" {

    fn getauxval(at_type: c_ulong) -> c_ulong;
}

extern "C" fn eclipse_getauxval(at_type: c_ulong) -> c_ulong {
    let r = unsafe { getauxval(at_type) };
    trace(format_args!("getauxval(type={at_type}) -> {r}"));
    r
}

extern "C" fn eclipse_sched_getcpu() -> c_int {
    let raw = unsafe { libc::sched_getcpu() };
    let r = if raw < 0 { 0 } else { raw };
    trace(format_args!("sched_getcpu() -> {r} (raw={raw})"));
    r
}

extern "C" fn eclipse_getpagesize() -> c_int {
    let r = super::map::host_page_size() as c_int;
    trace(format_args!("getpagesize() -> {r}"));
    r
}

unsafe extern "C" fn eclipse_sysinfo(info: *mut c_void) -> c_int {
    let r = unsafe { libc::sysinfo(info.cast::<libc::sysinfo>()) };
    trace(format_args!("sysinfo(info={info:p}) -> {r}"));
    r
}

pub const SYSQ_NATIVE_COUNT: usize = 5;

pub fn register_natives(mut register: impl FnMut(&'static str, u64)) {
    register("sysconf", eclipse_sysconf as *const () as u64);
    register("getauxval", eclipse_getauxval as *const () as u64);
    register("sched_getcpu", eclipse_sched_getcpu as *const () as u64);
    register("getpagesize", eclipse_getpagesize as *const () as u64);
    register("sysinfo", eclipse_sysinfo as *const () as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysconf_pagesize_is_the_real_page_size() {
        let page = super::super::map::host_page_size() as c_long;
        assert_eq!(eclipse_sysconf(SC_PAGESIZE), page);
        assert_eq!(eclipse_sysconf(SC_PAGE_SIZE), page);
        assert!(page >= 4096, "page size must be ≥ 4096 on a real host");
    }

    #[test]
    fn sysconf_cpu_counts_are_positive_and_ordered() {
        let onln = eclipse_sysconf(SC_NPROCESSORS_ONLN);
        let conf = eclipse_sysconf(SC_NPROCESSORS_CONF);
        assert!(
            onln > 0,
            "online CPU count must be > 0 (was -1 under the bug)"
        );
        assert!(conf > 0, "configured CPU count must be > 0");
        assert!(conf >= onln, "configured CPUs ≥ online CPUs");
    }

    #[test]
    fn sysconf_clk_tck_is_positive() {
        let tck = eclipse_sysconf(SC_CLK_TCK);
        assert!(
            tck > 0,
            "clock tick rate must be > 0 (was -1 under the bug)"
        );
    }

    #[test]
    fn sysconf_phys_pages_is_positive() {
        let phys = eclipse_sysconf(SC_PHYS_PAGES);
        assert!(phys > 0, "physical RAM page count must be > 0");
    }

    #[test]
    fn sysconf_unmapped_constant_returns_indeterminate_not_a_wrong_value() {
        assert_eq!(eclipse_sysconf(0x7fff), -1);
    }

    #[test]
    fn getauxval_at_pagesz_is_the_page_size() {
        const AT_PAGESZ: c_ulong = 6;
        let v = eclipse_getauxval(AT_PAGESZ);
        assert!(v > 0, "AT_PAGESZ must be > 0");
        assert_eq!(v as c_long, super::super::map::host_page_size() as c_long);
    }

    #[test]
    fn getpagesize_matches_host() {
        assert_eq!(
            eclipse_getpagesize() as u64,
            super::super::map::host_page_size()
        );
    }

    #[test]
    fn sched_getcpu_is_nonnegative() {
        assert!(eclipse_sched_getcpu() >= 0);
    }

    #[test]
    fn online_cpu_count_helper_is_positive() {
        assert!(online_cpu_count() > 0);
        assert!(conf_cpu_count() >= online_cpu_count());
    }

    #[test]
    fn registers_the_expected_system_query_natives() {
        let mut names: Vec<&'static str> = Vec::new();
        register_natives(|name, addr| {
            assert!(addr != 0, "{name} must have a non-null address");
            names.push(name);
        });
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "getauxval",
                "getpagesize",
                "sched_getcpu",
                "sysconf",
                "sysinfo"
            ]
        );
        assert_eq!(names.len(), SYSQ_NATIVE_COUNT);
    }
}
