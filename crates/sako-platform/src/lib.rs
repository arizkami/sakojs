// SPDX-License-Identifier: BSD-3-Clause

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("sako-platform currently supports only Windows");

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::io;
use std::mem::MaybeUninit;
use std::os::windows::io::{
    AsHandle, AsRawHandle, AsRawSocket, AsSocket, BorrowedHandle, BorrowedSocket, FromRawHandle,
    OwnedHandle, OwnedSocket,
};
use std::ptr::{self, NonNull};
use std::time::Duration;

const WAIT_TIMEOUT: i32 = 258;
const ERROR_IO_PENDING: i32 = 997;
const SOCKET_ERROR: i32 = -1;
const STATUS_CANCELLED: usize = 0xC000_0120;

#[repr(C)]
#[derive(Default)]
struct NativeOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

#[repr(C)]
struct WsaBuffer {
    length: u32,
    data: *mut u8,
}

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
    fn CancelIoEx(handle: *mut c_void, overlapped: *mut NativeOverlapped) -> i32;
    fn ReadFile(
        handle: *mut c_void,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
        overlapped: *mut NativeOverlapped,
    ) -> i32;
    fn WriteFile(
        handle: *mut c_void,
        buffer: *const c_void,
        bytes_to_write: u32,
        bytes_written: *mut u32,
        overlapped: *mut NativeOverlapped,
    ) -> i32;
}

#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSARecv(
        socket: usize,
        buffers: *mut WsaBuffer,
        buffer_count: u32,
        bytes_received: *mut u32,
        flags: *mut u32,
        overlapped: *mut NativeOverlapped,
        completion_routine: *mut c_void,
    ) -> i32;
    fn WSASend(
        socket: usize,
        buffers: *mut WsaBuffer,
        buffer_count: u32,
        bytes_sent: *mut u32,
        flags: u32,
        overlapped: *mut NativeOverlapped,
        completion_routine: *mut c_void,
    ) -> i32;
    fn WSAGetLastError() -> i32;
}

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

