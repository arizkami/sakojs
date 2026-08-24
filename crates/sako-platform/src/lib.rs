// SPDX-License-Identifier: BSD-3-Clause

#![cfg_attr(not(any(windows, unix)), allow(dead_code))]

#[cfg(not(any(windows, unix)))]
compile_error!("sako-platform currently supports only Windows and Unix-like platforms");

use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::ptr::NonNull;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Completion {
    pub key: usize,
    pub bytes_transferred: u32,
    pub overlapped: Option<NonNull<c_void>>,
    pub status: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    FileRead,
    FileWrite,
    SocketReceive,
    SocketSend,
}

#[derive(Debug)]
pub struct OperationCompletion {
    pub id: u64,
    pub key: usize,
    pub kind: OperationKind,
    pub bytes_transferred: u32,
    pub status: usize,
    pub buffer: Vec<u8>,
}

/// Neutral status codes both platform reactors report through
/// `OperationCompletion::status`, so `succeeded`/`cancelled` never branch on a
/// platform-specific value. Windows translates its real NTSTATUS (`0` already
/// means success there too; `STATUS_CANCELLED = 0xC000_0120`) into these at
/// the point a completion is built; the epoll reactor produces cancellation
/// itself (there is no kernel-owned pending operation to report it) and uses
/// them directly.
pub const STATUS_SUCCESS: usize = 0;
pub const STATUS_CANCELLED: usize = 1;
pub const STATUS_FAILURE: usize = 2;

impl OperationCompletion {
    pub fn succeeded(&self) -> bool {
        self.status == STATUS_SUCCESS
    }

    pub fn cancelled(&self) -> bool {
        self.status == STATUS_CANCELLED
    }
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

/// Hashes the integer keys the reactor uses — operation identifiers, and
/// OVERLAPPED addresses on Windows or raw descriptors on Unix — by
/// multiplication instead of SipHash. Both are already unique and
/// unpredictable to anything outside the process, so the cryptographic
/// mixing of the default hasher is pure overhead on a path that runs twice
/// per socket operation.
#[derive(Default)]
pub struct IntegerHasher(u64);

impl Hasher for IntegerHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0.rotate_left(8) ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

pub(crate) type IntegerMap<K, V> = HashMap<K, V, BuildHasherDefault<IntegerHasher>>;

/// Completion entries fetched from the kernel in one `poll` call. Both
/// reactors keep this array allocated for their lifetime instead of sizing it
/// to the operation capacity on every poll.
pub(crate) const MAXIMUM_POLL_ENTRIES: usize = 256;

/// Receive and send buffers retained for reuse between operations.
pub(crate) const MAXIMUM_POOLED_BUFFERS: usize = 512;

#[cfg(windows)]
mod iocp;
#[cfg(windows)]
pub use iocp::{IocpCounters, IocpReactor as Reactor};
#[cfg(windows)]
pub type RawDescriptor = std::os::windows::io::RawSocket;

#[cfg(all(unix, not(windows)))]
mod epoll;
#[cfg(all(unix, not(windows)))]
pub use epoll::{EpollCounters, EpollReactor as Reactor};
#[cfg(all(unix, not(windows)))]
pub type RawDescriptor = std::os::fd::RawFd;
