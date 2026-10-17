use std::ffi::c_int;

extern "C" {
    fn eclipse_bionic_tls_slots_at_thread_pointer() -> c_int;

    fn eclipse_bionic_stack_guard_seeded() -> c_int;
}

pub(super) fn at_thread_pointer() -> bool {
    unsafe { eclipse_bionic_tls_slots_at_thread_pointer() != 0 }
}

pub(super) fn stack_guard_seeded() -> bool {
    unsafe { eclipse_bionic_stack_guard_seeded() != 0 }
}
