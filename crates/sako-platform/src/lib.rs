// SPDX-License-Identifier: BSD-3-Clause

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("sako-platform currently supports only Windows");

use std::ffi::c_void;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::time::Duration;

const WAIT_TIMEOUT: i32 = 258;

#[repr(C)]
struct OverlappedEntry {
    completion_key: usize,
    overlapped: *mut c_void,
    internal: usize,
    bytes_transferred: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateIoCompletionPort(
        file_handle: *mut c_void,
        existing_port: *mut c_void,
        completion_key: usize,
        concurrent_threads: u32,
    ) -> *mut c_void;
    fn GetQueuedCompletionStatusEx(
        completion_port: *mut c_void,
        entries: *mut OverlappedEntry,
        count: u32,
        removed: *mut u32,
        milliseconds: u32,
        alertable: i32,
    ) -> i32;
    fn PostQueuedCompletionStatus(
        completion_port: *mut c_void,
        bytes_transferred: u32,
        completion_key: usize,
        overlapped: *mut c_void,
    ) -> i32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion {
    pub key: usize,
    pub bytes_transferred: u32,
    pub overlapped: *mut c_void,
}

#[derive(Debug)]
pub enum PostError {
    Full,
    System(io::Error),
}

pub trait PlatformRuntime {
    fn poll(&mut self, timeout: Duration, maximum: usize) -> io::Result<Vec<Completion>>;
    fn wake(&mut self) -> Result<(), PostError>;
}

pub struct IocpReactor {
    port: OwnedHandle,
    capacity: usize,
    pending_posts: usize,
}

impl IocpReactor {
    pub fn new(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || capacity > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IOCP capacity must be between 1 and u32::MAX",
            ));
        }
        // SAFETY: INVALID_HANDLE_VALUE requests a new completion port. A non-null
        // result is an owned kernel HANDLE transferred into OwnedHandle exactly once.
        let raw =
            unsafe { CreateIoCompletionPort((-1_isize) as *mut c_void, ptr::null_mut(), 0, 0) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateIoCompletionPort returned a new owned HANDLE above.
        let port = unsafe { OwnedHandle::from_raw_handle(raw) };
        Ok(Self {
            port,
            capacity,
            pending_posts: 0,
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn pending_posts(&self) -> usize {
        self.pending_posts
    }

    pub fn associate(&self, handle: BorrowedHandle<'_>, key: usize) -> io::Result<()> {
        // SAFETY: both handles are live for this call. IOCP association does not
        // transfer ownership; the caller remains responsible for the I/O handle.
        let result = unsafe {
            CreateIoCompletionPort(handle.as_raw_handle(), self.port.as_raw_handle(), key, 0)
        };
        if result.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn post(
        &mut self,
        key: usize,
        bytes_transferred: u32,
        overlapped: *mut c_void,
    ) -> Result<(), PostError> {
        if self.pending_posts >= self.capacity {
            return Err(PostError::Full);
        }
        // SAFETY: the IOCP handle is live. The opaque OVERLAPPED pointer is never
        // dereferenced by this crate and is returned unchanged to the caller.
        let posted = unsafe {
            PostQueuedCompletionStatus(
                self.port.as_raw_handle(),
                bytes_transferred,
                key,
                overlapped,
            )
        };
        if posted == 0 {
            return Err(PostError::System(io::Error::last_os_error()));
        }
        self.pending_posts += 1;
        Ok(())
    }
}

impl PlatformRuntime for IocpReactor {
    fn poll(&mut self, timeout: Duration, maximum: usize) -> io::Result<Vec<Completion>> {
        let count = maximum.min(self.capacity).min(u32::MAX as usize);
        if count == 0 {
            return Ok(Vec::new());
        }
        let milliseconds = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut entries = Vec::<MaybeUninit<OverlappedEntry>>::with_capacity(count);
        let mut removed = 0_u32;
        // SAFETY: entries has storage for count values and Windows initializes the
        // first `removed` entries on success. The port remains live for this call.
        let result = unsafe {
            GetQueuedCompletionStatusEx(
                self.port.as_raw_handle(),
                entries.as_mut_ptr().cast(),
                count as u32,
                &mut removed,
                milliseconds,
                0,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(WAIT_TIMEOUT) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        // SAFETY: GetQueuedCompletionStatusEx initialized exactly `removed`
        // entries, which cannot exceed the supplied count.
        unsafe { entries.set_len(removed as usize) };
        self.pending_posts = self.pending_posts.saturating_sub(removed as usize);
        Ok(entries
            .into_iter()
            .map(|entry| {
                // SAFETY: every vector element is among the initialized entries.
                let entry = unsafe { entry.assume_init() };
                Completion {
                    key: entry.completion_key,
                    bytes_transferred: entry.bytes_transferred,
                    overlapped: entry.overlapped,
                }
            })
            .collect())
    }

    fn wake(&mut self) -> Result<(), PostError> {
        self.post(0, 0, ptr::null_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_posts_are_bounded_and_drained() {
        let mut reactor = IocpReactor::new(2).unwrap();
        reactor.post(10, 20, ptr::null_mut()).unwrap();
        reactor.post(11, 21, ptr::null_mut()).unwrap();
        assert!(matches!(
            reactor.post(12, 22, ptr::null_mut()),
            Err(PostError::Full)
        ));
        let completions = reactor.poll(Duration::ZERO, 2).unwrap();
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].key, 10);
        assert_eq!(completions[1].bytes_transferred, 21);
        assert_eq!(reactor.pending_posts(), 0);
        reactor.wake().unwrap();
        assert_eq!(reactor.poll(Duration::ZERO, 1).unwrap().len(), 1);
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(IocpReactor::new(0).is_err());
    }
}
