// SPDX-License-Identifier: BSD-3-Clause

use crate::{
    Completion, IntegerMap, MAXIMUM_POLL_ENTRIES, MAXIMUM_POOLED_BUFFERS, OperationCompletion,
    OperationKind, PlatformRuntime, PostError, RawDescriptor, STATUS_CANCELLED, STATUS_FAILURE,
    STATUS_SUCCESS,
};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::time::Duration;

/// `epoll_data.u64` value reserved for the reactor's own wake descriptor, so
/// it can never collide with a real (non-negative `i32`-range) file
/// descriptor used as a connection key.
const WAKE_SENTINEL: u64 = u64::MAX;

/// Counts how each submitted operation finished. Kept for parity with the
/// IOCP reactor's counters, though every epoll completion is in one sense
/// "inline": the reactor always performs the real syscall itself once the
/// kernel reports readiness, there is no separate completion packet to skip.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EpollCounters {
    pub receives_inline: u64,
    pub receives_pending: u64,
    pub sends_inline: u64,
    pub sends_pending: u64,
}

enum OperationOwner {
    /// The reactor owns this descriptor until its operation completes or is
    /// cancelled, at which point dropping it closes the descriptor. Never
    /// read directly; it exists only for that closing side effect.
    Owned(#[allow(dead_code)] OwnedFd),
    /// A descriptor owned by the caller. Dropping this variant does nothing;
    /// the caller keeps it alive until the completion is drained.
    Borrowed(#[allow(dead_code)] RawFd),
}

struct PendingRead {
    id: u64,
    length: usize,
    /// Never read directly; kept alive only so a submission that owns its
    /// descriptor closes it exactly when this struct is dropped.
    #[allow(dead_code)]
    owner: OperationOwner,
}

struct PendingWrite {
    id: u64,
    buffer: Vec<u8>,
    #[allow(dead_code)]
    owner: OperationOwner,
}

#[derive(Default)]
struct Registration {
    key: usize,
    /// The epoll interest mask currently registered for this descriptor, kept
    /// so `set_interest` only issues `epoll_ctl(MOD)` when the mask actually
    /// changes instead of on every submission.
    interest: u32,
    read: Option<PendingRead>,
    write: Option<PendingWrite>,
}

pub struct EpollReactor {
    epoll_fd: OwnedFd,
    wake_fd: OwnedFd,
    capacity: usize,
    pending_posts: usize,
    next_operation_id: u64,
    registrations: IntegerMap<RawFd, Registration>,
    operation_locations: IntegerMap<u64, (RawFd, bool)>,
    completed_operations: IntegerMap<u64, OperationCompletion>,
    /// Operations resolved synchronously at submit or cancel time (an eager
    /// read/write that did not need to wait for readiness, or a cancellation,
    /// which this reactor always resolves immediately since nothing about it
    /// is kernel-owned). These already sit in `completed_operations`, but
    /// `poll_operations` callers only learn an id finished through its
    /// `completed` output, so this queue is what feeds that on the next call
    /// — the same role IOCP's `inline_completed` plays for its own inline
    /// completions.
    immediately_completed: Vec<u64>,
    manual_completions: VecDeque<Completion>,
    events: Vec<libc::epoll_event>,
    buffer_pool: Vec<Vec<u8>>,
    counters: EpollCounters,
}

fn read_raw(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the caller guarantees `fd` is a valid, open descriptor for the
    // duration of this call, and `buffer` is writable for its full length.
    let result = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result as usize)
}

fn write_raw(fd: RawFd, buffer: &[u8]) -> io::Result<usize> {
    // SAFETY: the caller guarantees `fd` is a valid, open descriptor for the
    // duration of this call, and `buffer` is readable for its full length.
    let result = unsafe { libc::write(fd, buffer.as_ptr().cast(), buffer.len()) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result as usize)
}

impl EpollReactor {
    pub fn new(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || capacity > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reactor capacity must be between 1 and u32::MAX",
            ));
        }
        // SAFETY: no preconditions; a negative result is a normal error.
        let epoll_raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: epoll_create1 returned a new owned descriptor above.
        let epoll_fd = unsafe { OwnedFd::from_raw_fd(epoll_raw) };

        // SAFETY: no preconditions; a negative result is a normal error.
        let wake_raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if wake_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: eventfd returned a new owned descriptor above.
        let wake_fd = unsafe { OwnedFd::from_raw_fd(wake_raw) };

        let mut wake_event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: WAKE_SENTINEL,
        };
        // SAFETY: both descriptors are live and owned by this reactor.
        if unsafe {
            libc::epoll_ctl(
                epoll_fd.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                wake_fd.as_raw_fd(),
                &mut wake_event,
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }

        let entry_capacity = capacity.min(MAXIMUM_POLL_ENTRIES);
        Ok(Self {
            epoll_fd,
            wake_fd,
            capacity,
            pending_posts: 0,
            next_operation_id: 1,
            registrations: IntegerMap::with_capacity_and_hasher(capacity, Default::default()),
            operation_locations: IntegerMap::with_capacity_and_hasher(capacity, Default::default()),
            completed_operations: IntegerMap::with_capacity_and_hasher(
                capacity,
                Default::default(),
            ),
            immediately_completed: Vec::new(),
            manual_completions: VecDeque::new(),
            events: Vec::with_capacity(entry_capacity),
            buffer_pool: Vec::new(),
            counters: EpollCounters::default(),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn pending_posts(&self) -> usize {
        self.pending_posts
    }

    pub fn pending_operations(&self) -> usize {
        self.operation_locations.len()
    }

    pub fn retained_completions(&self) -> usize {
        self.completed_operations.len()
    }

    pub fn counters(&self) -> EpollCounters {
        self.counters
    }

    fn outstanding(&self) -> usize {
        self.pending_posts
            .saturating_add(self.operation_locations.len())
            .saturating_add(self.completed_operations.len())
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

    /// Takes a buffer of exactly `length` bytes from the reuse pool, falling
    /// back to a fresh allocation. Pooled buffers keep their initialized
    /// bytes, so a reused receive buffer costs neither an allocation nor a
    /// zero fill.
    pub fn acquire_buffer(&mut self, length: usize) -> Vec<u8> {
        let mut buffer = match self.buffer_pool.pop() {
            Some(buffer) if buffer.capacity() >= length => buffer,
            Some(buffer) => {
                self.recycle_buffer(buffer);
                Vec::with_capacity(length)
            }
            None => Vec::with_capacity(length),
        };
        if buffer.len() < length {
            buffer.resize(length, 0);
        } else {
            buffer.truncate(length);
        }
        buffer
    }

    /// Returns a completed operation's buffer to the reuse pool.
    pub fn recycle_buffer(&mut self, buffer: Vec<u8>) {
        if buffer.capacity() != 0 && self.buffer_pool.len() < MAXIMUM_POOLED_BUFFERS {
            self.buffer_pool.push(buffer);
        }
    }

    fn epoll_add(&mut self, fd: RawFd, key: usize) -> io::Result<()> {
        self.registrations.insert(
            fd,
            Registration {
                key,
                interest: 0,
                read: None,
                write: None,
            },
        );
        let mut event = libc::epoll_event {
            events: 0,
            u64: fd as u64,
        };
        // SAFETY: the epoll instance is live and `fd` is a valid, open
        // descriptor for the duration of this call.
        if unsafe { libc::epoll_ctl(self.epoll_fd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut event) }
            == -1
        {
            let error = io::Error::last_os_error();
            self.registrations.remove(&fd);
            return Err(error);
        }
        Ok(())
    }

    fn set_interest(&mut self, fd: RawFd, bit: u32, add: bool) -> io::Result<()> {
        let Some(registration) = self.registrations.get_mut(&fd) else {
            return Ok(());
        };
        let previous = registration.interest;
        let next = if add { previous | bit } else { previous & !bit };
        if next == previous {
            return Ok(());
        }
        registration.interest = next;
        let mut event = libc::epoll_event {
            events: next,
            u64: fd as u64,
        };
        // SAFETY: the epoll instance is live and `fd` was already added via
        // `epoll_add` (associate_socket / an owning submit call).
        if unsafe { libc::epoll_ctl(self.epoll_fd.as_raw_fd(), libc::EPOLL_CTL_MOD, fd, &mut event) }
            == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Associates a descriptor with this reactor.
    ///
    /// Every completion the epoll reactor reports is already the real result
    /// of a syscall performed after the kernel reported readiness, so unlike
    /// IOCP's inline-skip negotiation there is nothing a provider can refuse:
    /// this always succeeds in reporting `true`.
    pub fn associate_socket(&mut self, socket: RawDescriptor, key: usize) -> io::Result<bool> {
        self.epoll_add(socket, key)?;
        Ok(true)
    }

    pub fn submit_file_read(
        &mut self,
        handle: OwnedFd,
        key: usize,
        length: u32,
        offset: u64,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::from(handle);
        let mut buffer = vec![0_u8; length as usize];
        let (status, bytes_transferred) = match file.read_at(&mut buffer, offset) {
            Ok(read) => {
                buffer.truncate(read);
                (STATUS_SUCCESS, read as u32)
            }
            Err(_) => {
                buffer.clear();
                (STATUS_FAILURE, 0)
            }
        };
        // `file` (and the handle it owns) closes here: this operation already
        // completed synchronously, so nothing keeps it open any longer,
        // mirroring IOCP's file read closing its handle once drained.
        let id = self.allocate_operation_id();
        self.completed_operations.insert(
            id,
            OperationCompletion {
                id,
                key,
                kind: OperationKind::FileRead,
                bytes_transferred,
                status,
                buffer,
            },
        );
        self.immediately_completed.push(id);
        Ok(id)
    }

    pub fn submit_file_write(
        &mut self,
        handle: OwnedFd,
        key: usize,
        buffer: Vec<u8>,
        offset: u64,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::from(handle);
        let (status, bytes_transferred) = match file.write_at(&buffer, offset) {
            Ok(written) => (STATUS_SUCCESS, written as u32),
            Err(_) => (STATUS_FAILURE, 0),
        };
        let id = self.allocate_operation_id();
        self.completed_operations.insert(
            id,
            OperationCompletion {
                id,
                key,
                kind: OperationKind::FileWrite,
                bytes_transferred,
                status,
                buffer,
            },
        );
        self.immediately_completed.push(id);
        Ok(id)
    }

    pub fn submit_socket_receive(
        &mut self,
        socket: OwnedFd,
        key: usize,
        length: u32,
    ) -> Result<u64, PostError> {
        let fd = socket.as_raw_fd();
        self.submit_receive_inner(OperationOwner::Owned(socket), fd, key, length, true)
    }

    /// Submits a receive on a descriptor already associated with this reactor.
    ///
    /// # Safety
    ///
    /// `socket` must be the associated descriptor or a duplicate of it, and
    /// must remain open until this operation's completion is taken or
    /// cancelled.
    pub unsafe fn submit_associated_socket_receive(
        &mut self,
        socket: RawDescriptor,
        key: usize,
        length: u32,
        inline_completions: bool,
    ) -> Result<u64, PostError> {
        // epoll has no analog of a provider that can refuse an inline result
        // (unlike IOCP's FILE_SKIP_COMPLETION_PORT_ON_SUCCESS negotiation):
        // every eager attempt below is unconditionally safe, so this flag
        // does not change behavior. It stays in the signature purely so
        // sako-net can call both reactors through one shared call site.
        let _ = inline_completions;
        self.submit_receive_inner(OperationOwner::Borrowed(socket), socket, key, length, false)
    }

    fn submit_receive_inner(
        &mut self,
        owner: OperationOwner,
        fd: RawFd,
        key: usize,
        length: u32,
        register: bool,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        if register {
            self.epoll_add(fd, key).map_err(PostError::System)?;
        }
        // A borrowed descriptor's completion can be filed the moment the
        // syscall resolves: the caller keeps the real ownership, so nothing
        // closes early. An owned descriptor always waits for a real
        // readiness event instead, mirroring IOCP's inline-completion guard
        // for OperationOwner::Handle/Socket: an owned descriptor's completion
        // must not be filed anywhere but through the normal drain path,
        // where the caller has already given up on tracking it separately.
        if matches!(owner, OperationOwner::Borrowed(_)) {
            let mut buffer = self.acquire_buffer(length as usize);
            match read_raw(fd, &mut buffer[..length as usize]) {
                Ok(bytes_read) => {
                    buffer.truncate(bytes_read);
                    let id = self.allocate_operation_id();
                    self.counters.receives_inline += 1;
                    self.completed_operations.insert(
                        id,
                        OperationCompletion {
                            id,
                            key,
                            kind: OperationKind::SocketReceive,
                            bytes_transferred: bytes_read as u32,
                            status: STATUS_SUCCESS,
                            buffer,
                        },
                    );
                    self.immediately_completed.push(id);
                    return Ok(id);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.recycle_buffer(buffer);
                }
                Err(error) => return Err(PostError::System(error)),
            }
        }
        self.counters.receives_pending += 1;
        let id = self.allocate_operation_id();
        let Some(registration) = self.registrations.get_mut(&fd) else {
            return Err(PostError::System(io::Error::new(
                io::ErrorKind::NotFound,
                "descriptor is not associated with this reactor",
            )));
        };
        registration.key = key;
        registration.read = Some(PendingRead {
            id,
            length: length as usize,
            owner,
        });
        self.set_interest(fd, libc::EPOLLIN as u32, true)
            .map_err(PostError::System)?;
        self.operation_locations.insert(id, (fd, true));
        Ok(id)
    }

    pub fn submit_socket_send(
        &mut self,
        socket: OwnedFd,
        key: usize,
        buffer: Vec<u8>,
    ) -> Result<u64, PostError> {
        let fd = socket.as_raw_fd();
        self.submit_send_inner(OperationOwner::Owned(socket), fd, key, buffer, true)
    }

    /// Submits a send on a descriptor already associated with this reactor.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::submit_associated_socket_receive`].
    pub unsafe fn submit_associated_socket_send(
        &mut self,
        socket: RawDescriptor,
        key: usize,
        buffer: Vec<u8>,
        inline_completions: bool,
    ) -> Result<u64, PostError> {
        let _ = inline_completions;
        self.submit_send_inner(OperationOwner::Borrowed(socket), socket, key, buffer, false)
    }

    fn submit_send_inner(
        &mut self,
        owner: OperationOwner,
        fd: RawFd,
        key: usize,
        buffer: Vec<u8>,
        register: bool,
    ) -> Result<u64, PostError> {
        self.reserve_operation()?;
        if register {
            self.epoll_add(fd, key).map_err(PostError::System)?;
        }
        if matches!(owner, OperationOwner::Borrowed(_)) {
            match write_raw(fd, &buffer) {
                Ok(bytes_written) => {
                    let id = self.allocate_operation_id();
                    self.counters.sends_inline += 1;
                    self.completed_operations.insert(
                        id,
                        OperationCompletion {
                            id,
                            key,
                            kind: OperationKind::SocketSend,
                            bytes_transferred: bytes_written as u32,
                            status: STATUS_SUCCESS,
                            buffer,
                        },
                    );
                    self.immediately_completed.push(id);
                    return Ok(id);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(PostError::System(error)),
            }
        }
        self.counters.sends_pending += 1;
        let id = self.allocate_operation_id();
        let Some(registration) = self.registrations.get_mut(&fd) else {
            return Err(PostError::System(io::Error::new(
                io::ErrorKind::NotFound,
                "descriptor is not associated with this reactor",
            )));
        };
        registration.key = key;
        registration.write = Some(PendingWrite { id, buffer, owner });
        self.set_interest(fd, libc::EPOLLOUT as u32, true)
            .map_err(PostError::System)?;
        self.operation_locations.insert(id, (fd, false));
        Ok(id)
    }

    /// Cancellation is entirely local bookkeeping: unlike IOCP's kernel-owned
    /// pending operation, nothing here is tracked by the kernel beyond plain
    /// epoll interest, so cancelling synthesizes the completion immediately
    /// instead of waiting for a later drain to notice it.
    pub fn cancel_operation(&mut self, id: u64) -> io::Result<bool> {
        let Some(&(fd, is_read)) = self.operation_locations.get(&id) else {
            return Ok(false);
        };
        let key;
        let mut taken_read = None;
        let mut taken_write = None;
        {
            let Some(registration) = self.registrations.get_mut(&fd) else {
                return Ok(false);
            };
            key = registration.key;
            if is_read {
                taken_read = registration.read.take();
                if taken_read.is_none() {
                    return Ok(false);
                }
            } else {
                taken_write = registration.write.take();
                if taken_write.is_none() {
                    return Ok(false);
                }
            }
        }
        // Clear epoll interest while `fd` is still guaranteed open: an owned
        // descriptor closes as soon as the taken operation drops below, and
        // epoll_ctl on an already-closed descriptor would fail.
        let clear_result = self.set_interest(
            fd,
            if is_read { libc::EPOLLIN } else { libc::EPOLLOUT } as u32,
            false,
        );
        self.operation_locations.remove(&id);
        let buffer = taken_write.take().map_or_else(Vec::new, |pending| pending.buffer);
        // `taken_read`'s owner (if any) drops here, closing an owned
        // descriptor only after epoll interest for it has already been
        // cleared above.
        drop(taken_read);
        self.completed_operations.insert(
            id,
            OperationCompletion {
                id,
                key,
                kind: if is_read {
                    OperationKind::SocketReceive
                } else {
                    OperationKind::SocketSend
                },
                bytes_transferred: 0,
                status: STATUS_CANCELLED,
                buffer,
            },
        );
        self.immediately_completed.push(id);
        clear_result?;
        Ok(true)
    }

    pub fn take_operation_completion(&mut self, id: u64) -> Option<OperationCompletion> {
        self.completed_operations.remove(&id)
    }

    fn resolve_pending_read(&mut self, fd: RawFd) -> Option<u64> {
        let key;
        let pending;
        {
            let registration = self.registrations.get_mut(&fd)?;
            pending = registration.read.take()?;
            key = registration.key;
        }
        let mut buffer = self.acquire_buffer(pending.length);
        let (status, bytes_transferred) = match read_raw(fd, &mut buffer[..pending.length]) {
            Ok(read) => {
                buffer.truncate(read);
                (STATUS_SUCCESS, read as u32)
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // A spurious level-triggered wakeup: put the operation back
                // and wait for the next real readiness event.
                if let Some(registration) = self.registrations.get_mut(&fd) {
                    registration.read = Some(pending);
                }
                self.recycle_buffer(buffer);
                return None;
            }
            Err(_) => {
                buffer.clear();
                (STATUS_FAILURE, 0)
            }
        };
        let id = pending.id;
        let _ = self.set_interest(fd, libc::EPOLLIN as u32, false);
        self.operation_locations.remove(&id);
        self.completed_operations.insert(
            id,
            OperationCompletion {
                id,
                key,
                kind: OperationKind::SocketReceive,
                bytes_transferred,
                status,
                buffer,
            },
        );
        Some(id)
    }

    fn resolve_pending_write(&mut self, fd: RawFd) -> Option<u64> {
        let key;
        let pending;
        {
            let registration = self.registrations.get_mut(&fd)?;
            pending = registration.write.take()?;
            key = registration.key;
        }
        let (status, bytes_transferred, buffer) = match write_raw(fd, &pending.buffer) {
            Ok(written) => (STATUS_SUCCESS, written as u32, pending.buffer),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if let Some(registration) = self.registrations.get_mut(&fd) {
                    registration.write = Some(pending);
                }
                return None;
            }
            Err(_) => (STATUS_FAILURE, 0, pending.buffer),
        };
        let id = pending.id;
        let _ = self.set_interest(fd, libc::EPOLLOUT as u32, false);
        self.operation_locations.remove(&id);
        self.completed_operations.insert(
            id,
            OperationCompletion {
                id,
                key,
                kind: OperationKind::SocketSend,
                bytes_transferred,
                status,
                buffer,
            },
        );
        Some(id)
    }

    /// Waits up to `timeout` for completions and files each one against its
    /// pending operation. Returns the number of entries applied.
    ///
    /// `completed` collects the operation identifiers that finished, which
    /// lets callers dispatch without scanning their own connection tables.
    pub fn poll_operations(
        &mut self,
        timeout: Duration,
        maximum: usize,
        completed: &mut Vec<u64>,
    ) -> io::Result<usize> {
        // Operations resolved synchronously (an eager read/write, or a
        // cancellation) are already sitting in `completed_operations`;
        // reporting them first and then polling without blocking keeps a
        // caller that asked to wait from sleeping on results it could
        // already act on. Mirrors IOCP's `inline_completed` handling.
        let immediate = self.immediately_completed.len();
        if immediate != 0 {
            completed.append(&mut self.immediately_completed);
            let drained = self.drain(Duration::ZERO, maximum, None, Some(completed))?;
            return Ok(immediate + drained);
        }
        self.drain(timeout, maximum, None, Some(completed))
    }

    fn signal_wake(&self) -> io::Result<()> {
        let value: u64 = 1;
        // SAFETY: wake_fd is a live eventfd owned by this reactor.
        let result = unsafe {
            libc::write(
                self.wake_fd.as_raw_fd(),
                (&raw const value).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            // An already-saturated eventfd counter still wakes a blocked
            // wait; nothing was lost by not incrementing it further.
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
        }
        Ok(())
    }

    fn drain_wake_fd(&self) {
        let mut value: u64 = 0;
        // SAFETY: wake_fd is a live eventfd owned by this reactor. A
        // WouldBlock/EAGAIN result just means nothing was pending, which is
        // fine to ignore here.
        unsafe {
            libc::read(
                self.wake_fd.as_raw_fd(),
                (&raw mut value).cast(),
                std::mem::size_of::<u64>(),
            )
        };
    }

    fn drain(
        &mut self,
        timeout: Duration,
        maximum: usize,
        mut reported: Option<&mut Vec<Completion>>,
        mut completed: Option<&mut Vec<u64>>,
    ) -> io::Result<usize> {
        // Completions queued by `post` belong only to the generic `poll`
        // trait method (used by tests); `poll_operations`, the production
        // path, never posts manually and always goes straight to epoll_wait.
        if let Some(reported) = reported.as_deref_mut() {
            let mut drained = 0;
            while drained < maximum {
                let Some(completion) = self.manual_completions.pop_front() else {
                    break;
                };
                self.pending_posts = self.pending_posts.saturating_sub(1);
                reported.push(completion);
                drained += 1;
            }
            if drained != 0 {
                return Ok(drained);
            }
        }

        let count = maximum.min(self.events.capacity()).min(i32::MAX as usize);
        if count == 0 {
            return Ok(0);
        }
        let milliseconds = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let mut events = std::mem::take(&mut self.events);
        events.resize(
            count,
            libc::epoll_event {
                events: 0,
                u64: 0,
            },
        );
        // SAFETY: events has writable storage for `count` entries and the
        // epoll instance remains live for this call.
        let ready = unsafe {
            libc::epoll_wait(
                self.epoll_fd.as_raw_fd(),
                events.as_mut_ptr(),
                count as i32,
                milliseconds,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            events.clear();
            self.events = events;
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(error);
        }
        let mut applied = 0;
        for event in &events[..ready as usize] {
            if event.u64 == WAKE_SENTINEL {
                self.drain_wake_fd();
                continue;
            }
            let fd = event.u64 as RawFd;
            let readable = event.events & (libc::EPOLLIN | libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0;
            let writable = event.events & (libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR) as u32 != 0;
            if readable {
                if let Some(id) = self.resolve_pending_read(fd) {
                    if let Some(completed) = completed.as_deref_mut() {
                        completed.push(id);
                    }
                    applied += 1;
                }
            }
            if writable {
                if let Some(id) = self.resolve_pending_write(fd) {
                    if let Some(completed) = completed.as_deref_mut() {
                        completed.push(id);
                    }
                    applied += 1;
                }
            }
        }
        events.clear();
        self.events = events;
        Ok(applied)
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
        self.manual_completions.push_back(Completion {
            key,
            bytes_transferred,
            overlapped,
            status: STATUS_SUCCESS,
        });
        self.pending_posts += 1;
        let _ = self.signal_wake();
        Ok(())
    }
}

impl PlatformRuntime for EpollReactor {
    fn poll(&mut self, timeout: Duration, maximum: usize) -> io::Result<Vec<Completion>> {
        let mut reported = Vec::new();
        self.drain(timeout, maximum, Some(&mut reported), None)?;
        Ok(reported)
    }

    fn wake(&mut self) -> Result<(), PostError> {
        self.post(0, 0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::time::Instant;

    #[test]
    fn completion_posts_are_bounded_and_drained() {
        let mut reactor = EpollReactor::new(2).unwrap();
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
        assert!(EpollReactor::new(0).is_err());
    }

    #[test]
    fn synchronous_file_reads_are_owned_until_consumed() {
        let path = std::env::temp_dir().join(format!(
            "sako-epoll-file-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"overlapped-file").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut reactor = EpollReactor::new(2).unwrap();
        let operation = reactor.submit_file_read(file.into(), 41, 10, 0).unwrap();
        let completed = reactor.take_operation_completion(operation).unwrap();
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
        let mut reactor = EpollReactor::new(1).unwrap();
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