impl OperationCompletion {
    pub fn succeeded(&self) -> bool {
        self.status == 0
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

pub struct IocpReactor {
    port: OwnedHandle,
    capacity: usize,
    pending_posts: usize,
    next_operation_id: u64,
    operations: HashMap<usize, PendingOperation>,
    completed_operations: VecDeque<OperationCompletion>,
}

enum OperationOwner {
    Handle(OwnedHandle),
    Socket(OwnedSocket),
}

impl OperationOwner {
    fn raw_handle(&self) -> *mut c_void {
        match self {
            Self::Handle(handle) => handle.as_raw_handle(),
            Self::Socket(socket) => socket.as_raw_socket() as *mut c_void,
        }
    }
}

struct PendingOperation {
    id: u64,
    key: usize,
    kind: OperationKind,
    owner: OperationOwner,
    overlapped: Box<NativeOverlapped>,
    buffer: Vec<u8>,
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
            next_operation_id: 1,
            operations: HashMap::with_capacity(capacity),
            completed_operations: VecDeque::with_capacity(capacity),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn pending_posts(&self) -> usize {
        self.pending_posts
    }

    pub fn pending_operations(&self) -> usize {
        self.operations.len()
    }

    pub fn retained_completions(&self) -> usize {
        self.completed_operations.len()
    }

    fn outstanding(&self) -> usize {
        self.pending_posts
            .saturating_add(self.operations.len())
            .saturating_add(self.completed_operations.len())
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

    pub fn associate_socket(&self, socket: BorrowedSocket<'_>, key: usize) -> io::Result<()> {
        // SAFETY: Winsock SOCKET values are valid handles for IOCP association.
        // The borrowed socket and completion port are live for this call, and
        // association transfers ownership of neither resource.
        let result = unsafe {
            CreateIoCompletionPort(
                socket.as_raw_socket() as *mut c_void,
                self.port.as_raw_handle(),
                key,
                0,
            )
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
        overlapped: Option<NonNull<c_void>>,
    ) -> Result<(), PostError> {
        if self.outstanding() >= self.capacity {
            return Err(PostError::Full);
        }
        // SAFETY: the IOCP handle is live. The opaque OVERLAPPED pointer is never
        // dereferenced by this crate and is returned unchanged to the caller.
        let posted = unsafe {
            PostQueuedCompletionStatus(
                self.port.as_raw_handle(),
                bytes_transferred,
                key,
                overlapped.map_or(ptr::null_mut(), NonNull::as_ptr),
            )
        };
        if posted == 0 {
            return Err(PostError::System(io::Error::last_os_error()));
        }
        self.pending_posts += 1;
        Ok(())
    }

    fn reserve_operation(&self) -> Result<(), PostError> {
        if self.outstanding() >= self.capacity {
            Err(PostError::Full)
        } else {
            Ok(())
        }
    }

    fn allocate_operation_id(&mut self) -> u64 {
        let id = self.next_operation_id;
        self.next_operation_id = self.next_operation_id.wrapping_add(1).max(1);
        id
    }

    fn register_operation(&mut self, operation: PendingOperation) -> u64 {
        let id = operation.id;
        let pointer = operation.overlapped.as_ref() as *const NativeOverlapped as usize;
        self.operations.insert(pointer, operation);
        id
    }

    pub fn submit_file_read(
        &mut self,
        handle: OwnedHandle,
        key: usize,
        length: u32,
        offset: u64,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        self.associate(handle.as_handle(), key)
            .map_err(PostError::System)?;
        let mut operation = PendingOperation {
            id: self.allocate_operation_id(),
            key,
            kind: OperationKind::FileRead,
            owner: OperationOwner::Handle(handle),
            overlapped: Box::new(NativeOverlapped {
                offset: offset as u32,
                offset_high: (offset >> 32) as u32,
                ..NativeOverlapped::default()
            }),
            buffer: vec![0; length as usize],
        };
        let mut immediate = 0;
        // SAFETY: the operation owns the handle, stable OVERLAPPED allocation,
        // and fixed buffer until its completion is removed from this reactor.
        let result = unsafe {
            ReadFile(
                operation.owner.raw_handle(),
                operation.buffer.as_mut_ptr().cast(),
                length,
                &mut immediate,
                operation.overlapped.as_mut(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(PostError::System(error));
            }
        }
        Ok(self.register_operation(operation))
    }

    pub fn submit_file_write(
        &mut self,
        handle: OwnedHandle,
        key: usize,
        buffer: Vec<u8>,
        offset: u64,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        let length = u32::try_from(buffer.len()).map_err(|_| {
            PostError::System(io::Error::new(
                io::ErrorKind::InvalidInput,
                "overlapped file write exceeds u32::MAX",
            ))
        })?;
        self.associate(handle.as_handle(), key)
            .map_err(PostError::System)?;
        let mut operation = PendingOperation {
            id: self.allocate_operation_id(),
            key,
            kind: OperationKind::FileWrite,
            owner: OperationOwner::Handle(handle),
            overlapped: Box::new(NativeOverlapped {
                offset: offset as u32,
                offset_high: (offset >> 32) as u32,
                ..NativeOverlapped::default()
            }),
            buffer,
        };
        let mut immediate = 0;
        // SAFETY: the operation owns the handle, stable OVERLAPPED allocation,
        // and immutable fixed buffer until IOCP reports completion.
        let result = unsafe {
            WriteFile(
                operation.owner.raw_handle(),
                operation.buffer.as_ptr().cast(),
                length,
                &mut immediate,
                operation.overlapped.as_mut(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING) {
                return Err(PostError::System(error));
            }
        }
        Ok(self.register_operation(operation))
    }

    pub fn submit_socket_receive(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        length: u32,
    ) -> Result<u64, PostError> {
        self.submit_socket_receive_inner(socket, key, length, true)
    }

    /// Submits a receive on a socket file object already associated with this IOCP.
    ///
    /// # Safety
    ///
    /// `socket` must be the associated socket or a duplicate of that socket. If
    /// it belongs to another completion port, completion cannot be drained here.
    pub unsafe fn submit_associated_socket_receive(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        length: u32,
    ) -> Result<u64, PostError> {
        self.submit_socket_receive_inner(socket, key, length, false)
    }

    fn submit_socket_receive_inner(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        length: u32,
        associate: bool,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        if associate {
            self.associate_socket(socket.as_socket(), key)
                .map_err(PostError::System)?;
        }
        let mut operation = PendingOperation {
            id: self.allocate_operation_id(),
            key,
            kind: OperationKind::SocketReceive,
            owner: OperationOwner::Socket(socket),
            overlapped: Box::new(NativeOverlapped::default()),
            buffer: vec![0; length as usize],
        };
        let mut wsa_buffer = WsaBuffer {
            length,
            data: operation.buffer.as_mut_ptr(),
        };
        let mut immediate = 0;
        let mut flags = 0;
        // SAFETY: the operation owns the socket, stable OVERLAPPED allocation,
        // and fixed receive buffer until IOCP reports completion.
        let result = unsafe {
            WSARecv(
                operation.owner.raw_handle() as usize,
                &mut wsa_buffer,
                1,
                &mut immediate,
                &mut flags,
                operation.overlapped.as_mut(),
                ptr::null_mut(),
            )
        };
        if result == SOCKET_ERROR {
            // SAFETY: WSAGetLastError has no preconditions.
            let error = unsafe { WSAGetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(PostError::System(io::Error::from_raw_os_error(error)));
            }
        }
        Ok(self.register_operation(operation))
    }

    pub fn submit_socket_send(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        buffer: Vec<u8>,
    ) -> Result<u64, PostError> {
        self.submit_socket_send_inner(socket, key, buffer, true)
    }

    /// Submits a send on a socket file object already associated with this IOCP.
    ///
    /// # Safety
    ///
    /// `socket` must be the associated socket or a duplicate of that socket. If
    /// it belongs to another completion port, completion cannot be drained here.
    pub unsafe fn submit_associated_socket_send(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        buffer: Vec<u8>,
    ) -> Result<u64, PostError> {
        self.submit_socket_send_inner(socket, key, buffer, false)
    }

    fn submit_socket_send_inner(
        &mut self,
        socket: OwnedSocket,
        key: usize,
        buffer: Vec<u8>,
        associate: bool,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        let length = u32::try_from(buffer.len()).map_err(|_| {
            PostError::System(io::Error::new(
                io::ErrorKind::InvalidInput,
                "overlapped socket send exceeds u32::MAX",
            ))
        })?;
        if associate {
            self.associate_socket(socket.as_socket(), key)
                .map_err(PostError::System)?;
        }
        let mut operation = PendingOperation {
            id: self.allocate_operation_id(),
            key,
            kind: OperationKind::SocketSend,
            owner: OperationOwner::Socket(socket),
            overlapped: Box::new(NativeOverlapped::default()),
            buffer,
        };
        let mut wsa_buffer = WsaBuffer {
            length,
            data: operation.buffer.as_mut_ptr(),
        };
        let mut immediate = 0;
        // SAFETY: the operation owns the socket, stable OVERLAPPED allocation,
        // and fixed send buffer until IOCP reports completion.
        let result = unsafe {
            WSASend(
                operation.owner.raw_handle() as usize,
                &mut wsa_buffer,
                1,
                &mut immediate,
                0,
                operation.overlapped.as_mut(),
                ptr::null_mut(),
            )
        };
        if result == SOCKET_ERROR {
            // SAFETY: WSAGetLastError has no preconditions.
            let error = unsafe { WSAGetLastError() };
            if error != ERROR_IO_PENDING {
                return Err(PostError::System(io::Error::from_raw_os_error(error)));
            }
        }
        Ok(self.register_operation(operation))
    }

    pub fn cancel_operation(&self, id: u64) -> io::Result<bool> {
        let Some(operation) = self
            .operations
            .values()
            .find(|operation| operation.id == id)
        else {
            return Ok(false);
        };
        // SAFETY: the operation owns the live handle and OVERLAPPED allocation.
        // CancelIoEx does not release them; its completion must still be drained.
        let result = unsafe {
            CancelIoEx(
                operation.owner.raw_handle(),
                operation.overlapped.as_ref() as *const NativeOverlapped as *mut NativeOverlapped,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(1168) {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(true)
    }

    pub fn take_operation_completion(&mut self, id: u64) -> Option<OperationCompletion> {
        let index = self
            .completed_operations
            .iter()
            .position(|completion| completion.id == id)?;
        self.completed_operations.remove(index)
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
        let mut manual_completions = 0_usize;
        let completions = entries
            .into_iter()
            .map(|entry| {
                // SAFETY: every vector element is among the initialized entries.
                let entry = unsafe { entry.assume_init() };
                let pointer = NonNull::new(entry.overlapped);
                if let Some(pointer) = pointer {
                    if let Some(operation) = self.operations.remove(&(pointer.as_ptr() as usize)) {
                        self.completed_operations.push_back(OperationCompletion {
                            id: operation.id,
                            key: operation.key,
                            kind: operation.kind,
                            bytes_transferred: entry.bytes_transferred,
                            status: entry.internal,
                            buffer: operation.buffer,
                        });
                    } else {
                        manual_completions += 1;
                    }
                } else {
                    manual_completions += 1;
                }
                Completion {
                    key: entry.completion_key,
                    bytes_transferred: entry.bytes_transferred,
                    overlapped: pointer,
                    status: entry.internal,
                }
            })
            .collect();
        self.pending_posts = self.pending_posts.saturating_sub(manual_completions);
        Ok(completions)
    }

    fn wake(&mut self) -> Result<(), PostError> {
        self.post(0, 0, None)
    }
}

impl Drop for IocpReactor {
    fn drop(&mut self) {
        let ids = self
            .operations
            .values()
            .map(|operation| operation.id)
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.cancel_operation(id);
        }
        while !self.operations.is_empty() {
            let _ = self.poll(Duration::from_millis(100), self.capacity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::Instant;

    #[test]
    fn completion_posts_are_bounded_and_drained() {
        let mut reactor = IocpReactor::new(2).unwrap();
        reactor.post(10, 20, None).unwrap();
        reactor.post(11, 21, None).unwrap();
        assert!(matches!(reactor.post(12, 22, None), Err(PostError::Full)));
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

    #[test]
    fn overlapped_file_reads_are_owned_until_consumed() {
        const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
        let path = std::env::temp_dir().join(format!(
            "sako-iocp-file-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"overlapped-file").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(&path)
            .unwrap();
        let mut reactor = IocpReactor::new(2).unwrap();
        let operation = reactor.submit_file_read(file.into(), 41, 10, 0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let completed = loop {
            reactor.poll(Duration::from_millis(10), 2).unwrap();
            if let Some(completed) = reactor.take_operation_completion(operation) {
                break completed;
            }
            assert!(Instant::now() < deadline, "overlapped file read timed out");
        };
        assert!(completed.succeeded());
        assert_eq!(completed.kind, OperationKind::FileRead);
        assert_eq!(completed.key, 41);
        assert_eq!(completed.bytes_transferred, 10);
        assert_eq!(&completed.buffer[..10], b"overlapped");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pending_socket_receive_is_cancelled_and_backpressured() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let mut reactor = IocpReactor::new(1).unwrap();
        let operation = reactor
            .submit_socket_receive(client.try_clone().unwrap().into(), 77, 4096)
            .unwrap();
        assert!(matches!(
            reactor.submit_socket_receive(client.try_clone().unwrap().into(), 78, 4096),
            Err(PostError::Full)
        ));
        assert!(reactor.cancel_operation(operation).unwrap());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            reactor.poll(Duration::from_millis(10), 1).unwrap();
            if reactor.retained_completions() != 0 {
                break;
            }
            assert!(Instant::now() < deadline, "cancel completion timed out");
        }
        assert!(matches!(reactor.post(1, 0, None), Err(PostError::Full)));
        let completed = reactor.take_operation_completion(operation).unwrap();
        assert!(completed.cancelled());
        assert_eq!(completed.kind, OperationKind::SocketReceive);
        assert_eq!(reactor.pending_operations(), 0);
        reactor.post(1, 0, None).unwrap();
        reactor.poll(Duration::ZERO, 1).unwrap();
    }
}
