// SPDX-License-Identifier: BSD-3-Clause

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("sako-diagnostics currently supports only Windows and Linux");

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessStats {
    pub resident_set_bytes: u64,
    pub private_bytes: u64,
    pub os_handles: u64,
}

#[cfg(windows)]
mod windows_stats {
    use super::ProcessStats;
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;

    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCountersEx {
        size: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> i32;
    }

    pub fn process_stats() -> io::Result<ProcessStats> {
        // SAFETY: GetCurrentProcess returns a process-wide pseudo-handle that must
        // not be closed and remains valid for both synchronous queries below.
        let process = unsafe { GetCurrentProcess() };
        let mut counters = ProcessMemoryCountersEx {
            size: size_of::<ProcessMemoryCountersEx>() as u32,
            ..ProcessMemoryCountersEx::default()
        };
        // SAFETY: counters points to writable storage with the exact declared size.
        if unsafe {
            GetProcessMemoryInfo(
                process,
                &raw mut counters,
                size_of::<ProcessMemoryCountersEx>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut handles = 0_u32;
        // SAFETY: handles points to initialized writable storage for this call.
        if unsafe { GetProcessHandleCount(process, &raw mut handles) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ProcessStats {
            resident_set_bytes: counters.working_set_size as u64,
            private_bytes: counters.private_usage as u64,
            os_handles: handles.into(),
        })
    }
}

#[cfg(target_os = "linux")]
mod linux_stats {
    use super::ProcessStats;
    use std::fs;
    use std::io;

    /// Reads one `/proc/self/status` field's value in kibibytes (the kernel
    /// always reports memory fields there as `<name>:  <kB value> kB`).
    fn status_field_kib(status: &str, name: &str) -> Option<u64> {
        status.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.trim_start();
            let rest = rest.strip_suffix("kB")?.trim();
            rest.parse::<u64>().ok()
        })
    }

    pub fn process_stats() -> io::Result<ProcessStats> {
        let status = fs::read_to_string("/proc/self/status")?;
        let resident_set_bytes = status_field_kib(&status, "VmRSS:")
            .ok_or_else(|| io::Error::other("cannot read VmRSS from /proc/self/status"))?
            * 1024;
        // Linux has no exact analog of Windows' "private bytes" counter; RSS is
        // the closest single-number approximation available without walking
        // /proc/self/smaps, so it is reused here for both fields.
        let private_bytes = resident_set_bytes;
        // GetProcessHandleCount's closest Linux analog is the number of open
        // file descriptors, which is also what the leak-detection tests care
        // about (a descriptor that should have closed still showing up here).
        let os_handles = fs::read_dir("/proc/self/fd")?.count() as u64;
        Ok(ProcessStats {
            resident_set_bytes,
            private_bytes,
            os_handles,
        })
    }
}

pub fn process_stats() -> io::Result<ProcessStats> {
    #[cfg(windows)]
    {
        windows_stats::process_stats()
    }
    #[cfg(all(target_os = "linux", not(windows)))]
    {
        linux_stats::process_stats()
    }
}

#[derive(Debug, Default)]
pub struct ComponentCounters {
    native_live_bytes: AtomicU64,
    external_bytes: AtomicU64,
    buffer_pool_bytes: AtomicU64,
    sockets: AtomicU64,
    queued_operations: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ComponentSnapshot {
    pub native_live_bytes: u64,
    pub external_bytes: u64,
    pub buffer_pool_bytes: u64,
    pub sockets: u64,
    pub queued_operations: u64,
}

impl ComponentCounters {
    pub fn add_native_bytes(&self, bytes: u64) {
        self.native_live_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn remove_native_bytes(&self, bytes: u64) {
        subtract_saturating(&self.native_live_bytes, bytes);
    }

    pub fn add_external_bytes(&self, bytes: u64) {
        self.external_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn remove_external_bytes(&self, bytes: u64) {
        subtract_saturating(&self.external_bytes, bytes);
    }

    pub fn set_buffer_pool_bytes(&self, bytes: u64) {
        self.buffer_pool_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn set_sockets(&self, sockets: u64) {
        self.sockets.store(sockets, Ordering::Relaxed);
    }

    pub fn set_queued_operations(&self, queued: u64) {
        self.queued_operations.store(queued, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ComponentSnapshot {
        ComponentSnapshot {
            native_live_bytes: self.native_live_bytes.load(Ordering::Relaxed),
            external_bytes: self.external_bytes.load(Ordering::Relaxed),
            buffer_pool_bytes: self.buffer_pool_bytes.load(Ordering::Relaxed),
            sockets: self.sockets.load(Ordering::Relaxed),
            queued_operations: self.queued_operations.load(Ordering::Relaxed),
        }
    }
}

fn subtract_saturating(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(value))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_process_memory_and_handle_statistics() {
        let stats = process_stats().unwrap();
        assert!(stats.resident_set_bytes > 0);
        assert!(stats.private_bytes > 0);
        assert!(stats.os_handles > 0);
    }

    #[test]
    fn component_counters_snapshot_and_saturate() {
        let counters = ComponentCounters::default();
        counters.add_native_bytes(10);
        counters.remove_native_bytes(3);
        counters.remove_native_bytes(100);
        counters.add_external_bytes(20);
        counters.set_buffer_pool_bytes(30);
        counters.set_sockets(2);
        counters.set_queued_operations(4);
        assert_eq!(
            counters.snapshot(),
            ComponentSnapshot {
                native_live_bytes: 0,
                external_bytes: 20,
                buffer_pool_bytes: 30,
                sockets: 2,
                queued_operations: 4,
            }
        );
    }
}
