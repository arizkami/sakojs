// SPDX-License-Identifier: BSD-3-Clause

use std::ffi::c_void;

use sako_v8::Runtime;

unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
}

#[test]
fn shutdown_releases_platform_handles() {
    let handles_before = process_handle_count();
    {
        let mut runtime = Runtime::new().expect("V8 should initialize");
        runtime
            .execute("Promise.resolve(1 + 1);", "lifecycle-test.js")
            .expect("JavaScript should execute");
    }
    let handles_after = process_handle_count();

    assert!(
        handles_after <= handles_before + 2,
        "V8 shutdown left process handles open: before={handles_before}, after={handles_after}"
    );
}

fn process_handle_count() -> u32 {
    let mut count = 0;
    // SAFETY: GetCurrentProcess returns a process-lifetime pseudo-handle and
    // count points to writable storage for the duration of the call.
    let success = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    assert_ne!(success, 0, "GetProcessHandleCount failed");
    count
}
