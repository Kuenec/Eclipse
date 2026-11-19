use std::ffi::c_void;
use std::time::Duration;

use rustix::process::Signal;

use super::*;
use crate::audio::private_server::test_command;

const CHILD: &str = "ECLIPSE_TEST_STAND_IN_CHILD";

const CHILD_LIMIT: Duration = Duration::from_secs(30);

fn address(function: &str) -> usize {
    for_function(function).expect("a free stand-in") as usize
}

#[test]
fn the_ndk_surface_is_sorted_for_its_binary_search() {
    assert!(ndk_surface::FUNCTIONS
        .windows(2)
        .all(|pair| pair[0].0 < pair[1].0));
}

#[test]
fn a_function_keeps_one_stand_in() {
    let first = address("AMediaExtractor_new");
    assert_eq!(address("AMediaExtractor_new"), first);
    assert_ne!(address("AMediaExtractor_delete"), first);
    assert!(bound().contains(&("AMediaExtractor_new".to_owned(), Some(Answer::Zero))));
}

#[test]
fn ndk_functions_answer_that_the_device_does_not_support_them() {
    let pointer: extern "C" fn() -> *mut c_void =
        unsafe { std::mem::transmute(address("AMediaMuxer_new")) };
    let status: extern "C" fn(*mut c_void, *const c_void) -> i32 =
        unsafe { std::mem::transmute(address("AMediaCodec_setParameters")) };
    let found: extern "C" fn(*mut c_void, *const u8, *mut usize) -> bool =
        unsafe { std::mem::transmute(address("AMediaFormat_getSize")) };
    let count: extern "C" fn(*mut c_void, *mut c_void) -> i32 =
        unsafe { std::mem::transmute(address("AHardwareBuffer_unlock")) };
    let precision: extern "C" fn(*const c_void) -> f32 =
        unsafe { std::mem::transmute(address("AMotionEvent_getXPrecision")) };

    assert!(pointer().is_null());
    assert_eq!(
        status(std::ptr::null_mut(), std::ptr::null()),
        AMEDIA_ERROR_UNSUPPORTED
    );
    assert!(!found(
        std::ptr::null_mut(),
        std::ptr::null(),
        std::ptr::null_mut()
    ));
    assert_eq!(count(std::ptr::null_mut(), std::ptr::null_mut()), -1);
    assert_eq!(precision(std::ptr::null()), 0.0);
}

#[test]
fn a_function_android_never_had_stops_roblox_only_when_called() {
    let call: extern "C" fn() -> isize =
        unsafe { std::mem::transmute(address("eclipse_test_function_android_never_had")) };
    assert!(bound().contains(&("eclipse_test_function_android_never_had".to_owned(), None)));
    if std::env::var_os(CHILD).is_some() {
        call();
        return;
    }
    let mut command = test_command(
        "loader::stand_in::tests::a_function_android_never_had_stops_roblox_only_when_called",
    );
    command.env(CHILD, "1");
    let output = crate::bounded_child::output(&mut command, CHILD_LIMIT);
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&output.status),
        Some(Signal::ABORT.as_raw()),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
